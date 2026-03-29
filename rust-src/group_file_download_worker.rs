use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::event_utils::EventContext;
use crate::logger::Logger;

#[derive(Clone)]
pub struct GroupFileDownloadWorker {
    inner: Arc<Mutex<GroupFileDownloadWorkerHandle>>,
    next_request_id: Arc<AtomicU64>,
}

struct GroupFileDownloadWorkerHandle {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl GroupFileDownloadWorker {
    pub async fn start(project_root: &std::path::Path, config_path: &std::path::Path, logger: Logger) -> Result<Self> {
        let script_path = project_root.join("scripts").join("rust-group-download-worker.mjs");
        let mut child = Command::new("node")
            .arg(&script_path)
            .arg(config_path)
            .current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("启动群文件下载 worker 失败: {}", script_path.display()))?;

        let stdin = child.stdin.take().context("群文件下载 worker stdin 不可用")?;
        let stdout = child.stdout.take().context("群文件下载 worker stdout 不可用")?;

        let worker = Self {
            inner: Arc::new(Mutex::new(GroupFileDownloadWorkerHandle {
                child,
                stdin,
                stdout: BufReader::new(stdout),
            })),
            next_request_id: Arc::new(AtomicU64::new(1)),
        };
        logger.info("群文件下载兼容 worker 已启动。").await;
        Ok(worker)
    }

    // 这里把原版超重的下载/构建流量隔离到 Node worker，避免把 2800 行下载逻辑重新塞进主事件循环。
    pub async fn handle_group_message(&self, context: &EventContext, event: &Value, text: &str) -> Result<bool> {
        let response = self
            .request(
                "handle_group_message",
                json!({
                    "context": {
                        "messageType": context.message_type,
                        "groupId": context.group_id,
                        "userId": context.user_id,
                        "selfId": context.self_id
                    },
                    "event": event,
                    "text": text
                }),
            )
            .await?;
        Ok(response.get("handled").and_then(Value::as_bool).unwrap_or(false))
    }

    pub async fn start_group_download_flow_from_tool(
        &self,
        context: &EventContext,
        message_id: &str,
        request_text: &str,
        request: &Value,
    ) -> Result<Value> {
        let response = self
            .request(
                "start_group_download_flow_from_tool",
                json!({
                    "context": {
                        "messageType": context.message_type,
                        "groupId": context.group_id,
                        "userId": context.user_id,
                        "selfId": context.self_id
                    },
                    "event": {
                        "message_id": message_id.trim(),
                        "raw_message": request_text.trim()
                    },
                    "request": request
                }),
            )
            .await?;
        Ok(response.get("handled").cloned().unwrap_or(Value::Null))
    }

    pub async fn stop(&self) -> Result<()> {
        let _ = self.request("shutdown", Value::Null).await;
        let mut handle = self.inner.lock().await;
        let _ = handle.child.wait().await;
        Ok(())
    }

    async fn request(&self, action: &str, payload: Value) -> Result<Value> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let request = json!({
            "id": request_id,
            "action": action,
            "payload": payload
        });

        let mut handle = self.inner.lock().await;
        let line = serde_json::to_string(&request)?;
        handle.stdin.write_all(line.as_bytes()).await?;
        handle.stdin.write_all(b"\n").await?;
        handle.stdin.flush().await?;

        let mut response_line = String::new();
        let bytes = handle.stdout.read_line(&mut response_line).await?;
        if bytes == 0 {
            bail!("群文件下载 worker 提前退出");
        }

        let response: Value = serde_json::from_str(response_line.trim())
            .context("解析群文件下载 worker 响应失败")?;
        if response.get("ok").and_then(Value::as_bool) != Some(true) {
            bail!(
                "群文件下载 worker 请求失败: {}",
                response.get("error").and_then(Value::as_str).unwrap_or("unknown error")
            );
        }
        Ok(response)
    }
}
