use anyhow::{Result, bail};
use chrono::{Local, TimeZone, Utc};
use serde_json::{Value, json};

use crate::config::QaConfig;
use crate::event_utils::{EventContext, get_sender_name};
use crate::logger::Logger;
use crate::openai_chat_client::{ChatMessage, CompleteOptions, OpenAiChatClient};
use crate::runtime_config_store::RuntimeConfigStore;
use crate::state_store::StateStore;

#[derive(Debug, Clone)]
pub struct ChatResult {
    pub text: String,
    pub notice: String,
}

#[derive(Debug, Clone)]
pub struct GroupPromptStatus {
    pub enabled: bool,
    pub proactive_reply_enabled: bool,
    pub filter_heartbeat_enabled: bool,
    pub filter_heartbeat_interval: u64,
    pub file_download_enabled: bool,
    pub file_download_folder_name: String,
    pub filter_prompt: String,
    pub answer_prompt: String,
}

#[derive(Clone)]
pub struct ChatSessionManager {
    config: QaConfig,
    chat_client: OpenAiChatClient,
    state_store: StateStore,
    logger: Logger,
    runtime_config_store: RuntimeConfigStore,
    heartbeat_counters: std::sync::Arc<tokio::sync::Mutex<std::collections::BTreeMap<String, u64>>>,
}

impl ChatSessionManager {
    pub fn new(
        config: QaConfig,
        chat_client: OpenAiChatClient,
        state_store: StateStore,
        logger: Logger,
        runtime_config_store: RuntimeConfigStore,
    ) -> Self {
        Self {
            config,
            chat_client,
            state_store,
            logger,
            runtime_config_store,
            heartbeat_counters: Default::default(),
        }
    }

    pub fn build_session_key(&self, context: &EventContext) -> String {
        if context.message_type == "group" {
            format!("qa:group:{}", context.group_id.trim())
        } else {
            format!("qa:private:{}", context.user_id.trim())
        }
    }

    pub async fn is_group_enabled(&self, group_id: &str) -> bool {
        self.runtime_config_store
            .is_qa_group_enabled(group_id, &self.config.enabled_group_ids)
            .await
    }

    pub async fn is_group_proactive_reply_enabled(&self, group_id: &str) -> bool {
        self.runtime_config_store
            .is_qa_group_proactive_reply_enabled(group_id, &self.config.enabled_group_ids)
            .await
    }

    pub async fn get_group_prompt_status(&self, group_id: &str) -> GroupPromptStatus {
        let override_entry = self.runtime_config_store.get_group_qa_override(group_id).await;
        GroupPromptStatus {
            enabled: self.is_group_enabled(group_id).await,
            proactive_reply_enabled: self.is_group_proactive_reply_enabled(group_id).await,
            filter_heartbeat_enabled: self
                .runtime_config_store
                .is_qa_group_filter_heartbeat_enabled(group_id, &self.config.enabled_group_ids)
                .await,
            filter_heartbeat_interval: self
                .runtime_config_store
                .get_qa_group_filter_heartbeat_interval(group_id)
                .await,
            file_download_enabled: self
                .runtime_config_store
                .is_qa_group_file_download_enabled(group_id)
                .await,
            file_download_folder_name: self
                .runtime_config_store
                .get_qa_group_file_download_folder_name(group_id)
                .await,
            filter_prompt: override_entry
                .as_ref()
                .map(|item| item.filter_prompt.as_str())
                .filter(|item| !item.trim().is_empty())
                .unwrap_or(self.config.filter.prompt.as_str())
                .to_string(),
            answer_prompt: override_entry
                .as_ref()
                .map(|item| item.answer_prompt.as_str())
                .filter(|item| !item.trim().is_empty())
                .unwrap_or(self.config.answer.system_prompt.as_str())
                .to_string(),
        }
    }

