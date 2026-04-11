use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
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
pub struct OpenAiProviderConfig {
    pub name: Option<String>,
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

#[derive(Debug, Clone)]
pub struct OpenAiChatClientConfig {
    pub enabled: bool,
    pub provider_label: Option<String>,
    pub prefer_provider_model: bool,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f64,
    pub request_timeout_ms: u64,
    pub retry_attempts: usize,
    pub retry_delay_ms: u64,
    pub failure_cooldown_ms: u64,
    pub failure_cooldown_threshold: usize,
    pub alternate_providers: Vec<OpenAiProviderConfig>,
    pub expensive_fallback_base_url: Option<String>,
    pub expensive_fallback_api_key: Option<String>,
    pub expensive_fallback_model: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CompleteOptions {
    pub model: Option<String>,
    pub temperature: Option<f64>,
    pub allow_expensive_fallback: bool,
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
    alternate_clients: Vec<OpenAiChatClient>,
    expensive_fallback_client: Option<Box<OpenAiChatClient>>,
}

impl OpenAiChatClient {
    pub fn new(config: OpenAiChatClientConfig, logger: Logger) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .build()
            .context("创建聊天客户端失败")?;
        let alternate_clients = build_alternate_clients(&config, logger.clone())?;
        let expensive_fallback_client = build_expensive_fallback_client(&config, logger.clone())?;
        Ok(Self {
            config,
            logger,
            client,
            cooldown_until: Default::default(),
            cooldown_reason: Default::default(),
            retryable_failure_streak: Default::default(),
            transport_suppressed_until: Default::default(),
            alternate_clients,
            expensive_fallback_client,
        })
    }

    // 先保留原版最关键的两条传输链路：chat/completions 与 responses。
    pub async fn complete(
        &self,
        messages: &[ChatMessage],
        options: CompleteOptions,
    ) -> Result<String> {
        match self
            .complete_with_provider_priority(messages, &options)
            .await
        {
            Ok(text) => Ok(text),
            Err(primary_error) => {
                if !options.allow_expensive_fallback {
                    return Err(primary_error);
                }
                let Some(fallback_client) = self.expensive_fallback_client.as_deref() else {
                    return Err(primary_error);
                };
                self.logger
                    .warn("主聊天接口失败，当前请求允许昂贵兜底，改用 plus key 重试。")
                    .await;
                let mut fallback_options = options.clone();
                fallback_options.allow_expensive_fallback = false;
                match fallback_client
                    .complete_without_expensive_fallback(messages, &fallback_options)
                    .await
                {
                    Ok(text) => {
                        self.logger
                            .warn("昂贵兜底已成功返回结果；请留意额外成本。")
                            .await;
                        Ok(text)
                    }
                    Err(fallback_error) => {
                        Err(primary_error.context(format!("昂贵兜底也失败：{fallback_error:#}")))
                    }
                }
            }
        }
    }

    async fn complete_with_provider_priority(
        &self,
        messages: &[ChatMessage],
        options: &CompleteOptions,
    ) -> Result<String> {
        match self
            .complete_without_expensive_fallback(messages, options)
            .await
        {
            Ok(text) => Ok(text),
            Err(primary_error) => {
                let mut failed_label = self.provider_label();
                let mut last_error = primary_error;
                for alternate in &self.alternate_clients {
                    let next_label = alternate.provider_label();
                    self.logger
                        .warn(format!(
                            "聊天 provider {} 失败，尝试下一个 provider：{}",
                            failed_label,
                            next_label
                        ))
                        .await;
                    match alternate
                        .complete_without_expensive_fallback(messages, options)
                        .await
                    {
                        Ok(text) => {
                            self.logger
                                .warn(format!("已切换到备用聊天 provider：{next_label}"))
                                .await;
                            return Ok(text);
                        }
                        Err(error) => {
                            failed_label = next_label;
                            last_error = error;
                        }
                    }
                }
                Err(last_error)
            }
        }
    }

    async fn complete_without_expensive_fallback(
        &self,
        messages: &[ChatMessage],
        options: &CompleteOptions,
    ) -> Result<String> {
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
                    self.transport_suppressed_until
                        .lock()
                        .await
                        .remove(&transport);
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
                *self.cooldown_until.lock().await = Some(
                    Instant::now()
                        + Duration::from_millis(self.config.failure_cooldown_ms.max(1_000)),
                );
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

    fn provider_label(&self) -> String {
        self.config
            .provider_label
            .as_deref()
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("{} [{}]", self.config.base_url, self.config.model))
    }

