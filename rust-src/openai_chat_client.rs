use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::logger::Logger;
use crate::utils::{join_url, sleep_ms};

const TRANSPORT_SUPPRESS_MS: u64 = 30_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Value,
}

#[derive(Debug, Clone)]
pub struct OpenAiChatClientConfig {
    pub enabled: bool,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f64,
    pub request_timeout_ms: u64,
    pub retry_attempts: usize,
    pub retry_delay_ms: u64,
    pub failure_cooldown_ms: u64,
    pub failure_cooldown_threshold: usize,
}

#[derive(Debug, Clone, Default)]
pub struct CompleteOptions {
    pub model: Option<String>,
    pub temperature: Option<f64>,
}

#[derive(Clone)]
pub struct OpenAiChatClient {
    config: OpenAiChatClientConfig,
    logger: Logger,
    client: Client,
    cooldown_until: std::sync::Arc<tokio::sync::Mutex<Option<Instant>>>,
    cooldown_reason: std::sync::Arc<tokio::sync::Mutex<String>>,
    retryable_failure_streak: std::sync::Arc<tokio::sync::Mutex<usize>>,
    transport_suppressed_until: std::sync::Arc<tokio::sync::Mutex<HashMap<String, Instant>>>,
}

impl OpenAiChatClient {
    pub fn new(config: OpenAiChatClientConfig, logger: Logger) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .build()
            .context("创建聊天客户端失败")?;
        Ok(Self {
            config,
            logger,
            client,
            cooldown_until: Default::default(),
            cooldown_reason: Default::default(),
            retryable_failure_streak: Default::default(),
            transport_suppressed_until: Default::default(),
        })
    }

    // 这里故意保留原 MJS 的双传输链路，并继续只走流式输出。
    // 用户这边的 XEM8K5/cc-switch 代理长期只能稳定处理 SSE；
    // 另外当低信息/幻觉检查把主模型打回时，同一轮还会继续复用这个客户端。
    // 所以传输优先级、短时熔断和冷却策略必须尽量对齐原版，不能为了“简化”把兼容层删掉。
    pub async fn complete(
        &self,
        messages: &[ChatMessage],
        options: CompleteOptions,
    ) -> Result<String> {
        self.validate()?;
        self.ensure_not_in_cooldown().await?;
        let transports = self.available_transports().await;
        let proxy_managed_failover = is_cc_switch_proxy(&self.config.base_url);
        let mut last_error = None;

        for (index, transport) in transports.iter().enumerate() {
            let result = if transport == "responses" {
                self.complete_via_responses(messages, &options).await
            } else {
                self.complete_via_chat(messages, &options).await
            };

            match result {
                Ok(text) => {
                    *self.retryable_failure_streak.lock().await = 0;
                    self.transport_suppressed_until
                        .lock()
                        .await
                        .remove(transport.as_str());
                    return Ok(text);
                }
                Err(error) => {
                    self.logger
                        .warn(format!("聊天接口 {transport} 失败：{error:#}"))
                        .await;
                    if proxy_managed_failover
                        && transport == "chat"
                        && should_suppress_transport(transport, &error)
                    {
                        self.suppress_transport(transport, TRANSPORT_SUPPRESS_MS)
                            .await;
                    }
                    last_error = Some(error);
                    let has_alternate_transport = index + 1 < transports.len();
                    if has_alternate_transport
                        && should_fallback_transport(last_error.as_ref().expect("last error"))
                    {
                        let next_transport = &transports[index + 1];
                        self.logger
                            .warn(format!(
                                "聊天接口 {transport} 不稳定，切换到 {next_transport}：{}",
                                last_error.as_ref().expect("last error").to_string()
                            ))
                            .await;
                        continue;
                    }
                    break;
                }
            }
        }

        let Some(error) = last_error else {
            bail!("聊天接口调用失败");
        };

        if is_retryable_error(&error) || has_retryable_http_status(&error) {
            let mut streak = self.retryable_failure_streak.lock().await;
            *streak += 1;
            if proxy_managed_failover {
                self.logger
                    .warn(format!(
                        "聊天接口失败，CC Switch 代理已接管整流，跳过本地冷却：{error:#}"
                    ))
                    .await;
            } else if *streak >= self.config.failure_cooldown_threshold.max(1) {
                *self.cooldown_until.lock().await = Some(
                    Instant::now()
                        + Duration::from_millis(self.config.failure_cooldown_ms.max(1_000)),
                );
                *self.cooldown_reason.lock().await = format!("{error:#}");
            } else {
                self.logger
                    .warn(format!(
                        "聊天接口连续失败 {}/{}, 暂不进入冷却：{error:#}",
                        *streak,
                        self.config.failure_cooldown_threshold.max(1)
                    ))
                    .await;
            }
        } else {
            *self.retryable_failure_streak.lock().await = 0;
        }
        Err(error)
    }

    fn validate(&self) -> Result<()> {
        if self.config.base_url.trim().is_empty() {
            bail!("chat.baseUrl 未配置");
        }
        if self.config.model.trim().is_empty() {
            bail!("chat.model 未配置");
        }
        Ok(())
    }

    async fn complete_via_chat(
        &self,
        messages: &[ChatMessage],
        options: &CompleteOptions,
    ) -> Result<String> {
        let model = options
            .model
            .clone()
            .unwrap_or_else(|| self.config.model.clone());
        let temperature = options.temperature.unwrap_or(self.config.temperature);
        let body = json!({
            "model": model,
            "temperature": temperature,
            "messages": messages,
            "stream": true
        });

        self.execute_retriable_request(|| async {
            let url = join_url(&self.config.base_url, "chat/completions")?;
            let mut request = self
                .client
                .post(url)
                .header("Accept", "text/event-stream")
                .json(&body);
            if !self.config.api_key.trim().is_empty() {
                request = request.bearer_auth(&self.config.api_key);
            }
            let response = request.send().await.context("请求 chat/completions 失败")?;
            let response = ensure_success_status(response, "chat/completions").await?;
            read_chat_completion_stream(response).await
        })
        .await
    }

    async fn complete_via_responses(
        &self,
        messages: &[ChatMessage],
        options: &CompleteOptions,
    ) -> Result<String> {
        let model = options
            .model
            .clone()
            .unwrap_or_else(|| self.config.model.clone());
        let temperature = options.temperature.unwrap_or(self.config.temperature);
        let body = json!({
            "model": model,
            "temperature": temperature,
            "input": build_responses_input(messages),
            "stream": true
        });

        self.execute_retriable_request(|| async {
            let url = join_url(&self.config.base_url, "responses")?;
            let mut request = self
                .client
                .post(url)
                .header("Accept", "text/event-stream")
                .json(&body);
            if !self.config.api_key.trim().is_empty() {
                request = request.bearer_auth(&self.config.api_key);
            }
            let response = request.send().await.context("请求 responses 失败")?;
            let response = ensure_success_status(response, "responses").await?;
            read_responses_stream(response).await
        })
        .await
    }

    async fn execute_retriable_request<F, Fut>(&self, run_request: F) -> Result<String>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<String>>,
    {
        let max_attempts = self.config.retry_attempts.max(1);
        for attempt in 1..=max_attempts {
            match run_request().await {
                Ok(value) => return Ok(value),
                Err(error) if attempt < max_attempts && is_retryable_error(&error) => {
                    let delay_ms = self.config.retry_delay_ms.max(200) * attempt as u64;
                    self.logger
                        .warn(format!(
                            "聊天接口请求异常，准备重试（{attempt}/{max_attempts}）：{error:#}"
                        ))
                        .await;
                    sleep_ms(delay_ms).await;
                }
                Err(error) => return Err(error),
            }
        }
        bail!("聊天接口调用失败")
    }

    async fn ensure_not_in_cooldown(&self) -> Result<()> {
        let until = *self.cooldown_until.lock().await;
        if let Some(until) = until
            && Instant::now() < until
        {
            let seconds = until
                .saturating_duration_since(Instant::now())
                .as_secs()
                .max(1);
            let reason = self.cooldown_reason.lock().await.clone();
            bail!("聊天接口暂时不可用，已进入 {seconds} 秒冷却：{reason}");
        }
        Ok(())
    }

    async fn suppress_transport(&self, transport: &str, duration_ms: u64) {
        let until = Instant::now() + Duration::from_millis(duration_ms.max(1_000));
        self.transport_suppressed_until
            .lock()
            .await
            .insert(transport.to_string(), until);
    }

    async fn available_transports(&self) -> Vec<String> {
        let preferred = preferred_transports(&self.config.base_url);
        let suppressed = self.transport_suppressed_until.lock().await.clone();
        let available = preferred
            .iter()
            .filter(|transport| {
                suppressed
                    .get((*transport).as_str())
                    .map(|until| *until <= Instant::now())
                    .unwrap_or(true)
            })
            .cloned()
            .collect::<Vec<_>>();
        if available.is_empty() {
            preferred
        } else {
            available
        }
    }
}

