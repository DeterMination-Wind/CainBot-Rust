use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use chrono::{Local, TimeZone, Utc};
use serde_json::{Value, json};
use tokio::fs;
use tokio::sync::Mutex;

use crate::config::QaConfig;
use crate::event_utils::{EventContext, get_sender_name, plain_text_from_message};
use crate::logger::Logger;
use crate::message_input::ChatInput;
use crate::openai_chat_client::{ChatMessage, CompleteOptions, OpenAiChatClient};
use crate::reply_markdown_renderer::render_reply_markdown_image;
use crate::runtime_config_store::{GroupQaOverride, RuntimeConfigStore};
use crate::state_store::StateStore;

#[derive(Debug, Clone)]
pub struct ChatResult {
    pub text: String,
    pub notice: String,
    pub group_file_download_request: Option<GroupFileDownloadRequest>,
}

#[derive(Debug, Clone)]
pub struct GroupFileDownloadRequest {
    pub request_text: String,
    pub request: Value,
}

#[derive(Debug, Clone, Default)]
pub struct LowInformationReplyReview {
    pub text: String,
    pub start_group_file_download: bool,
    pub request_text: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct HallucinationReplyReview {
    pub approved: bool,
    pub feedback: String,
    pub reason: String,
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
    runtime_config_store: RuntimeConfigStore,
    heartbeat_counters: Arc<Mutex<BTreeMap<String, u64>>>,
    correction_memory_checked: Arc<Mutex<HashSet<String>>>,
    logger: Logger,
}

impl ChatSessionManager {
    pub async fn start(
        _project_root: &Path,
        _config_path: &Path,
        config: QaConfig,
        chat_client: OpenAiChatClient,
        state_store: StateStore,
        logger: Logger,
        runtime_config_store: RuntimeConfigStore,
    ) -> Result<Self> {
        let manager = Self {
            config,
            chat_client,
            state_store,
            runtime_config_store,
            heartbeat_counters: Default::default(),
            correction_memory_checked: Default::default(),
            logger,
        };
        Ok(manager)
    }

