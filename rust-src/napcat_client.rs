use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::logger::Logger;
use crate::utils::{build_reply_message, join_url, sleep_ms, split_text};

#[derive(Debug, Clone)]
pub struct NapCatClientConfig {
    pub base_url: String,
    pub event_base_url: String,
    pub event_path: String,
    pub request_timeout_ms: u64,
    pub headers: BTreeMap<String, String>,
    pub max_concurrent_events: usize,
}

#[derive(Clone)]
pub struct NapCatClient {
    config: NapCatClientConfig,
    logger: Logger,
    client: Client,
    stopped: Arc<AtomicBool>,
    event_semaphore: Arc<Semaphore>,
}

impl NapCatClient {
    pub fn new(config: NapCatClientConfig, logger: Logger) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .build()
            .context("创建 NapCat HTTP 客户端失败")?;
        Ok(Self {
            event_semaphore: Arc::new(Semaphore::new(config.max_concurrent_events.max(1))),
            config,
            logger,
            client,
            stopped: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    pub async fn call(&self, action: &str, params: Value) -> Result<Value> {
        let url = join_url(&self.config.base_url, action)?;
        let mut request = self.client.post(url).json(&params);
        for (key, value) in &self.config.headers {
            request = request.header(key, value);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("调用 NapCat API 失败: {action}"))?;
        if !response.status().is_success() {
            bail!("NapCat API {action} 返回 HTTP {}", response.status());
        }
        let payload: Value = response
            .json()
            .await
            .with_context(|| format!("解析 NapCat API 返回失败: {action}"))?;
        if let Some(status) = payload.get("status").and_then(Value::as_str)
            && status != "ok"
        {
            bail!(
                "NapCat API {action} 失败: {}",
                payload
                    .get("message")
                    .or_else(|| payload.get("wording"))
                    .and_then(Value::as_str)
                    .unwrap_or(status)
            );
        }
        if let Some(retcode) = payload.get("retcode").and_then(Value::as_i64)
            && retcode != 0
        {
            bail!("NapCat API {action} retcode={retcode}");
        }
        Ok(payload.get("data").cloned().unwrap_or(payload))
    }

    pub async fn send_group_message(&self, group_id: &str, message: Value) -> Result<Value> {
        self.call(
            "send_group_msg",
            json!({
                "group_id": group_id.trim(),
                "message": message
            }),
        )
        .await
    }

    pub async fn send_private_message(&self, user_id: &str, message: impl Into<Value>) -> Result<Value> {
        self.call(
            "send_private_msg",
            json!({
                "user_id": user_id.trim(),
                "message": message.into()
            }),
        )
        .await
    }

    pub async fn get_message(&self, message_id: &str) -> Result<Value> {
        self.call(
            "get_msg",
            json!({
                "message_id": message_id.trim()
            }),
        )
        .await
    }

    pub async fn get_group_member_info(&self, group_id: &str, user_id: &str, no_cache: bool) -> Result<Value> {
        self.call(
            "get_group_member_info",
            json!({
                "group_id": group_id.trim(),
                "user_id": user_id.trim(),
                "no_cache": no_cache
            }),
        )
        .await
    }

    pub async fn delete_message(&self, message_id: &str) -> Result<Value> {
        self.call(
            "delete_msg",
            json!({
                "message_id": message_id.trim()
            }),
        )
        .await
    }

    pub async fn reply_text(
        &self,
        message_type: &str,
        target_id: &str,
        reply_to_message_id: Option<&str>,
        text: &str,
    ) -> Result<Vec<Value>> {
        let mut results = Vec::new();
        for (index, part) in split_text(text, 1_400).iter().enumerate() {
            let message = if index == 0 {
                build_reply_message(reply_to_message_id, part)
            } else {
                Value::String(part.clone())
            };
            let result = if message_type == "group" {
                self.send_group_message(target_id, message).await?
            } else {
                self.send_private_message(target_id, message).await?
            };
            results.push(result);
        }
        Ok(results)
    }

    pub async fn start_event_loop<F, Fut>(&self, on_event: F) -> Result<()>
    where
        F: Fn(Value) -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let mut backoff_ms = 2_000u64;
        while !self.stopped.load(Ordering::SeqCst) {
            match self.run_event_stream(on_event.clone()).await {
                Ok(()) => backoff_ms = 2_000,
                Err(_) if self.stopped.load(Ordering::SeqCst) => return Ok(()),
                Err(error) => {
                    self.logger.warn(format!("SSE 连接断开：{error}")).await;
                    sleep_ms(backoff_ms).await;
                    backoff_ms = (backoff_ms * 2).min(30_000);
                }
            }
        }
        Ok(())
    }

    async fn run_event_stream<F, Fut>(&self, on_event: F) -> Result<()>
    where
        F: Fn(Value) -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let event_base = if self.config.event_base_url.trim().is_empty() {
            &self.config.base_url
        } else {
            &self.config.event_base_url
        };
        let url = join_url(event_base, &self.config.event_path)?;
        let mut request = self.client.get(url).header("Accept", "text/event-stream");
        for (key, value) in &self.config.headers {
            request = request.header(key, value);
        }
        let response = request.send().await.context("连接 NapCat SSE 失败")?;
        if !response.status().is_success() {
            bail!("NapCat SSE 返回 HTTP {}", response.status());
        }
        self.logger.info("NapCat SSE 已连接。").await;

        let mut stream = response.bytes_stream();
        let mut buffer = Vec::<u8>::new();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("读取 NapCat SSE 数据失败")?;
            for byte in bytes {
                if byte != b'\r' {
                    buffer.push(byte);
                }
            }

            // 这里按字节流切块，避免每次都把剩余 buffer 重新拷成新的 String。
            while let Some(index) = find_sse_separator(&buffer) {
                let block = buffer[..index].to_vec();
                buffer.drain(..index + 2);
                if let Some(event) = parse_sse_event(&block)? {
                    self.dispatch_event(on_event.clone(), event).await?;
                }
            }

            if self.stopped.load(Ordering::SeqCst) {
                return Ok(());
            }
        }
        bail!("NapCat SSE 连接已结束")
    }

    // 这里用信号量压住事件并发，避免某一波 SSE 突发把所有内存顶上去。
    async fn dispatch_event<F, Fut>(&self, on_event: F, event: Value) -> Result<()>
    where
        F: Fn(Value) -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let permit = self
            .event_semaphore
            .clone()
            .acquire_owned()
            .await
            .context("获取事件并发许可失败")?;
        let logger = self.logger.clone();
        tokio::spawn(async move {
            let _permit: OwnedSemaphorePermit = permit;
            if let Err(error) = on_event(event).await {
                logger.error(format!("事件处理失败：{error:#}")).await;
            }
        });
        Ok(())
    }
}

fn parse_sse_event(block: &[u8]) -> Result<Option<Value>> {
    let block = std::str::from_utf8(block).context("SSE 事件不是合法 UTF-8")?;
    let mut data_lines = Vec::new();
    for line in block.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start().to_string());
        }
    }
    if data_lines.is_empty() {
        return Ok(None);
    }
    let data = data_lines.join("\n");
    let payload = serde_json::from_str::<Value>(&data)
        .with_context(|| format!("解析 SSE 事件失败: {data}"))?;
    Ok(Some(payload))
}

fn find_sse_separator(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|window| window == b"\n\n")
}