async fn ensure_success_status(
    response: reqwest::Response,
    transport: &str,
) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let detail = response.text().await.unwrap_or_default();
    if detail.trim().is_empty() {
        bail!("聊天接口 {transport} 返回 HTTP {status}");
    }
    bail!(
        "聊天接口 {transport} 返回 HTTP {status}: {}",
        compact_single_line(&detail, 240)
    )
}

async fn read_chat_completion_stream(response: reqwest::Response) -> Result<String> {
    read_streaming_text(
        response,
        "chat/completions",
        extract_chat_stream_delta,
        extract_assistant_text,
    )
    .await
}

async fn read_responses_stream(response: reqwest::Response) -> Result<String> {
    read_streaming_text(
        response,
        "responses",
        extract_responses_stream_delta,
        extract_responses_text,
    )
    .await
}

async fn read_streaming_text(
    response: reqwest::Response,
    transport: &str,
    delta_extractor: fn(&Value) -> Option<String>,
    final_extractor: fn(&Value) -> Option<String>,
) -> Result<String> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::<u8>::new();
    let mut accumulated = String::new();
    let mut final_text = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("读取 {transport} 流式响应失败"))?;
        buffer.extend_from_slice(&chunk);
        while let Some((index, separator_len)) = find_sse_event_boundary(&buffer) {
            let event_bytes = buffer[..index].to_vec();
            buffer.drain(..index + separator_len);
            process_sse_event(
                &event_bytes,
                transport,
                delta_extractor,
                final_extractor,
                &mut accumulated,
                &mut final_text,
            )?;
        }
    }

    if !buffer.is_empty() {
        if accumulated.is_empty()
            && let Ok(payload) = serde_json::from_slice::<Value>(&buffer)
        {
            append_stream_payload(
                &payload,
                delta_extractor,
                final_extractor,
                &mut accumulated,
                &mut final_text,
            )?;
        } else {
            process_sse_event(
                &buffer,
                transport,
                delta_extractor,
                final_extractor,
                &mut accumulated,
                &mut final_text,
            )?;
        }
    }

    let merged = if !accumulated.trim().is_empty() {
        accumulated
    } else {
        final_text
    };
    let normalized = normalize_completion_text(&merged);
    if !normalized.is_empty() {
        return Ok(normalized);
    }

    if content_type.contains("application/json") {
        bail!("聊天接口未返回可用文本");
    }
    bail!("聊天接口未返回可用文本")
}