    pub async fn stop(&self) -> Result<()> {
        Ok(())
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
        let override_entry = self
            .runtime_config_store
            .get_group_qa_override(group_id)
            .await;
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

    pub async fn record_incoming_message(
        &self,
        context: &EventContext,
        event: &Value,
        summary: &str,
    ) -> Result<()> {
        let session_key = self.build_session_key(context);
        self.state_store.refresh_chat_sessions_from_disk().await?;
        let entry = json!({
            "role": "user",
            "kind": if context.message_type == "group" { "group-message" } else { "private-message" },
            "messageId": event.get("message_id").map(value_to_string).unwrap_or_default(),
            "userId": context.user_id,
            "sender": get_sender_name(event),
            "text": summary.trim(),
            "rawText": plain_text_from_message(event.get("message").unwrap_or(&Value::Null), event.get("raw_message").and_then(Value::as_str)),
            "time": format_event_time(event.get("time").and_then(Value::as_i64)),
            "createdAt": Utc::now().to_rfc3339(),
        });
        self.state_store
            .append_chat_session_entry(
                &session_key,
                entry,
                self.config.answer.max_timeline_messages,
            )
            .await?;
        Ok(())
    }

    pub async fn mark_hinted(&self, context: &EventContext, message_id: &str) -> Result<()> {
        let session_key = self.build_session_key(context);
        self.state_store.refresh_chat_sessions_from_disk().await?;
        self.state_store
            .set_chat_session_hinted_message(&session_key, message_id)
            .await?;
        Ok(())
    }

    pub async fn chat(
        &self,
        context: &EventContext,
        input: &ChatInput,
        allow_expensive_fallback: bool,
    ) -> Result<ChatResult> {
        if !input.has_content() || input.history_text.trim().is_empty() {
            bail!("聊天内容不能为空");
        }

        let session_key = self.build_session_key(context);
        self.state_store.refresh_chat_sessions_from_disk().await?;

        let user_entry = json!({
            "role": "user",
            "kind": if context.message_type == "group" { "direct-question" } else { "private-question" },
            "messageId": input.runtime_context.current_message_id,
            "userId": context.user_id,
            "sender": if input.runtime_context.sender_name.trim().is_empty() {
                if context.user_id.trim().is_empty() { "用户".to_string() } else { context.user_id.clone() }
            } else {
                input.runtime_context.sender_name.clone()
            },
            "text": if input.runtime_context.timeline_text.trim().is_empty() {
                input.history_text.chars().take(600).collect::<String>()
            } else {
                input.runtime_context.timeline_text.chars().take(600).collect::<String>()
            },
            "time": format_event_time((input.runtime_context.current_time > 0).then_some(input.runtime_context.current_time)),
            "createdAt": Utc::now().to_rfc3339(),
        });
        self.state_store
            .append_chat_session_entry(
                &session_key,
                user_entry,
                self.config.answer.max_timeline_messages,
            )
            .await?;

        if context.message_type == "group"
            && self
                .runtime_config_store
                .is_qa_group_file_download_enabled(&context.group_id)
                .await
        {
            let request_text = if !input.runtime_context.timeline_text.trim().is_empty() {
                input.runtime_context.timeline_text.trim().to_string()
            } else if !input.text.trim().is_empty() {
                input.text.trim().to_string()
            } else {
                input.history_text.trim().to_string()
            };
            if looks_like_group_file_download_request(&request_text) {
                let handoff_entry = json!({
                    "role": "assistant",
                    "kind": "tool-handoff",
                    "messageId": "",
                    "userId": context.self_id,
                    "sender": "Cain",
                    "text": "[已转交群文件下载流程]",
                    "time": Utc::now().to_rfc3339(),
                    "createdAt": Utc::now().to_rfc3339(),
                });
                self.state_store
                    .append_chat_session_entry(
                        &session_key,
                        handoff_entry,
                        self.config.answer.max_timeline_messages,
                    )
                    .await?;
                return Ok(ChatResult {
                    text: String::new(),
                    notice: "group-file-download-started".to_string(),
                    group_file_download_request: Some(GroupFileDownloadRequest {
                        request_text: request_text.clone(),
                        request: json!({
                            "request_text": request_text
                        }),
                    }),
                });
            }
        }

        let session = self.state_store.get_chat_session(&session_key).await?;
        let timeline = self
            .build_timeline_from_messages(
                &session.messages,
                self.config.answer.context_window_messages,
            )
            .await;
        let system_prompt = self.build_answer_system_prompt(context).await;
        let user_prompt = format!(
            "以下是当前共享上下文：\n{}\n\n以下是本次需要你回答的请求：\n{}",
            timeline,
            input.text.trim()
        );
        let user_content = if input.images.is_empty() {
            Value::String(user_prompt)
        } else {
            let mut parts = vec![json!({
                "type": "text",
                "text": user_prompt,
            })];
            parts.extend(input.images.iter().cloned());
            Value::Array(parts)
        };
        let mut working_messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: Value::String(system_prompt),
            },
            ChatMessage {
                role: "user".to_string(),
                content: user_content,
            },
        ];
        let review_source_text = if !input.runtime_context.timeline_text.trim().is_empty() {
            input.runtime_context.timeline_text.trim().to_string()
        } else if !input.text.trim().is_empty() {
            input.text.trim().to_string()
        } else {
            input.history_text.trim().to_string()
        };
        let on_low_information = "suppress";
        let mut low_information_retry_attempts = 0usize;
        let mut hallucination_retry_attempts = 0usize;