    pub async fn should_run_group_proactive_filter(&self, group_id: &str) -> (bool, u64, u64) {
        let normalized = group_id.trim();
        if normalized.is_empty() {
            return (true, 0, 1);
        }
        if !self
            .runtime_config_store
            .is_qa_group_filter_heartbeat_enabled(normalized, &self.config.enabled_group_ids)
            .await
        {
            self.heartbeat_counters.lock().await.remove(normalized);
            return (true, 0, 1);
        }
        let interval = self
            .runtime_config_store
            .get_qa_group_filter_heartbeat_interval(normalized)
            .await
            .max(1);
        if interval <= 1 {
            self.heartbeat_counters.lock().await.remove(normalized);
            return (true, 1, interval);
        }
        let mut counters = self.heartbeat_counters.lock().await;
        let next_count = counters.get(normalized).copied().unwrap_or_default() + 1;
        if next_count >= interval {
            counters.insert(normalized.to_string(), 0);
            (true, next_count, interval)
        } else {
            counters.insert(normalized.to_string(), next_count);
            (false, next_count, interval)
        }
    }

    pub async fn reset_group_filter_heartbeat(&self, group_id: &str) {
        self.heartbeat_counters.lock().await.remove(group_id.trim());
    }

    pub async fn record_incoming_message(&self, context: &EventContext, event: &Value, summary: &str) -> Result<()> {
        let session_key = self.build_session_key(context);
        let entry = json!({
            "role": "user",
            "kind": if context.message_type == "group" { "group-message" } else { "private-message" },
            "messageId": event.get("message_id").map(value_to_string).unwrap_or_default(),
            "userId": context.user_id,
            "sender": get_sender_name(event),
            "text": summary.trim(),
            "rawText": summary.trim(),
            "time": format_event_time(event.get("time").and_then(Value::as_i64)),
            "createdAt": Utc::now().to_rfc3339(),
        });
        self.state_store
            .append_chat_session_entry(&session_key, entry, self.config.answer.max_timeline_messages)
            .await?;
        Ok(())
    }

    pub async fn mark_hinted(&self, context: &EventContext, message_id: &str) -> Result<()> {
        let session_key = self.build_session_key(context);
        self.state_store
            .set_chat_session_hinted_message(&session_key, message_id)
            .await?;
        Ok(())
    }