fn process_sse_event(
    raw: &[u8],
    transport: &str,
    delta_extractor: fn(&Value) -> Option<String>,
    final_extractor: fn(&Value) -> Option<String>,
    accumulated: &mut String,
    final_text: &mut String,
) -> Result<()> {
    if raw.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(());
    }

    let text = String::from_utf8(raw.to_vec())
        .unwrap_or_else(|_| String::from_utf8_lossy(raw).to_string());
    let mut data_lines = Vec::<String>::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start().to_string());
        }
    }

    let payload_text = if data_lines.is_empty() {
        text.trim().to_string()
    } else {
        data_lines.join("\n").trim().to_string()
    };
    if payload_text.is_empty() || payload_text == "[DONE]" {
        return Ok(());
    }

    let payload = serde_json::from_str::<Value>(&payload_text)
        .with_context(|| format!("解析 {transport} 流式事件失败"))?;
    append_stream_payload(
        &payload,
        delta_extractor,
        final_extractor,
        accumulated,
        final_text,
    )
}

fn append_stream_payload(
    payload: &Value,
    delta_extractor: fn(&Value) -> Option<String>,
    final_extractor: fn(&Value) -> Option<String>,
    accumulated: &mut String,
    final_text: &mut String,
) -> Result<()> {
    if let Some(error_text) = extract_stream_error(payload) {
        bail!("聊天接口流式返回错误: {error_text}");
    }

    if let Some(delta) = delta_extractor(payload) {
        accumulated.push_str(&delta);
    }
    if let Some(text) = final_extractor(payload) {
        *final_text = text;
    }
    if let Some(response) = payload.get("response")
        && let Some(text) = final_extractor(response)
    {
        *final_text = text;
    }
    Ok(())
}

fn find_sse_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0usize;
    while index + 1 < buffer.len() {
        if buffer[index] == b'\n' && buffer[index + 1] == b'\n' {
            return Some((index, 2));
        }
        if index + 3 < buffer.len()
            && buffer[index] == b'\r'
            && buffer[index + 1] == b'\n'
            && buffer[index + 2] == b'\r'
            && buffer[index + 3] == b'\n'
        {
            return Some((index, 4));
        }
        index += 1;
    }
    None
}