        loop {
            let completion = self
                .chat_client
                .complete(
                    &working_messages,
                    CompleteOptions {
                        model: Some(self.config.answer.model.clone()),
                        temperature: Some(self.config.answer.temperature),
                        allow_expensive_fallback,
                    },
                )
                .await?;
            if let Some(handoff) = parse_group_file_download_handoff(&completion) {
                if context.message_type != "group" {
                    let fallback_text = "文件下载流程仅支持群聊。请在对应群里 @ 我并说明版本与平台，我会直接走下载状态机。";
                    let assistant_entry = json!({
                        "role": "assistant",
                        "kind": "answer",
                        "messageId": "",
                        "userId": context.self_id,
                        "sender": "Cain",
                        "text": fallback_text,
                        "time": Utc::now().to_rfc3339(),
                        "createdAt": Utc::now().to_rfc3339(),
                    });
                    self.state_store
                        .append_chat_session_entry(
                            &session_key,
                            assistant_entry,
                            self.config.answer.max_timeline_messages,
                        )
                        .await?;
                    return Ok(ChatResult {
                        text: fallback_text.to_string(),
                        notice: String::new(),
                        group_file_download_request: None,
                    });
                }
                let handoff_entry = json!({
                    "role": "assistant",
                    "kind": "tool-handoff",
                    "messageId": "",
                    "userId": context.self_id,
                    "sender": "Cain",
                    "text": "[已转交群文件下载流程]",
                    "time": Utc::now().to_rfc3339(),
                    "createdAt": Utc::now().to_rfc3339(),
                });
                self.state_store
                    .append_chat_session_entry(
                        &session_key,
                        handoff_entry,
                        self.config.answer.max_timeline_messages,
                    )
                    .await?;
                return Ok(ChatResult {
                    text: String::new(),
                    notice: "group-file-download-started".to_string(),
                    group_file_download_request: Some(handoff),
                });
            }
            let answer_text = completion.trim().to_string();
            if answer_text.is_empty() {
                self.logger
                    .warn(format!(
                        "聊天主模型返回空内容，抑制本次回复：source={}",
                        review_source_text.chars().take(120).collect::<String>()
                    ))
                    .await;
                return Ok(ChatResult {
                    text: String::new(),
                    notice: "review-suppressed".to_string(),
                    group_file_download_request: None,
                });
            }

            self.logger
                .info(format!(
                    "低信息检查开始：attempt={}, source={}, reply={}",
                    low_information_retry_attempts + 1,
                    review_source_text.chars().take(120).collect::<String>(),
                    answer_text.chars().take(120).collect::<String>()
                ))
                .await;
            let low_information_review = self
                .review_low_information_reply(&review_source_text, &answer_text, on_low_information)
                .await?;
            if low_information_review.start_group_file_download {
                return Ok(ChatResult {
                    text: String::new(),
                    notice: "group-file-download-started".to_string(),
                    group_file_download_request: Some(GroupFileDownloadRequest {
                        request_text: if low_information_review.request_text.trim().is_empty() {
                            review_source_text.clone()
                        } else {
                            low_information_review.request_text.trim().to_string()
                        },
                        request: json!({
                            "request_text": if low_information_review.request_text.trim().is_empty() {
                                review_source_text.clone()
                            } else {
                                low_information_review.request_text.trim().to_string()
                            }
                        }),
                    }),
                });
            }

            let reviewed_answer = low_information_review.text.trim().to_string();
            if reviewed_answer.is_empty() {
                low_information_retry_attempts += 1;
                self.logger
                    .info(format!(
                        "低信息检查打回主模型重答：attempt={}, feedback={}",
                        low_information_retry_attempts,
                        if low_information_review.reason.trim().is_empty() {
                            "no-feedback"
                        } else {
                            low_information_review.reason.trim()
                        }
                    ))
                    .await;
                working_messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: Value::String(answer_text),
                });
                working_messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: Value::String(build_low_information_retry_prompt(
                        low_information_review.reason.as_str(),
                    )),
                });
                continue;
            }
            self.logger
                .info(format!(
                    "低信息检查通过：{}",
                    if low_information_review.reason.trim().is_empty() {
                        "no-reason"
                    } else {
                        low_information_review.reason.trim()
                    }
                ))
                .await;

            self.logger
                .info(format!(
                    "幻觉检查开始：attempt={}, source={}, reply={}",
                    hallucination_retry_attempts + 1,
                    review_source_text.chars().take(120).collect::<String>(),
                    reviewed_answer.chars().take(120).collect::<String>()
                ))
                .await;
            let hallucination_review = self
                .review_hallucination_reply(&review_source_text, &reviewed_answer)
                .await?;
            if hallucination_review.approved {
                self.logger
                    .info(format!(
                        "幻觉检查通过：{}",
                        if hallucination_review.reason.trim().is_empty() {
                            "no-reason"
                        } else {
                            hallucination_review.reason.trim()
                        }
                    ))
                    .await;
                let assistant_entry = json!({
                    "role": "assistant",
                    "kind": "answer",
                    "messageId": "",
                    "userId": context.self_id,
                    "sender": "Cain",
                    "text": reviewed_answer,
                    "time": Utc::now().to_rfc3339(),
                    "createdAt": Utc::now().to_rfc3339(),
                });
                self.state_store
                    .append_chat_session_entry(
                        &session_key,
                        assistant_entry,
                        self.config.answer.max_timeline_messages,
                    )
                    .await?;

                return Ok(ChatResult {
                    text: reviewed_answer,
                    notice: "low-information-reviewed".to_string(),
                    group_file_download_request: None,
                });
            }

            hallucination_retry_attempts += 1;
            self.logger
                .info(format!(
                    "幻觉检查打回主模型重答：attempt={}, feedback={}",
                    hallucination_retry_attempts,
                    if hallucination_review.feedback.trim().is_empty() {
                        "no-feedback"
                    } else {
                        hallucination_review.feedback.trim()
                    }
                ))
                .await;
            working_messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: Value::String(reviewed_answer),
            });
            working_messages.push(ChatMessage {
                role: "user".to_string(),
                content: Value::String(build_hallucination_retry_prompt(
                    hallucination_review.feedback.as_str(),
                    hallucination_review.reason.as_str(),
                )),
            });
        }
    }

    pub async fn render_markdown_image(&self, text: &str) -> Result<Option<String>> {
        render_reply_markdown_image(text).await
    }

    pub async fn should_suggest_reply(
        &self,
        context: &EventContext,
        event: &Value,
        summary: &str,
    ) -> Result<(bool, String)> {
        let session_key = self.build_session_key(context);
        self.state_store.refresh_chat_sessions_from_disk().await?;
        let session = self.state_store.get_chat_session(&session_key).await?;
        if session.last_hinted_message_id.trim()
            == event
                .get("message_id")
                .map(value_to_string)
                .unwrap_or_default()
        {
            return Ok((false, "already-hinted".to_string()));
        }

        let recent_context = self
            .build_timeline_from_messages(
                &session.messages,
                self.config.answer.context_window_messages.min(12),
            )
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
                    allow_expensive_fallback: false,
                },
            )
            .await?;
        let parsed = extract_json_object(&raw);
        Ok((
            parsed
                .get("should_prompt")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            parsed
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string(),
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
        self.state_store.refresh_chat_sessions_from_disk().await?;
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
                            self.build_timeline_from_messages(
                                &session.messages,
                                self.config.topic_closure.message_window
                            )
                            .await
                        )),
                    },
                ],
                CompleteOptions {
                    model: Some(self.config.topic_closure.model.clone()),
                    temperature: Some(self.config.topic_closure.temperature),
                    allow_expensive_fallback: false,
                },
            )
            .await?;
        let parsed = extract_json_object(&raw);
        let should_end = parsed
            .get("should_end")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let reason = parsed
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if should_end {
            self.state_store.clear_chat_session(&session_key).await?;
        }
        Ok((should_end, reason))
    }

    pub async fn disable_group_proactive_replies(
        &self,
        group_id: &str,
    ) -> Result<GroupPromptStatus> {
        self.runtime_config_store
            .set_qa_group_proactive_reply_enabled(group_id, false, &self.config.enabled_group_ids)
            .await?;
        Ok(self.get_group_prompt_status(group_id).await)
    }

    pub async fn update_filter_prompt(
        &self,
        group_id: &str,
        instruction: &str,
    ) -> Result<(String, String)> {
        self.review_and_persist_prompt(group_id, "filter", instruction)
            .await
    }

    pub async fn update_answer_prompt(
        &self,
        group_id: &str,
        instruction: &str,
    ) -> Result<(String, String)> {
        self.review_and_persist_prompt(group_id, "answer", instruction)
            .await
    }

    pub async fn maybe_capture_correction_memory(
        &self,
        context: &EventContext,
        event: &Value,
    ) -> Result<Option<String>> {
        if context.message_type != "group" {
            return Ok(None);
        }
        let raw_text = plain_text_from_message(
            event.get("message").unwrap_or(&Value::Null),
            event.get("raw_message").and_then(Value::as_str),
        );
        if !looks_like_correction_candidate(&raw_text) {
            return Ok(None);
        }
        let message_id = event
            .get("message_id")
            .map(value_to_string)
            .unwrap_or_default();
        let capture_key = format!(
            "{}:{}",
            self.build_session_key(context),
            if message_id.is_empty() {
                raw_text.chars().take(80).collect::<String>()
            } else {
                message_id.clone()
            }
        );
        {
            let mut checked = self.correction_memory_checked.lock().await;
            if checked.contains(&capture_key) {
                return Ok(None);
            }
            checked.insert(capture_key);
            if checked.len() > 2_000 {
                checked.clear();
            }
        }
        let Some(memory_file) = self.config.answer.memory_file.as_ref() else {
            return Ok(None);
        };
        self.state_store.refresh_chat_sessions_from_disk().await?;
        let session = self
            .state_store
            .get_chat_session(&self.build_session_key(context))
            .await?;
        if session.messages.len() < 2 {
            return Ok(None);
        }
        let recent_messages = if session.messages.len() > 18 {
            session.messages[session.messages.len() - 18..].to_vec()
        } else {
            session.messages.clone()
        };
        let mut recent_assistant_distance = usize::MAX;
        for index in (0..recent_messages.len().saturating_sub(1)).rev() {
            if recent_messages[index].get("role").and_then(Value::as_str) == Some("assistant") {
                recent_assistant_distance = recent_messages.len() - 1 - index;
                break;
            }
        }
        if recent_assistant_distance > 8 {
            return Ok(None);
        }
        let raw = self
            .chat_client
            .complete(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: Value::String([
                            "你负责从群聊上下文中判断：Cain 是否刚被群友纠正了一个适合写入长期记忆的事实错误。",
                            "只有当最近上下文里确实出现了 Cain 先回答错、随后群友给出更正事实时，should_append 才能为 true。",
                            "只保留可长期复用的稳定事实；不要记录闲聊、情绪、一次性事件、个人偏好、时间戳、用户名、群号。",
                            "特别排除时效性信息：版本号、release tag、最新版本、最新 release、commit hash 等随时会变的数据不应写入记忆。",
                            "如果当前消息只是补充讨论、玩笑、猜测，或无法确认 Cain 之前说错了，就返回 false。",
                            "输出必须是 JSON：{\"should_append\":boolean,\"memory\":\"简短事实句\",\"reason\":\"简短原因\"}。",
                            "memory 最多 40 字，不能为空；如果 should_append=false，则 memory 置空字符串。"
                        ].join("\n")),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: Value::String(format!(
                            "群号：{}\n当前消息发送者：{} ({})\n当前消息：{}\n\n最近聊天时间线：\n{}",
                            context.group_id,
                            get_sender_name(event),
                            context.user_id,
                            raw_text.trim(),
                            self.build_timeline_from_messages(&recent_messages, recent_messages.len()).await
                        )),
                    },
                ],
                CompleteOptions {
                    model: Some(self.config.filter.model.clone()),
                    temperature: Some(0.1),
                    allow_expensive_fallback: false,
                },
            )
            .await?;
        let parsed = extract_json_object(&raw);
        let should_append = parsed
            .get("should_append")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let memory = parsed
            .get("memory")
            .or_else(|| parsed.get("entry"))
            .or_else(|| parsed.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let reason = parsed
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if !should_append || memory.is_empty() {
            return Ok(None);
        }
        let appended = append_memory_entry(memory_file, &memory).await?;
        self.logger
            .info(format!(
                "长期记忆{}：{}{}",
                if appended { "已新增" } else { "已存在" },
                memory,
                if reason.is_empty() {
                    String::new()
                } else {
                    format!(" ({reason})")
                }
            ))
            .await;
        Ok(Some(memory))
    }

    pub async fn review_low_information_reply(
        &self,
        source_text: &str,
        reply_text: &str,
        on_low_information: &str,
    ) -> Result<LowInformationReplyReview> {
        let normalized_reply = reply_text.trim();
        if normalized_reply.is_empty() {
            return Ok(LowInformationReplyReview {
                reason: "empty-reply".to_string(),
                ..Default::default()
            });
        }
        let normalized_source = source_text.trim();
        if normalized_source.is_empty() {
            return Ok(LowInformationReplyReview {
                text: normalized_reply.to_string(),
                ..Default::default()
            });
        }
        let raw = self
            .chat_client
            .complete(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: Value::String([
                            "你是聊天回复质检器，只判断这条回复该不该发出去。",
                            "如果回复只是把用户问题换词重复、空泛复述、没有新增信息、没有具体定位、没有实际帮助，就判定 allow=false。",
                            "如果回复给出了具体做法、具体定位、明确结论、有效下一步，判定 allow=true。",
                            "当用户在问“怎么改/怎么做/在哪里/哪个字段”时，像“改对应字段”“看对应对象”“去改相关配置”这类话都算低信息空话。",
                            "像“需要查文档再确认”“请提供更多上下文/配置名称我才能定位”“还没能读取对应文件/JSON，因此不敢确定”“收到，先读取某文件”这类把工作往后推、但没有给出读取结果的回复，一律判定 allow=false。",
                            "如果这类问题本来就应该先读文件或调工具确认，而拟发送回复里既没有真实读取结果，也没有具体字段/路径/对象名/版本结论，也一律 allow=false。",
                            "如果用户原话本身是要安装包、jar、zip、apk、客户端、最新版文件、release 资产、插件包、服务器插件，而拟发送回复只是“帮你交给下载流程”“等我给你找文件”“我去走下载流程”这种口头承诺但没有真实调用，那么应判定 allow=false，并设置 start_group_file_download=true。",
                            "出现 start_group_file_download=true 时，request_text 默认填写用户原话；除非用户原话缺关键信息且你能更精确重写，否则不要改写。",
                            "只输出 JSON：{\"allow\":boolean,\"fallback\":\"可选的替代短句\",\"reason\":\"简短原因\",\"start_group_file_download\":boolean,\"request_text\":\"可选，默认用用户原话\"}",
                            "fallback 只在 allow=false 且需要替代短句时填写，否则留空。",
                            "如果当前模式是 fallback，并且这条回复属于“先去查文档/先去读文件”的空话，fallback 应改成一句更硬的纠偏短句，明确要求先读取对应文件或工具结果后再回答，不要复述原空话。"
                        ].join("\n")),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: Value::String(format!(
                            "用户原话：{}\n拟发送回复：{}\n低信息时的处理模式：{}",
                            normalized_source,
                            normalized_reply,
                            if on_low_information == "fallback" { "fallback" } else { "suppress" }
                        )),
                    },
                ],
                CompleteOptions {
                    model: Some(self.config.low_information_filter_model.clone()),
                    temperature: Some(0.1),
                    allow_expensive_fallback: false,
                },
            )
            .await;
        let raw = match raw {
            Ok(raw) => raw,
            Err(error) => {
                self.logger
                    .warn(format!("低信息回复判定失败，回退为原回复：{error:#}"))
                    .await;
                return Ok(LowInformationReplyReview {
                    text: normalized_reply.to_string(),
                    reason: "filter-error".to_string(),
                    ..Default::default()
                });
            }
        };
        let parsed = extract_json_object(&raw);
        let allow = parsed.get("allow").and_then(Value::as_bool).unwrap_or(true);
        let reason = parsed
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let start_group_file_download = parsed
            .get("start_group_file_download")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let request_text = parsed
            .get("request_text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if allow {
            return Ok(LowInformationReplyReview {
                text: normalized_reply.to_string(),
                reason,
                ..Default::default()
            });
        }
        self.logger
            .info(format!(
                "已拦截低信息回复：{} | source={} | reply={}",
                if reason.is_empty() {
                    "no-reason"
                } else {
                    reason.as_str()
                },
                normalized_source.chars().take(80).collect::<String>(),
                normalized_reply.chars().take(80).collect::<String>()
            ))
            .await;
        if start_group_file_download {
            return Ok(LowInformationReplyReview {
                start_group_file_download: true,
                request_text: if request_text.is_empty() {
                    normalized_source.to_string()
                } else {
                    request_text
                },
                reason,
                ..Default::default()
            });
        }
        if on_low_information == "fallback" {
            return Ok(LowInformationReplyReview {
                text: build_low_information_fallback(normalized_source, normalized_reply),
                reason,
                ..Default::default()
            });
        }
        Ok(LowInformationReplyReview {
            reason,
            ..Default::default()
        })
    }

    pub async fn review_hallucination_reply(
        &self,
        source_text: &str,
        reply_text: &str,
    ) -> Result<HallucinationReplyReview> {
        if !self.config.hallucination_check.enabled {
            self.logger.info("幻觉检查已关闭，跳过。").await;
            return Ok(HallucinationReplyReview {
                approved: true,
                reason: "checker-disabled".to_string(),
                ..Default::default()
            });
        }

        let normalized_reply = reply_text.trim();
        if normalized_reply.is_empty() {
            return Ok(HallucinationReplyReview {
                approved: false,
                reason: "empty-reply".to_string(),
                ..Default::default()
            });
        }
        let normalized_source = source_text.trim();
        if normalized_source.is_empty() {
            return Ok(HallucinationReplyReview {
                approved: true,
                reason: "empty-source".to_string(),
                ..Default::default()
            });
        }

        let raw = self
            .chat_client
            .complete(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: Value::String([
                            "你是 Cain 的回答事实校对器。",
                            "你的任务是检查候选回答是否存在幻觉（即声称了上下文中没有依据的具体事实）。",
                            "幻觉包括：声称某文件/字段/版本/release 存在但上下文中未确认、给出和上下文矛盾的数值或路径、凭空编造不在上下文中的具体细节。",
                            "如果候选回答已经足够稳妥或只是闲聊/观点，直接 approved=true，不要为了改而改。",
                            "只输出 JSON：{\"approved\":boolean,\"feedback\":\"如果有幻觉，简短说明哪里不对，最多80字；没有则留空\",\"reason\":\"简短原因\"}",
                            "不要使用 Markdown。"
                        ].join("\n")),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: Value::String(format!(
                            "用户原话：{}\n候选回答：{}",
                            normalized_source, normalized_reply
                        )),
                    },
                ],
                CompleteOptions {
                    model: Some(self.config.hallucination_check.model.clone()),
                    temperature: Some(self.config.hallucination_check.temperature),
                    allow_expensive_fallback: false,
                },
            )
            .await;
        let raw = match raw {
            Ok(raw) => raw,
            Err(error) => {
                self.logger
                    .warn(format!("幻觉检查失败，回退原回答：{error:#}"))
                    .await;
                return Ok(HallucinationReplyReview {
                    approved: true,
                    reason: "checker-error".to_string(),
                    ..Default::default()
                });
            }
        };

        let parsed = extract_json_object(&raw);
        let approved = parsed
            .get("approved")
            .or_else(|| parsed.get("allow"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let feedback = parsed
            .get("feedback")
            .or_else(|| parsed.get("fallback"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let reason = parsed
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        Ok(HallucinationReplyReview {
            approved,
            feedback,
            reason,
        })
    }

    async fn build_answer_system_prompt(&self, context: &EventContext) -> String {
        let override_entry = if context.message_type == "group" {
            self.runtime_config_store
                .get_group_qa_override(&context.group_id)
                .await
        } else {
            None
        };
        let base_prompt = override_entry
            .as_ref()
            .map(|item| item.answer_prompt.as_str())
            .filter(|item| !item.trim().is_empty())
            .unwrap_or(self.config.answer.system_prompt.as_str())
            .trim()
            .to_string();
        let mut parts = vec![base_prompt];
        parts.push(
            [
                "回复要求：",
                "- 先给结论，再给可执行步骤。",
                "- 如果缺少关键上下文，明确说出还缺哪一个具体信息。",
                "- 禁止只说“我去查一下/稍后再说”的空话。",
                "- 默认使用简体中文，不要使用 Markdown。",
            ]
            .join("\n"),
        );
        if let Some(memory_prompt) = self.load_long_term_memory_prompt().await {
            parts.push(memory_prompt);
        }
        parts
            .into_iter()
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    async fn load_long_term_memory_prompt(&self) -> Option<String> {
        let memory_path = self.config.answer.memory_file.as_ref()?;
        let raw = match fs::read_to_string(memory_path).await {
            Ok(text) => text,
            Err(_) => return None,
        };
        let lines = raw
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        if lines.is_empty() {
            return None;
        }
        let selected = if lines.len() > 20 {
            &lines[lines.len() - 20..]
        } else {
            &lines[..]
        };
        Some(format!(
            "以下是长期记忆（仅用于提高一致性，遇到冲突以当前上下文为准）：\n{}",
            selected.join("\n")
        ))
    }

    async fn build_timeline_from_messages(
        &self,
        messages: &[Value],
        max_messages: usize,
    ) -> String {
        let items = if messages.len() > max_messages {
            &messages[messages.len() - max_messages..]
        } else {
            messages
        };
        if items.is_empty() {
            return "(暂无共享上下文)".to_string();
        }
        items
            .iter()
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
                let text = item
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("(空消息)")
                    .trim();
                format!(
                    "{}. [{}] {}：{}",
                    index + 1,
                    item.get("time")
                        .or_else(|| item.get("createdAt"))
                        .and_then(Value::as_str)
                        .unwrap_or("-"),
                    speaker,
                    text
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn review_and_persist_prompt(
        &self,
        group_id: &str,
        prompt_type: &str,
        instruction: &str,
    ) -> Result<(String, String)> {
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
                            if prompt_type == "filter" {
                                "过滤 prompt"
                            } else {
                                "聊天 prompt"
                            },
                            current_prompt,
                            instruction.trim()
                        )),
                    },
                ],
                CompleteOptions {
                    model: Some(self.config.prompt_review.model.clone()),
                    temperature: Some(0.2),
                    allow_expensive_fallback: false,
                },
            )
            .await?;
        let parsed = extract_json_object(&raw);
        if !parsed
            .get("approved")
            .and_then(Value::as_bool)
            .unwrap_or(true)
        {
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
            .filter(|item: &&str| !item.is_empty())
            .ok_or_else(|| anyhow::anyhow!("prompt 审核未返回有效 prompt"))?
            .to_string();
        let existing = self
            .runtime_config_store
            .get_group_qa_override(group_id)
            .await;
        self.runtime_config_store
            .set_group_qa_override(GroupQaOverride {
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
                created_at: existing
                    .as_ref()
                    .map(|item| item.created_at.clone())
                    .unwrap_or_default(),
                updated_at: String::new(),
            })
            .await?;
        Ok((
            prompt,
            parsed
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string(),
        ))
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
            if depth == 0
                && let Some(start) = start_index
                && let Ok(value) = serde_json::from_str::<Value>(&source[start..=index])
            {
                return value;
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

fn looks_like_correction_candidate(text: &str) -> bool {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.len() < 4 {
        return false;
    }
    if [
        "说错",
        "讲错",
        "不对",
        "不是",
        "纠正",
        "更正",
        "其实",
        "应该",
        "应为",
        "而是",
        "正确",
        "是指",
        "指的是",
    ]
    .iter()
    .any(|pattern| normalized.contains(pattern))
    {
        return true;
    }
    let lowercase = normalized.to_lowercase();
    [
        "倍速",
        "单位生产",
        "工厂不会加速",
        "原版",
        "x端",
        "mindustryx",
        "release",
        "tag",
        "版本",
        "pc",
        "电脑版",
        "桌面版",
        "apk",
        "jar",
        "exe",
    ]
    .iter()
    .any(|pattern| lowercase.contains(pattern))
}

fn build_low_information_fallback(source_text: &str, reply_text: &str) -> String {
    let combined = format!("{}\n{}", source_text.trim(), reply_text.trim()).to_lowercase();
    if [
        "mindustry",
        "mindustryx",
        "mdt",
        "牡丹亭",
        "datapatch",
        "方块",
        "建筑",
        "炮塔",
        "单位",
        "物品",
        "液体",
        "状态",
        "星球",
        "天气",
        "字段",
        "超速",
        "投影",
        "穹顶",
    ]
    .iter()
    .any(|pattern| combined.contains(pattern))
    {
        return "还没定位到具体字段。".to_string();
    }
    if [
        "模组",
        "mod",
        "插件",
        "脚本",
        "源码",
        "仓库",
        "项目",
        "目录",
        "构建",
        "编译",
        "报错",
        "服务端",
        "服务器",
    ]
    .iter()
    .any(|pattern| combined.contains(pattern))
    {
        return "还没定位到具体位置。".to_string();
    }
    "还没定位到具体答案。".to_string()
}

fn build_low_information_retry_prompt(feedback: &str) -> String {
    let normalized_feedback = feedback.trim();
    [
        if normalized_feedback.is_empty() {
            "系统质检纠偏：你上一条回答被低信息质检器打回。".to_string()
        } else {
            format!(
                "系统质检纠偏：你上一条回答被低信息质检器打回，原因：{}",
                normalized_feedback
            )
        },
        "不要复述问题，不要说“还没定位到”“我先去查”“需要更多上下文”“请先读取文件”等空话。".to_string(),
        "下一条回答必须直接给出当前上下文里已经能确认的具体字段、路径、对象名、数值、版本结论或可执行步骤。".to_string(),
        "如果当前上下文仍不足以确认这些具体信息，就只保留已经能确认的部分，不要输出泛泛结论。".to_string(),
    ]
    .join("\n")
}

fn build_hallucination_retry_prompt(feedback: &str, reason: &str) -> String {
    let normalized_feedback = feedback.trim();
    let normalized_reason = reason.trim();
    [
        if normalized_feedback.is_empty() && normalized_reason.is_empty() {
            "系统校对纠偏：你上一条回答被事实校对器打回。".to_string()
        } else if normalized_feedback.is_empty() {
            format!(
                "系统校对纠偏：你上一条回答被事实校对器打回，原因：{}",
                normalized_reason
            )
        } else {
            format!(
                "系统校对纠偏：你上一条回答被事实校对器打回，原因：{}",
                normalized_feedback
            )
        },
        "请基于已有上下文重新组织回答，去掉无法确认的具体事实。".to_string(),
        "如果确实不确定，就明确说不确定，不要编造字段、版本号、路径、数值。".to_string(),
    ]
    .join("\n")
}

fn looks_like_group_file_download_request(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    if normalized.is_empty() {
        return false;
    }
    let request_like = [
        "下载",
        "发一下",
        "发个",
        "来个",
        "来一份",
        "求",
        "求发",
        "能发",
        "给我",
        "有没有",
    ]
    .iter()
    .any(|item| normalized.contains(item));
    let artifact_like = [
        "release",
        "asset",
        "最新版",
        "latest",
        "apk",
        "jar",
        "zip",
        "客户端",
        "安装包",
        "服务端",
        "server",
        "desktop",
    ]
    .iter()
    .any(|item| normalized.contains(item));
    if request_like && artifact_like {
        return true;
    }

    let has_commit = normalized
        .split_whitespace()
        .map(|token| token.trim_matches(|ch: char| !ch.is_ascii_hexdigit()))
        .any(|token| {
            (7..=40).contains(&token.len()) && token.chars().all(|ch| ch.is_ascii_hexdigit())
        });
    let build_like = ["commit", "hash", "sha", "提交", "构建", "编译", "build"]
        .iter()
        .any(|item| normalized.contains(item));
    has_commit && build_like
}

fn parse_group_file_download_handoff(raw: &str) -> Option<GroupFileDownloadRequest> {
    let parsed = extract_json_object(raw);
    if parsed
        .get("start_group_file_download")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return None;
    }
    let request_text = parsed
        .get("request_text")
        .or_else(|| parsed.get("requestText"))
        .or_else(|| parsed.get("query"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let request = parsed
        .get("request")
        .cloned()
        .unwrap_or_else(|| json!({ "request_text": request_text }));
    let normalized_request_text = if request_text.is_empty() {
        request
            .get("request_text")
            .or_else(|| request.get("text"))
            .or_else(|| request.get("query"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string()
    } else {
        request_text
    };
    if normalized_request_text.is_empty() {
        return None;
    }
    Some(GroupFileDownloadRequest {
        request_text: normalized_request_text,
        request,
    })
}

async fn append_memory_entry(path: &Path, entry: &str) -> Result<bool> {
    let normalized_entry = entry.trim();
    if normalized_entry.is_empty() {
        bail!("memory entry 不能为空");
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let existing = match fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    if existing
        .lines()
        .map(str::trim)
        .any(|line| !line.is_empty() && line == normalized_entry)
    {
        return Ok(false);
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(normalized_entry);
    next.push('\n');
    fs::write(path, next).await?;
    Ok(true)
}
