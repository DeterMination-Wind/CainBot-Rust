use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::TimeZone;
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::config::CodexBridgeConfig;
use crate::logger::Logger;
use crate::napcat_client::NapCatClient;

#[derive(Debug, Clone)]
pub struct CodexBridgeInfo {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub base_url: String,
    pub send_group_file_url: String,
    pub send_group_file_to_folder_url: String,
    pub send_group_message_url: String,
    pub send_private_message_url: String,
    pub read_group_messages_url: String,
    pub read_private_messages_url: String,
    pub read_file_url: String,
    pub health_url: String,
    pub token_required: bool,
}

pub struct CodexBridgeServer {
    config: CodexBridgeConfig,
    napcat_client: NapCatClient,
    logger: Logger,
    stop_flag: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl CodexBridgeServer {
    pub fn new(config: CodexBridgeConfig, napcat_client: NapCatClient, logger: Logger) -> Self {
        Self {
            config,
            napcat_client,
            logger,
            stop_flag: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    pub async fn start(&mut self) -> Result<Option<CodexBridgeInfo>> {
        if !self.config.enabled {
            return Ok(None);
        }
        if self.worker.is_some() {
            return Ok(Some(self.get_info()));
        }

        let server = Server::http(format!("{}:{}", normalize_host(&self.config.host), self.config.port))
            .map_err(|error| anyhow::anyhow!("启动 Codex bridge HTTP 服务失败: {error}"))?;
        let stop_flag = self.stop_flag.clone();
        let logger = self.logger.clone();
        let napcat_client = self.napcat_client.clone();
        let config = self.config.clone();
        let runtime_handle = tokio::runtime::Handle::current();

        let worker = thread::Builder::new()
            .name("cain-codex-bridge".to_string())
            .spawn(move || {
                while !stop_flag.load(Ordering::SeqCst) {
                    match server.recv_timeout(Duration::from_millis(250)) {
                        Ok(Some(request)) => {
                            if let Err(error) = handle_request(&runtime_handle, &config, &napcat_client, request) {
                                let runtime = runtime_handle.clone();
                                let logger = logger.clone();
                                runtime.block_on(async move {
                                    logger.warn(format!("Codex 文件桥请求处理失败：{error:#}")).await;
                                });
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let runtime = runtime_handle.clone();
                            let logger = logger.clone();
                            runtime.block_on(async move {
                                logger.warn(format!("Codex 文件桥监听失败：{error}")).await;
                            });
                            break;
                        }
                    }
                }
            })
            .context("启动 Codex bridge 工作线程失败")?;

        self.worker = Some(worker);
        self.logger
            .info(format!("Codex 文件桥已启动：{}", self.get_info().base_url))
            .await;
        Ok(Some(self.get_info()))
    }

    pub async fn stop(&mut self) -> Result<()> {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        Ok(())
    }

    pub fn get_info(&self) -> CodexBridgeInfo {
        let host = normalize_host(&self.config.host);
        let base_url = format!("http://{}:{}", host, self.config.port);
        CodexBridgeInfo {
            enabled: self.config.enabled,
            host: host.clone(),
            port: self.config.port,
            base_url: base_url.clone(),
            send_group_file_url: format!("{base_url}/codex/send-group-file"),
            send_group_file_to_folder_url: format!("{base_url}/codex/send-group-file-to-folder"),
            send_group_message_url: format!("{base_url}/codex/send-group-message"),
            send_private_message_url: format!("{base_url}/codex/send-private-message"),
            read_group_messages_url: format!("{base_url}/codex/read-group-messages"),
            read_private_messages_url: format!("{base_url}/codex/read-private-messages"),
            read_file_url: format!("{base_url}/codex/read-file"),
            health_url: format!("{base_url}/codex/health"),
            token_required: !self.config.token.trim().is_empty(),
        }
    }
}

fn handle_request(
    runtime_handle: &tokio::runtime::Handle,
    config: &CodexBridgeConfig,
    napcat_client: &NapCatClient,
    mut request: Request,
) -> Result<()> {
    if request.method() == &Method::Get && request.url() == "/codex/health" {
        let info = build_info(config);
        return respond_json(
            request,
            StatusCode(200),
            json!({
                "ok": true,
                "service": "cain-codex-bridge",
                "enabled": info.enabled,
                "host": info.host,
                "port": info.port,
                "baseUrl": info.base_url,
                "sendGroupFileUrl": info.send_group_file_url,
                "sendGroupFileToFolderUrl": info.send_group_file_to_folder_url,
                "sendGroupMessageUrl": info.send_group_message_url,
                "sendPrivateMessageUrl": info.send_private_message_url,
                "readGroupMessagesUrl": info.read_group_messages_url,
                "readPrivateMessagesUrl": info.read_private_messages_url,
                "readFileUrl": info.read_file_url,
                "healthUrl": info.health_url,
                "tokenRequired": info.token_required
            }),
        );
    }

    validate_local_request(config, &request)?;
    let payload = read_json_payload(&mut request)?;
    let path = request.url().to_string();

    match (request.method(), path.as_str()) {
        (&Method::Post, "/codex/send-group-message") => {
            let group_id = get_required_string(&payload, "groupId")?;
            let message = build_outgoing_message(&payload)?;
            let result = runtime_handle.block_on(async { napcat_client.send_group_message(&group_id, message).await })?;
            respond_json(
                request,
                StatusCode(200),
                json!({
                    "ok": true,
                    "target": format!("group:{group_id}"),
                    "result": result
                }),
            )
        }
        (&Method::Post, "/codex/send-private-message") => {
            let user_id = get_required_string(&payload, "userId")?;
            let message = build_outgoing_message(&payload)?;
            let result = runtime_handle.block_on(async { napcat_client.send_private_message(&user_id, message).await })?;
            respond_json(
                request,
                StatusCode(200),
                json!({
                    "ok": true,
                    "target": format!("private:{user_id}"),
                    "result": result
                }),
            )
        }
        (&Method::Post, "/codex/send-group-file") | (&Method::Post, "/codex/send-group-file-to-folder") => {
            let group_id = get_required_string(&payload, "groupId")?;
            let file_path = get_required_string_any(&payload, &["filePath", "file"])?;
            let file_name = payload.get("fileName").and_then(Value::as_str);
            let folder_name = payload
                .get("folderName")
                .or_else(|| payload.get("folder"))
                .or_else(|| payload.get("folderPath"))
                .and_then(Value::as_str);
            if path.ends_with("to-folder") && folder_name.unwrap_or_default().trim().is_empty() {
                bail!("folderName/folderPath 不能为空");
            }
            let notify_text = payload.get("notifyText").and_then(Value::as_str);
            let result = runtime_handle.block_on(async {
                napcat_client
                    .send_local_file_to_group(&group_id, &file_path, file_name, folder_name, notify_text)
                    .await
            })?;
            respond_json(
                request,
                StatusCode(200),
                json!({
                    "ok": true,
                    "groupId": group_id,
                    "result": result
                }),
            )
        }
        (&Method::Post, "/codex/read-group-messages") => {
            let group_id = get_required_string(&payload, "groupId")?;
            let count = payload.get("count").and_then(Value::as_u64).unwrap_or(20).clamp(1, 200) as usize;
            let history = runtime_handle.block_on(async { napcat_client.get_group_message_history(&group_id, count).await })?;
            let messages = normalize_history_messages(&history, count);
            respond_json(
                request,
                StatusCode(200),
                json!({
                    "ok": true,
                    "target": format!("group:{group_id}"),
                    "requestedCount": count,
                    "returnedCount": messages.len(),
                    "messages": messages
                }),
            )
        }
        (&Method::Post, "/codex/read-private-messages") => {
            let user_id = get_required_string(&payload, "userId")?;
            let count = payload.get("count").and_then(Value::as_u64).unwrap_or(20).clamp(1, 200) as usize;
            let history = runtime_handle.block_on(async { napcat_client.get_friend_message_history(&user_id, count).await })?;
            let messages = normalize_history_messages(&history, count);
            respond_json(
                request,
                StatusCode(200),
                json!({
                    "ok": true,
                    "target": format!("private:{user_id}"),
                    "requestedCount": count,
                    "returnedCount": messages.len(),
                    "messages": messages
                }),
            )
        }
        (&Method::Post, "/codex/read-file") => {
            let file_path = get_required_string_any(&payload, &["path", "filePath"])?;
            let start_line = payload.get("startLine").or_else(|| payload.get("start_line")).and_then(Value::as_u64).unwrap_or(1) as usize;
            let end_line = payload.get("endLine").or_else(|| payload.get("end_line")).and_then(Value::as_u64);
            let max_chars = payload.get("maxChars").or_else(|| payload.get("max_chars")).and_then(Value::as_u64).unwrap_or(12_000) as usize;
            let result = read_text_file_window(&file_path, start_line, end_line.map(|item| item as usize), max_chars)?;
            let mut response = result;
            response["ok"] = Value::Bool(true);
            respond_json(request, StatusCode(200), response)
        }
        _ => respond_json(request, StatusCode(404), json!({ "ok": false, "error": "未找到接口" })),
    }
}

fn validate_local_request(config: &CodexBridgeConfig, request: &Request) -> Result<()> {
    let remote = request.remote_addr();
    let is_loopback = remote.map(|addr| addr.ip().is_loopback()).unwrap_or(false);
    if !is_loopback {
        bail!(
            "仅允许本机访问，当前来源：{}",
            remote
                .map(|addr| addr.ip().to_string())
                .unwrap_or_else(|| "(unknown)".to_string())
        );
    }
    let required_token = config.token.trim();
    if required_token.is_empty() {
        return Ok(());
    }
    let auth_header = request
        .headers()
        .iter()
        .find(|header| header.field.as_str().as_str().eq_ignore_ascii_case("authorization"))
        .map(|header| header.value.as_str().trim())
        .unwrap_or_default();
    let provided = auth_header
        .strip_prefix("Bearer ")
        .or_else(|| auth_header.strip_prefix("bearer "))
        .unwrap_or_default()
        .trim();
    if provided != required_token {
        bail!("Bearer Token 无效");
    }
    Ok(())
}

fn read_json_payload(request: &mut Request) -> Result<Value> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body).context("读取请求体失败")?;
    if body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&body).context("请求体不是有效 JSON")
}

fn build_outgoing_message(payload: &Value) -> Result<Value> {
    let reply_to_message_id = payload
        .get("replyToMessageId")
        .or_else(|| payload.get("replyTo"))
        .or_else(|| payload.get("reply_to_message_id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let text = payload
        .get("text")
        .or_else(|| payload.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .replace("\\r\\n", "\n")
        .replace("\\n", "\n")
        .trim()
        .to_string();
    let at_user_ids = extract_string_list(
        payload
            .get("atUserIds")
            .or_else(|| payload.get("at_users"))
            .or_else(|| payload.get("mentions"))
            .or_else(|| payload.get("atUserId")),
    );

    let mut segments = Vec::new();
    if !reply_to_message_id.is_empty() {
        segments.push(json!({
            "type": "reply",
            "data": { "id": reply_to_message_id }
        }));
    }
    for (index, user_id) in at_user_ids.iter().enumerate() {
        segments.push(json!({
            "type": "at",
            "data": { "qq": user_id }
        }));
        if index < at_user_ids.len() - 1 || !text.is_empty() {
            segments.push(json!({
                "type": "text",
                "data": { "text": " " }
            }));
        }
    }
    if !text.is_empty() {
        segments.push(json!({
            "type": "text",
            "data": { "text": text }
        }));
    }
    if segments.is_empty() {
        bail!("消息内容不能为空；请提供 text/message，或提供 atUserIds");
    }
    if segments.len() == 1 && segments[0].get("type").and_then(Value::as_str) == Some("text") {
        return Ok(Value::String(
            segments[0]
                .get("data")
                .and_then(|data| data.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ));
    }
    Ok(Value::Array(segments))
}

fn normalize_history_messages(payload: &Value, count: usize) -> Vec<Value> {
    let items = if let Some(items) = payload.as_array() {
        items.clone()
    } else if let Some(items) = payload.get("messages").and_then(Value::as_array) {
        items.clone()
    } else if let Some(items) = payload.get("data").and_then(|data| data.get("messages")).and_then(Value::as_array) {
        items.clone()
    } else {
        Vec::new()
    };

    let mut normalized = items
        .into_iter()
        .enumerate()
        .map(|(index, message)| normalize_message_record(&message, index + 1))
        .collect::<Vec<_>>();
    if normalized.len() > count {
        normalized = normalized[normalized.len() - count..].to_vec();
    }
    normalized
}

fn normalize_message_record(message: &Value, index: usize) -> Value {
    let at_user_ids = message
        .get("message")
        .and_then(Value::as_array)
        .map(|segments| {
            segments
                .iter()
                .filter(|segment| segment.get("type").and_then(Value::as_str) == Some("at"))
                .filter_map(|segment| segment.get("data").and_then(|data| data.get("qq")))
                .map(value_to_string)
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    json!({
        "index": index,
        "time": format_epoch_time(message.get("time").and_then(Value::as_i64)),
        "userId": message.get("user_id").map(value_to_string).unwrap_or_default(),
        "sender": message
            .get("sender")
            .and_then(|sender| sender.get("card").or_else(|| sender.get("nickname")))
            .and_then(Value::as_str)
            .unwrap_or_default(),
        "messageId": message.get("message_id").map(value_to_string).unwrap_or_default(),
        "messageSeq": message.get("message_seq").map(value_to_string).unwrap_or_default(),
        "realSeq": message.get("real_seq").map(value_to_string).unwrap_or_default(),
        "text": crate::event_utils::plain_text_from_message(
            message.get("message").unwrap_or(&Value::Null),
            message.get("raw_message").and_then(Value::as_str)
        ),
        "summary": crate::event_utils::plain_text_from_message(
            message.get("message").unwrap_or(&Value::Null),
            message.get("raw_message").and_then(Value::as_str)
        ),
        "atUserIds": at_user_ids,
        "imageCount": count_segments(message.get("message").unwrap_or(&Value::Null), "image"),
        "fileCount": count_segments(message.get("message").unwrap_or(&Value::Null), "file"),
        "faceCount": count_segments(message.get("message").unwrap_or(&Value::Null), "face"),
        "replyCount": count_segments(message.get("message").unwrap_or(&Value::Null), "reply")
    })
}

fn count_segments(message: &Value, segment_type: &str) -> usize {
    message
        .as_array()
        .map(|segments| {
            segments
                .iter()
                .filter(|segment| segment.get("type").and_then(Value::as_str) == Some(segment_type))
                .count()
        })
        .unwrap_or_default()
}

fn read_text_file_window(file_path: &str, start_line: usize, end_line: Option<usize>, max_chars: usize) -> Result<Value> {
    let absolute_path = if Path::new(file_path).is_absolute() {
        Path::new(file_path).to_path_buf()
    } else {
        std::env::current_dir()?.join(file_path)
    };
    let content = std::fs::read_to_string(&absolute_path)
        .with_context(|| format!("读取文件失败: {}", absolute_path.display()))?;
    let lines = content.lines().collect::<Vec<_>>();
    let total_lines = lines.len().max(1);
    let start = start_line.clamp(1, total_lines);
    let end = end_line.unwrap_or_else(|| (start + 199).min(total_lines)).clamp(start, total_lines);
    let mut text = lines[start - 1..end]
        .iter()
        .enumerate()
        .map(|(offset, line)| format!("{}: {}", start + offset, line))
        .collect::<Vec<_>>()
        .join("\n");
    let truncated = text.len() > max_chars;
    if truncated {
        text = format!("{}\n...[truncated]", &text[..max_chars]);
    }
    Ok(json!({
        "path": absolute_path.display().to_string(),
        "totalLines": total_lines,
        "startLine": start,
        "endLine": end,
        "truncated": truncated,
        "text": text
    }))
}

fn respond_json(request: Request, status: StatusCode, payload: Value) -> Result<()> {
    let body = serde_json::to_string_pretty(&payload)?;
    let response = Response::from_string(body)
        .with_status_code(status)
        .with_header(
            Header::from_bytes("Content-Type".as_bytes(), "application/json; charset=utf-8".as_bytes())
                .expect("valid content-type header"),
        );
    request.respond(response).context("发送 HTTP 响应失败")
}

fn get_required_string(payload: &Value, key: &str) -> Result<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow::anyhow!("{key} 不能为空"))
}

fn get_required_string_any(payload: &Value, keys: &[&str]) -> Result<String> {
    for key in keys {
        if let Some(value) = payload.get(*key).and_then(Value::as_str).map(str::trim).filter(|item| !item.is_empty()) {
            return Ok(value.to_string());
        }
    }
    bail!("{} 不能为空", keys.join("/"))
}

fn extract_string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToString::to_string)
            .collect(),
        Some(Value::String(text)) => {
            let normalized = text.trim();
            if normalized.is_empty() {
                Vec::new()
            } else {
                vec![normalized.to_string()]
            }
        }
        _ => Vec::new(),
    }
}

fn format_epoch_time(seconds: Option<i64>) -> Option<String> {
    seconds
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| chrono::Utc.timestamp_opt(seconds, 0).single())
        .map(|value| value.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

fn normalize_host(host: &str) -> String {
    let normalized = host.trim();
    if normalized.is_empty() {
        "127.0.0.1".to_string()
    } else {
        normalized.to_string()
    }
}

fn build_info(config: &CodexBridgeConfig) -> CodexBridgeInfo {
    let host = normalize_host(&config.host);
    let base_url = format!("http://{}:{}", host, config.port);
    CodexBridgeInfo {
        enabled: config.enabled,
        host: host.clone(),
        port: config.port,
        base_url: base_url.clone(),
        send_group_file_url: format!("{base_url}/codex/send-group-file"),
        send_group_file_to_folder_url: format!("{base_url}/codex/send-group-file-to-folder"),
        send_group_message_url: format!("{base_url}/codex/send-group-message"),
        send_private_message_url: format!("{base_url}/codex/send-private-message"),
        read_group_messages_url: format!("{base_url}/codex/read-group-messages"),
        read_private_messages_url: format!("{base_url}/codex/read-private-messages"),
        read_file_url: format!("{base_url}/codex/read-file"),
        health_url: format!("{base_url}/codex/health"),
        token_required: !config.token.trim().is_empty(),
    }
}