fn extract_stream_error(payload: &Value) -> Option<String> {
    if let Some(error) = payload.get("error") {
        return Some(compact_single_line(&error.to_string(), 240));
    }
    if payload.get("type").and_then(Value::as_str) == Some("error") {
        return Some(compact_single_line(&payload.to_string(), 240));
    }
    None
}

fn extract_assistant_text(payload: &Value) -> Option<String> {
    serde_json::from_value::<ChatCompletionPayload>(payload.clone())
        .ok()
        .and_then(|parsed| parsed.choices.into_iter().next())
        .and_then(|choice| choice.message.content.into_text())
}

fn extract_chat_stream_delta(payload: &Value) -> Option<String> {
    payload
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|choice| {
            choice
                .get("delta")
                .and_then(|delta| delta.get("content"))
                .and_then(value_to_text_piece)
        })
}

fn extract_responses_text(payload: &Value) -> Option<String> {
    if let Ok(parsed) = serde_json::from_value::<ResponsesPayload>(payload.clone()) {
        if let Some(text) = parsed.output_text.and_then(MaybeText::into_text)
            && !text.is_empty()
        {
            return Some(text);
        }
        if let Some(output) = parsed.output {
            let text = output
                .into_iter()
                .flat_map(|item| item.content.unwrap_or_default())
                .filter_map(MaybeText::into_text)
                .collect::<String>()
                .trim()
                .to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
    }

    if let Some(text) = payload.get("output_text").and_then(value_to_text)
        && !text.is_empty()
    {
        return Some(text);
    }

    payload
        .get("output")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("content").and_then(value_to_text))
                .collect::<String>()
                .trim()
                .to_string()
        })
        .filter(|item| !item.is_empty())
}

fn extract_responses_stream_delta(payload: &Value) -> Option<String> {
    let event_type = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_type.ends_with("output_text.delta") {
        return payload.get("delta").and_then(value_to_text_piece);
    }
    if event_type.contains("delta") {
        return payload
            .get("delta")
            .and_then(value_to_text_piece)
            .or_else(|| {
                payload
                    .get("part")
                    .and_then(|part| part.get("text"))
                    .and_then(value_to_text_piece)
            });
    }
    None
}

fn value_to_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_string()),
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(|item| {
                    item.get("text")
                        .or_else(|| item.get("output_text"))
                        .or_else(|| item.get("value"))
                        .and_then(Value::as_str)
                })
                .collect::<String>()
                .trim()
                .to_string();
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn value_to_text_piece(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => (!text.is_empty()).then(|| text.to_string()),
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(|item| {
                    item.get("text")
                        .or_else(|| item.get("output_text"))
                        .or_else(|| item.get("value"))
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                        .or_else(|| {
                            item.as_str()
                                .filter(|text| !text.is_empty())
                                .map(ToString::to_string)
                        })
                })
                .collect::<String>();
            (!text.is_empty()).then_some(text)
        }
        Value::Object(map) => map
            .get("text")
            .or_else(|| map.get("output_text"))
            .or_else(|| map.get("value"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(ToString::to_string),
        _ => None,
    }
}

fn build_responses_input(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|message| {
            let text_item_type = responses_text_item_type(&message.role);
            let content = match &message.content {
                Value::String(text) => vec![json!({ "type": text_item_type, "text": text })],
                Value::Array(items) => items
                    .iter()
                    .filter_map(|item| {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            Some(json!({ "type": text_item_type, "text": text }))
                        } else if let Some(url) = item.get("image_url").and_then(|value| {
                            value.as_str().map(ToString::to_string).or_else(|| {
                                value
                                    .get("url")
                                    .and_then(Value::as_str)
                                    .map(ToString::to_string)
                            })
                        }) {
                            Some(json!({ "type": "input_image", "image_url": url }))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>(),
                other => vec![json!({ "type": text_item_type, "text": other.to_string() })],
            };
            (!content.is_empty()).then(|| {
                json!({
                    "role": normalize_message_role(&message.role),
                    "content": content
                })
            })
        })
        .collect()
}

fn responses_text_item_type(role: &str) -> &'static str {
    if normalize_message_role(role) == "assistant" {
        "output_text"
    } else {
        "input_text"
    }
}

fn normalize_message_role(role: &str) -> &str {
    match role.trim().to_ascii_lowercase().as_str() {
        "system" => "system",
        "developer" => "developer",
        "assistant" => "assistant",
        "tool" => "tool",
        _ => "user",
    }
}

fn is_cc_switch_proxy(base_url: &str) -> bool {
    let lower = base_url.to_ascii_lowercase();
    (lower.contains("127.0.0.1:15721") || lower.contains("localhost:15721"))
        && lower.contains("/v1")
}