    async fn complete_via_chat(
        &self,
        messages: &[ChatMessage],
        options: &CompleteOptions,
    ) -> Result<String> {
        let model = self.resolve_model(options);
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
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .json(&body);
            if !self.config.api_key.trim().is_empty() {
                request = request.bearer_auth(&self.config.api_key);
            }
            let response = request.send().await.context("请求 chat/completions 失败")?;
            if !response.status().is_success() {
                bail!("聊天接口返回 HTTP {}", response.status());
            }
            if is_sse_response(&response) {
                read_chat_stream_text(response).await
            } else {
                let payload: Value = response
                    .json()
                    .await
                    .context("解析 chat/completions 响应失败")?;
                extract_assistant_text(&payload).context("聊天接口未返回可用文本")
            }
        })
        .await
    }

    async fn complete_via_responses(
        &self,
        messages: &[ChatMessage],
        options: &CompleteOptions,
    ) -> Result<String> {
        let model = self.resolve_model(options);
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
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .json(&body);
            if !self.config.api_key.trim().is_empty() {
                request = request.bearer_auth(&self.config.api_key);
            }
            let response = request.send().await.context("请求 responses 失败")?;
            if !response.status().is_success() {
                bail!("聊天接口返回 HTTP {}", response.status());
            }
            if is_sse_response(&response) {
                read_responses_stream_text(response).await
            } else {
                let payload: Value = response.json().await.context("解析 responses 响应失败")?;
                extract_responses_text(&payload).context("聊天接口未返回可用文本")
            }
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

    fn resolve_model(&self, options: &CompleteOptions) -> String {
        if self.config.prefer_provider_model {
            self.config.model.clone()
        } else {
            options
                .model
                .clone()
                .unwrap_or_else(|| self.config.model.clone())
        }
    }
}

fn build_alternate_clients(
    config: &OpenAiChatClientConfig,
    logger: Logger,
) -> Result<Vec<OpenAiChatClient>> {
    config
        .alternate_providers
        .iter()
        .cloned()
        .map(|provider| {
            OpenAiChatClient::new(client_config_from_provider(provider), logger.clone())
        })
        .collect()
}

fn build_expensive_fallback_client(
    config: &OpenAiChatClientConfig,
    logger: Logger,
) -> Result<Option<Box<OpenAiChatClient>>> {
    let api_key = config
        .expensive_fallback_api_key
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    if api_key.is_empty() {
        return Ok(None);
    }

    let mut fallback_config = config.clone();
    fallback_config.provider_label = Some("plus-expensive-fallback".to_string());
    fallback_config.base_url = config
        .expensive_fallback_base_url
        .as_deref()
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .unwrap_or(config.base_url.as_str())
        .to_string();
    fallback_config.api_key = api_key.to_string();
    fallback_config.model = config
        .expensive_fallback_model
        .as_deref()
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .unwrap_or(config.model.as_str())
        .to_string();
    fallback_config.alternate_providers = Vec::new();
    fallback_config.expensive_fallback_base_url = None;
    fallback_config.expensive_fallback_api_key = None;
    fallback_config.expensive_fallback_model = None;

    Ok(Some(Box::new(OpenAiChatClient::new(
        fallback_config,
        logger,
    )?)))
}

fn client_config_from_provider(provider: OpenAiProviderConfig) -> OpenAiChatClientConfig {
    OpenAiChatClientConfig {
        enabled: true,
        provider_label: provider.name,
        prefer_provider_model: true,
        base_url: provider.base_url,
        api_key: provider.api_key,
        model: provider.model,
        temperature: provider.temperature,
        request_timeout_ms: provider.request_timeout_ms,
        retry_attempts: provider.retry_attempts,
        retry_delay_ms: provider.retry_delay_ms,
        failure_cooldown_ms: provider.failure_cooldown_ms,
        failure_cooldown_threshold: provider.failure_cooldown_threshold,
        alternate_providers: Vec::new(),
        expensive_fallback_base_url: None,
        expensive_fallback_api_key: None,
        expensive_fallback_model: None,
    }
}

async fn read_chat_stream_text(response: reqwest::Response) -> Result<String> {
    let mut combined = String::new();
    let mut snapshot = None;
    consume_sse_stream(response, |event| {
        if event.data.trim() == "[DONE]" {
            return Ok(true);
        }
        let payload = serde_json::from_str::<Value>(&event.data)
            .with_context(|| format!("解析 chat/completions SSE 事件失败: {}", event.data))?;
        if let Some(delta) = extract_chat_stream_delta_text(&payload) {
            combined.push_str(&delta);
            return Ok(false);
        }
        if combined.trim().is_empty() {
            snapshot = extract_assistant_text(&payload).or(snapshot.take());
        }
        Ok(false)
    })
    .await?;

    normalize_text_output(combined)
        .or(snapshot.and_then(normalize_text_output))
        .context("聊天接口未返回可用文本")
}