    pub async fn chat(&self, context: &EventContext, input: &crate::message_input::ChatInput) -> Result<ChatResult> {
        if !input.has_content() {
            bail!("聊天内容不能为空");
        }
        let session_key = self.build_session_key(context);
        let timeline_text = self
            .build_timeline_block(&session_key, self.config.answer.context_window_messages)
            .await;
        let prompt_status = if context.message_type == "group" {
            Some(self.get_group_prompt_status(&context.group_id).await)
        } else {
            None
        };
        let system_prompt = prompt_status
            .as_ref()
            .map(|item| item.answer_prompt.as_str())
            .unwrap_or(self.config.answer.system_prompt.as_str());
        let user_content = [
            "以下是当前共享上下文：".to_string(),
            timeline_text,
            String::new(),
            "以下是本次需要你回答的请求：".to_string(),
            input.text.clone(),
        ]
        .join("\n");

        let answer = self
            .chat_client
            .complete(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: Value::String(system_prompt.to_string()),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: if input.images.is_empty() {
                            Value::String(user_content)
                        } else {
                            let mut parts = vec![json!({
                                "type": "text",
                                "text": user_content
                            })];
                            parts.extend(input.images.iter().cloned());
                            Value::Array(parts)
                        },
                    },
                ],
                CompleteOptions {
                    model: Some(self.config.answer.model.clone()),
                    temperature: Some(self.config.answer.temperature),
                },
            )
            .await?;
        self.append_assistant_entry(&session_key, context, &answer).await?;
        Ok(ChatResult {
            text: answer,
            notice: String::new(),
        })
    }

    pub async fn should_suggest_reply(&self, context: &EventContext, event: &Value, summary: &str) -> Result<(bool, String)> {
        let session_key = self.build_session_key(context);
        let session = self.state_store.get_chat_session(&session_key).await?;
        if session.last_hinted_message_id.trim() == event.get("message_id").map(value_to_string).unwrap_or_default() {
            return Ok((false, "already-hinted".to_string()));
        }

        let recent_context = self
            .build_timeline_from_messages(&session.messages, self.config.answer.context_window_messages.min(12))
            .await;
        let raw = self
            .chat_client
            .complete(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: Value::String(format!(
                            "{}\n\n你只负责判断是否值得提醒群友可以使用 Cain 来问。\n只输出 JSON：{{\"should_prompt\":boolean,\"reason\":\"简短原因\"}}。",
                            self.get_group_prompt_status(&context.group_id).await.filter_prompt
                        )),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: Value::String(format!(
                            "群号：{}\n发送者：{} ({})\n当前消息摘要：{}\n\n最近共享上下文：\n{}",
                            context.group_id,
                            get_sender_name(event),
                            context.user_id,
                            summary.trim(),
                            recent_context
                        )),
                    },
                ],
                CompleteOptions {
                    model: Some(self.config.filter.model.clone()),
                    temperature: Some(0.1),
                },
            )
            .await?;
        let parsed = extract_json_object(&raw);
        Ok((
            parsed.get("should_prompt").and_then(Value::as_bool).unwrap_or(false),
            parsed.get("reason").and_then(Value::as_str).unwrap_or_default().trim().to_string(),
        ))
    }

    pub async fn maybe_close_group_topic(&self, group_id: &str) -> Result<(bool, String)> {
        let context = EventContext {
            message_type: "group".to_string(),
            group_id: group_id.trim().to_string(),
            user_id: String::new(),
            self_id: String::new(),
        };
        let session_key = self.build_session_key(&context);
        let session = self.state_store.get_chat_session(&session_key).await?;
        if session.messages.is_empty() {
            return Ok((true, "empty-session".to_string()));
        }
        let raw = self
            .chat_client
            .complete(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: Value::String(self.config.topic_closure.system_prompt.clone()),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: Value::String(format!(
                            "群号：{}\n\n最近消息：\n{}",
                            group_id.trim(),
                            self.build_timeline_from_messages(&session.messages, self.config.topic_closure.message_window)
                                .await
                        )),
                    },
                ],
                CompleteOptions {
                    model: Some(self.config.topic_closure.model.clone()),
                    temperature: Some(self.config.topic_closure.temperature),
                },
            )
            .await?;
        let parsed = extract_json_object(&raw);
        let should_end = parsed.get("should_end").and_then(Value::as_bool).unwrap_or(false);
        let reason = parsed.get("reason").and_then(Value::as_str).unwrap_or_default().trim().to_string();
        if should_end {
            self.state_store.clear_chat_session(&session_key).await?;
        }
        Ok((should_end, reason))
    }

    pub async fn disable_group_proactive_replies(&self, group_id: &str) -> Result<GroupPromptStatus> {
        self.runtime_config_store
            .set_qa_group_proactive_reply_enabled(group_id, false, &self.config.enabled_group_ids)
            .await?;
        Ok(self.get_group_prompt_status(group_id).await)
    }

    pub async fn update_filter_prompt(&self, group_id: &str, instruction: &str) -> Result<(String, String)> {
        self.review_and_persist_prompt(group_id, "filter", instruction).await
    }

    pub async fn update_answer_prompt(&self, group_id: &str, instruction: &str) -> Result<(String, String)> {
        self.review_and_persist_prompt(group_id, "answer", instruction).await
    }

    pub async fn maybe_capture_correction_memory(&self, _context: &EventContext, _event: &Value) -> Result<Option<String>> {
        Ok(None)
    }

    async fn review_and_persist_prompt(&self, group_id: &str, prompt_type: &str, instruction: &str) -> Result<(String, String)> {
        let current = self.get_group_prompt_status(group_id).await;
        let current_prompt = if prompt_type == "filter" {
            current.filter_prompt
        } else {
            current.answer_prompt
        };
        let raw = self
            .chat_client
            .complete(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: Value::String(self.config.prompt_review.system_prompt.clone()),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: Value::String(format!(
                            "群号：{}\n\n目标类型：{}\n\n当前 prompt：\n{}\n\n管理员要求：\n{}",
                            group_id.trim(),
                            if prompt_type == "filter" { "过滤 prompt" } else { "聊天 prompt" },
                            current_prompt,
                            instruction.trim()
                        )),
                    },
                ],
                CompleteOptions {
                    model: Some(self.config.prompt_review.model.clone()),
                    temperature: Some(0.2),
                },
            )
            .await?;
        let parsed = extract_json_object(&raw);
        if !parsed.get("approved").and_then(Value::as_bool).unwrap_or(true) {
            bail!(
                "{}",
                parsed
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("prompt 审核未通过")
            );
        }
        let prompt = parsed
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .ok_or_else(|| anyhow::anyhow!("prompt 审核未返回有效 prompt"))?
            .to_string();
        let existing = self.runtime_config_store.get_group_qa_override(group_id).await;
        self.runtime_config_store
            .set_group_qa_override(crate::runtime_config_store::GroupQaOverride {
                group_id: group_id.trim().to_string(),
                filter_prompt: if prompt_type == "filter" {
                    prompt.clone()
                } else {
                    existing
                        .as_ref()
                        .map(|item| item.filter_prompt.clone())
                        .unwrap_or_default()
                },
                answer_prompt: if prompt_type == "answer" {
                    prompt.clone()
                } else {
                    existing
                        .as_ref()
                        .map(|item| item.answer_prompt.clone())
                        .unwrap_or_default()
                },
                created_at: existing.as_ref().map(|item| item.created_at.clone()).unwrap_or_default(),
                updated_at: String::new(),
            })
            .await?;
        Ok((
            prompt,
            parsed.get("reason").and_then(Value::as_str).unwrap_or_default().trim().to_string(),
        ))
    }

    async fn append_assistant_entry(&self, session_key: &str, context: &EventContext, answer: &str) -> Result<()> {
        let entry = json!({
            "role": "assistant",
            "kind": "answer",
            "messageId": "",
            "userId": context.self_id,
            "sender": "Cain",
            "text": answer.trim(),
            "time": Utc::now().to_rfc3339(),
            "createdAt": Utc::now().to_rfc3339(),
        });
        self.state_store
            .append_chat_session_entry(session_key, entry, self.config.answer.max_timeline_messages)
            .await?;
        Ok(())
    }

    async fn build_timeline_block(&self, session_key: &str, max_messages: usize) -> String {
        let session = match self.state_store.get_chat_session(session_key).await {
            Ok(session) => session,
            Err(_) => return "(暂无共享上下文)".to_string(),
        };
        self.build_timeline_from_messages(&session.messages, max_messages).await
    }

    async fn build_timeline_from_messages(&self, messages: &[Value], max_messages: usize) -> String {
        let items = if messages.len() > max_messages {
            &messages[messages.len() - max_messages..]
        } else {
            messages
        };
        if items.is_empty() {
            return "(暂无共享上下文)".to_string();
        }
        items.iter()
            .enumerate()
            .map(|(index, item)| {
                let speaker = item
                    .get("sender")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| {
                        if item.get("role").and_then(Value::as_str) == Some("assistant") {
                            "Cain"
                        } else {
                            "群友"
                        }
                    })
                    .trim();
                let text = item.get("text").and_then(Value::as_str).unwrap_or("(空消息)").trim();
                format!(
                    "{}. [{}] {}：{}",
                    index + 1,
                    item.get("time").and_then(Value::as_str).unwrap_or("-"),
                    speaker,
                    text
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn extract_json_object(text: &str) -> Value {
    let source = text.trim();
    let mut depth = 0usize;
    let mut start_index = None;
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in source.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            continue;
        }
        if ch == '{' {
            if depth == 0 {
                start_index = Some(index);
            }
            depth += 1;
            continue;
        }
        if ch == '}' {
            if depth == 0 {
                continue;
            }
            depth -= 1;
            if depth == 0 {
                if let Some(start) = start_index {
                    if let Ok(value) = serde_json::from_str::<Value>(&source[start..=index]) {
                        return value;
                    }
                }
            }
        }
    }
    json!({})
}

fn format_event_time(epoch_seconds: Option<i64>) -> String {
    if let Some(seconds) = epoch_seconds
        && seconds > 0
        && let Some(date) = Utc.timestamp_opt(seconds, 0).single()
    {
        return date.with_timezone(&Local).to_rfc3339();
    }
    Utc::now().to_rfc3339()
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}