fn preferred_transports(base_url: &str) -> Vec<String> {
    if is_cc_switch_proxy(base_url) {
        vec!["responses".to_string(), "chat".to_string()]
    } else {
        vec!["chat".to_string(), "responses".to_string()]
    }
}

fn is_retryable_error(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    text.contains("network")
        || text.contains("socket")
        || text.contains("timeout")
        || text.contains("timed out")
}

fn has_retryable_http_status(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    [
        "http 408", "http 425", "http 429", "http 500", "http 502", "http 503", "http 504",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn is_invalid_response_error(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("聊天接口未返回可用文本")
        || text.contains("聊天接口返回了空流")
        || text.contains("解析 chat/completions 流式事件失败")
        || text.contains("解析 responses 流式事件失败")
}

fn should_fallback_transport(error: &anyhow::Error) -> bool {
    is_retryable_error(error) || has_retryable_http_status(error)
}

fn should_suppress_transport(transport: &str, error: &anyhow::Error) -> bool {
    transport == "chat" && (is_retryable_error(error) || is_invalid_response_error(error))
}

fn normalize_completion_text(text: &str) -> String {
    text.trim().to_string()
}

fn compact_single_line(text: &str, max_chars: usize) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars.max(1))
        .collect::<String>()
}

#[derive(Debug, Deserialize)]
struct ChatCompletionPayload {
    #[serde(default)]
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessagePayload,
}

#[derive(Debug, Deserialize)]
struct ChatMessagePayload {
    content: MaybeText,
}

#[derive(Debug, Deserialize)]
struct ResponsesPayload {
    #[serde(default)]
    output_text: Option<MaybeText>,
    #[serde(default)]
    output: Option<Vec<ResponseOutputItem>>,
}

#[derive(Debug, Deserialize)]
struct ResponseOutputItem {
    #[serde(default)]
    content: Option<Vec<MaybeText>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MaybeText {
    Plain(String),
    Rich(Vec<Value>),
}

impl MaybeText {
    fn into_text(self) -> Option<String> {
        match self {
            Self::Plain(text) => {
                let normalized = text.trim().to_string();
                (!normalized.is_empty()).then_some(normalized)
            }
            Self::Rich(items) => {
                let text = items
                    .into_iter()
                    .filter_map(|item| {
                        item.get("text")
                            .or_else(|| item.get("output_text"))
                            .or_else(|| item.get("value"))
                            .and_then(Value::as_str)
                            .map(ToString::to_string)
                    })
                    .collect::<String>()
                    .trim()
                    .to_string();
                (!text.is_empty()).then_some(text)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        extract_chat_stream_delta, extract_responses_stream_delta, find_sse_event_boundary,
        has_retryable_http_status, preferred_transports, value_to_text_piece,
    };
    use anyhow::anyhow;
    use serde_json::json;

    #[test]
    fn extracts_chat_stream_text_delta() {
        let payload = json!({
            "choices": [
                {
                    "delta": {
                        "content": "你好"
                    }
                }
            ]
        });

        assert_eq!(extract_chat_stream_delta(&payload).as_deref(), Some("你好"));
    }

    #[test]
    fn extracts_responses_stream_text_delta() {
        let payload = json!({
            "type": "response.output_text.delta",
            "delta": "世界"
        });

        assert_eq!(
            extract_responses_stream_delta(&payload).as_deref(),
            Some("世界")
        );
    }

    #[test]
    fn value_to_text_piece_joins_rich_parts() {
        let value = json!([
            { "text": "你" },
            { "text": "好" }
        ]);

        assert_eq!(value_to_text_piece(&value).as_deref(), Some("你好"));
    }

    #[test]
    fn finds_sse_boundary_for_crlf_frames() {
        let buffer = b"data: {\"delta\":\"x\"}\r\n\r\nrest";
        assert_eq!(find_sse_event_boundary(buffer), Some((19, 4)));
    }

    #[test]
    fn prefers_responses_for_cc_switch_proxy() {
        assert_eq!(
            preferred_transports("http://127.0.0.1:15721/v1"),
            vec!["responses".to_string(), "chat".to_string()]
        );
        assert_eq!(
            preferred_transports("http://new.xem8k5.top:3000/v1"),
            vec!["chat".to_string(), "responses".to_string()]
        );
    }

    #[test]
    fn detects_retryable_http_status_from_error_text() {
        assert!(has_retryable_http_status(&anyhow!(
            "聊天接口 responses 返回 HTTP 502: upstream bad gateway"
        )));
        assert!(!has_retryable_http_status(&anyhow!(
            "聊天接口 responses 返回 HTTP 401: invalid token"
        )));
    }
}