async fn read_responses_stream_text(response: reqwest::Response) -> Result<String> {
    let mut combined = String::new();
    let mut snapshot = None;
    consume_sse_stream(response, |event| {
        if event.data.trim() == "[DONE]" {
            return Ok(true);
        }
        let payload = serde_json::from_str::<Value>(&event.data)
            .with_context(|| format!("解析 responses SSE 事件失败: {}", event.data))?;
        if let Some(delta) = extract_responses_stream_delta_text(&payload) {
            combined.push_str(&delta);
            return Ok(false);
        }
        if combined.trim().is_empty() {
            snapshot = extract_responses_stream_snapshot_text(&payload).or(snapshot.take());
        }
        Ok(false)
    })
    .await?;

    normalize_text_output(combined)
        .or(snapshot.and_then(normalize_text_output))
        .context("聊天接口未返回可用文本")
}

async fn consume_sse_stream<F>(response: reqwest::Response, mut on_event: F) -> Result<()>
where
    F: FnMut(SseEvent) -> Result<bool>,
{
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::<u8>::new();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("读取聊天 SSE 数据失败")?;
        for byte in bytes {
            if byte != b'\r' {
                buffer.push(byte);
            }
        }

        while let Some(index) = find_sse_separator(&buffer) {
            let block = buffer[..index].to_vec();
            buffer.drain(..index + 2);
            if let Some(event) = parse_sse_event(&block)?
                && on_event(event)?
            {
                return Ok(());
            }
        }
    }

    if !buffer.is_empty()
        && let Some(event) = parse_sse_event(&buffer)?
    {
        let _ = on_event(event)?;
    }
    Ok(())
}

fn normalize_text_output(text: String) -> Option<String> {
    let normalized = text.trim().to_string();
    (!normalized.is_empty()).then_some(normalized)
}

fn is_sse_response(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase().contains("event-stream"))
        .unwrap_or(false)
}

fn extract_chat_stream_delta_text(payload: &Value) -> Option<String> {
    payload
        .get("choices")
        .and_then(Value::as_array)
        .map(|choices| {
            choices
                .iter()
                .filter_map(|choice| {
                    choice
                        .get("delta")
                        .and_then(|delta| delta.get("content"))
                        .or_else(|| choice.get("message").and_then(|item| item.get("content")))
                        .and_then(value_to_text)
                })
                .collect::<String>()
        })
        .and_then(normalize_text_output)
}

fn extract_responses_stream_delta_text(payload: &Value) -> Option<String> {
    match payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "response.output_text.delta" => payload.get("delta").and_then(value_to_text),
        "response.output_text.done" => payload
            .get("text")
            .or_else(|| payload.get("delta"))
            .and_then(value_to_text),
        _ => None,
    }
}

fn extract_responses_stream_snapshot_text(payload: &Value) -> Option<String> {
    if let Some(response) = payload.get("response")
        && let Some(text) = extract_responses_text(response)
    {
        return Some(text);
    }
    if let Some(item) = payload.get("item")
        && let Some(text) = extract_responses_text(item)
    {
        return Some(text);
    }
    extract_responses_text(payload)
}

#[derive(Debug)]
struct SseEvent {
    data: String,
}

fn parse_sse_event(block: &[u8]) -> Result<Option<SseEvent>> {
    let block = std::str::from_utf8(block).context("聊天 SSE 事件不是合法 UTF-8")?;
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
    Ok(Some(SseEvent {
        data: data_lines.join("\n"),
    }))
}

fn find_sse_separator(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|window| window == b"\n\n")
}

fn extract_assistant_text(payload: &Value) -> Option<String> {
    serde_json::from_value::<ChatCompletionPayload>(payload.clone())
        .ok()
        .and_then(|parsed| parsed.choices.into_iter().next())
        .and_then(|choice| choice.message.content.into_text())
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

fn value_to_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => normalize_text_output(text.clone()),
        Value::Array(items) => {
            normalize_text_output(items.iter().filter_map(value_to_text).collect::<String>())
        }
        Value::Object(map) => ["text", "output_text", "value", "content", "delta"]
            .iter()
            .filter_map(|key| map.get(*key))
            .find_map(value_to_text),
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

fn is_retryable_error(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    text.contains("network")
        || text.contains("socket")
        || text.contains("timeout")
        || text.contains("timed out")
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
            Self::Plain(text) => value_to_text(&Value::String(text)),
            Self::Rich(items) => value_to_text(&Value::Array(items)),
        }
    }
}
