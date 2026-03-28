use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::logger::Logger;
use crate::utils::{join_url, sleep_ms};

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

    // 先保留原版最关键的两条传输链路：chat/completions 与 responses。
    pub async fn complete(&self, messages: &[ChatMessage], options: CompleteOptions) -> Result<String> {
        self.validate()?;
        self.ensure_not_in_cooldown().await?;
        let transports = self.available_transports().await;
        let mut last_error = None;

        for transport in transports {
            let result = if transport == "responses" {
                self.complete_via_responses(messages, &options).await
            } else {
                self.complete_via_chat(messages, &options).await
            };

            match result {
                Ok(text) => {
                    *self.retryable_failure_streak.lock().await = 0;
                    self.transport_suppressed_until.lock().await.remove(&transport);
                    return Ok(text);
                }
                Err(error) => {
                    self.logger
                        .warn(format!("聊天接口 {transport} 失败：{error:#}"))
                        .await;
                    last_error = Some(error);
                }
            }
        }

        let Some(error) = last_error else {
            bail!("聊天接口调用失败");
        };

        if is_retryable_error(&error) {
            let mut streak = self.retryable_failure_streak.lock().await;
            *streak += 1;
            if *streak >= self.config.failure_cooldown_threshold.max(1) {
                *self.cooldown_until.lock().await =
                    Some(Instant::now() + Duration::from_millis(self.config.failure_cooldown_ms.max(1_000)));
                *self.cooldown_reason.lock().await = format!("{error:#}");
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

    async fn complete_via_chat(&self, messages: &[ChatMessage], options: &CompleteOptions) -> Result<String> {
        let model = options.model.clone().unwrap_or_else(|| self.config.model.clone());
        let temperature = options.temperature.unwrap_or(self.config.temperature);
        let body = json!({
            "model": model,
            "temperature": temperature,
            "messages": messages,
            "stream": false
        });

        self.execute_retriable_request(|| async {
            let url = join_url(&self.config.base_url, "chat/completions")?;
            let mut request = self.client.post(url).json(&body);
            if !self.config.api_key.trim().is_empty() {
                request = request.bearer_auth(&self.config.api_key);
            }
            let response = request.send().await.context("请求 chat/completions 失败")?;
            if !response.status().is_success() {
                bail!("聊天接口返回 HTTP {}", response.status());
            }
            let payload: Value = response.json().await.context("解析 chat/completions 响应失败")?;
            extract_assistant_text(&payload).context("聊天接口未返回可用文本")
        })
        .await
    }

    async fn complete_via_responses(&self, messages: &[ChatMessage], options: &CompleteOptions) -> Result<String> {
        let model = options.model.clone().unwrap_or_else(|| self.config.model.clone());
        let temperature = options.temperature.unwrap_or(self.config.temperature);
        let body = json!({
            "model": model,
            "temperature": temperature,
            "input": build_responses_input(messages)
        });

        self.execute_retriable_request(|| async {
            let url = join_url(&self.config.base_url, "responses")?;
            let mut request = self.client.post(url).json(&body);
            if !self.config.api_key.trim().is_empty() {
                request = request.bearer_auth(&self.config.api_key);
            }
            let response = request.send().await.context("请求 responses 失败")?;
            if !response.status().is_success() {
                bail!("聊天接口返回 HTTP {}", response.status());
            }
            let payload: Value = response.json().await.context("解析 responses 响应失败")?;
            extract_responses_text(&payload).context("聊天接口未返回可用文本")
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
                        .warn(format!("聊天接口请求异常，准备重试（{attempt}/{max_attempts}）：{error:#}"))
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
            let seconds = until.saturating_duration_since(Instant::now()).as_secs().max(1);
            let reason = self.cooldown_reason.lock().await.clone();
            bail!("聊天接口暂时不可用，已进入 {seconds} 秒冷却：{reason}");
        }
        Ok(())
    }

    async fn available_transports(&self) -> Vec<String> {
        let preferred = if is_cc_switch_proxy(&self.config.base_url) {
            vec!["responses".to_string(), "chat".to_string()]
        } else {
            vec!["chat".to_string(), "responses".to_string()]
        };
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

fn extract_assistant_text(payload: &Value) -> Option<String> {
    payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(value_to_text)
}

fn extract_responses_text(payload: &Value) -> Option<String> {
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

fn build_responses_input(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|message| {
            let content = match &message.content {
                Value::String(text) => vec![json!({ "type": "input_text", "text": text })],
                Value::Array(items) => items
                    .iter()
                    .filter_map(|item| {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            Some(json!({ "type": "input_text", "text": text }))
                        } else if let Some(url) = item.get("image_url").and_then(|value| {
                            value.as_str().map(ToString::to_string).or_else(|| {
                                value.get("url").and_then(Value::as_str).map(ToString::to_string)
                            })
                        }) {
                            Some(json!({ "type": "input_image", "image_url": url }))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>(),
                other => vec![json!({ "type": "input_text", "text": other.to_string() })],
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
    (lower.contains("127.0.0.1:15721") || lower.contains("localhost:15721")) && lower.contains("/v1")
}

fn is_retryable_error(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    text.contains("network") || text.contains("socket") || text.contains("timeout") || text.contains("timed out")
}
