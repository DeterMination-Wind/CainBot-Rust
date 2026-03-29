use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
use crate::qa_session_worker::QaSessionWorker;
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
    project_root: PathBuf,
    config_path: PathBuf,
    logger: Logger,
    worker: Arc<Mutex<Option<QaSessionWorker>>>,
    active_requests: Arc<AtomicUsize>,
    last_used_ms: Arc<AtomicU64>,
    idle_timeout_ms: u64,
}

impl ChatSessionManager {
    pub async fn start(
        project_root: &Path,
        config_path: &Path,
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
            project_root: project_root.to_path_buf(),
            config_path: config_path.to_path_buf(),
            logger,
            worker: Arc::new(Mutex::new(None)),
            active_requests: Arc::new(AtomicUsize::new(0)),
            last_used_ms: Arc::new(AtomicU64::new(now_ms())),
            idle_timeout_ms: 5 * 60 * 1000,
        };
        manager.spawn_idle_reaper();
        Ok(manager)
    }

    pub async fn stop(&self) -> Result<()> {
        if let Some(worker) = self.worker.lock().await.take() {
            worker.stop().await?;
        }
        Ok(())
    }

    fn spawn_idle_reaper(&self) {
        let worker = self.worker.clone();
        let logger = self.logger.clone();
        let active_requests = self.active_requests.clone();
        let last_used_ms = self.last_used_ms.clone();
        let idle_timeout_ms = self.idle_timeout_ms;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                if active_requests.load(Ordering::SeqCst) != 0 {
                    continue;
                }
                let idle_for_ms = now_ms().saturating_sub(last_used_ms.load(Ordering::SeqCst));
                if idle_for_ms < idle_timeout_ms {
                    continue;
                }
                let maybe_worker = worker.lock().await.take();
                if let Some(worker) = maybe_worker {
                    logger
                        .info(format!(
                            "QA session worker 空闲超时，已回收常驻进程：idleMs={idle_for_ms}"
                        ))
                        .await;
                    let _ = worker.stop().await;
                }
            }
        });
    }

    async fn request(&self, action: &str, payload: Value) -> Result<Value> {
        self.active_requests.fetch_add(1, Ordering::SeqCst);
        self.last_used_ms.store(now_ms(), Ordering::SeqCst);
        let worker = self.ensure_worker().await?;
        let result = worker.request(action, payload).await;
        self.last_used_ms.store(now_ms(), Ordering::SeqCst);
        self.active_requests.fetch_sub(1, Ordering::SeqCst);
        result
    }

    async fn ensure_worker(&self) -> Result<QaSessionWorker> {
        let mut guard = self.worker.lock().await;
        if let Some(worker) = guard.as_ref() {
            return Ok(worker.clone());
        }
        let worker = QaSessionWorker::start(&self.project_root, &self.config_path, self.logger.clone()).await?;
        *guard = Some(worker.clone());
        Ok(worker)
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
            .append_chat_session_entry(&session_key, entry, self.config.answer.max_timeline_messages)
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

    pub async fn chat(&self, context: &EventContext, input: &ChatInput) -> Result<ChatResult> {
        let payload = self
            .request(
                "chat",
                json!({
                    "context": event_context_json(context),
                    "input": chat_input_json(input),
                }),
            )
            .await?;
        Ok(ChatResult {
            text: payload.get("text").and_then(Value::as_str).unwrap_or_default().trim().to_string(),
            notice: payload.get("notice").and_then(Value::as_str).unwrap_or_default().trim().to_string(),
            group_file_download_request: payload
                .get("groupFileDownloadRequest")
                .and_then(Value::as_object)
                .map(|request| GroupFileDownloadRequest {
                    request_text: request
                        .get("requestText")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                    request: request.get("request").cloned().unwrap_or(Value::Null),
                })
                .filter(|request| !request.request_text.is_empty()),
        })
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
        Ok((
            should_end,
            reason,
        ))
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
        let message_id = event.get("message_id").map(value_to_string).unwrap_or_default();
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
        let session = self.state_store.get_chat_session(&self.build_session_key(context)).await?;
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
                },
            )
            .await?;
        let parsed = extract_json_object(&raw);
        let should_append = parsed.get("should_append").and_then(Value::as_bool).unwrap_or(false);
        let memory = parsed
            .get("memory")
            .or_else(|| parsed.get("entry"))
            .or_else(|| parsed.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let reason = parsed.get("reason").and_then(Value::as_str).unwrap_or_default().trim().to_string();
        if !should_append || memory.is_empty() {
            return Ok(None);
        }
        let appended = append_memory_entry(memory_file, &memory).await?;
        self.logger
            .info(format!(
                "长期记忆{}：{}{}",
                if appended { "已新增" } else { "已存在" },
                memory,
                if reason.is_empty() { String::new() } else { format!(" ({reason})") }
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
        let reason = parsed.get("reason").and_then(Value::as_str).unwrap_or_default().trim().to_string();
        let start_group_file_download = parsed.get("start_group_file_download").and_then(Value::as_bool).unwrap_or(false);
        let request_text = parsed.get("request_text").and_then(Value::as_str).unwrap_or_default().trim().to_string();
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
                if reason.is_empty() { "no-reason" } else { reason.as_str() },
                normalized_source.chars().take(80).collect::<String>(),
                normalized_reply.chars().take(80).collect::<String>()
            ))
            .await;
        if start_group_file_download {
            return Ok(LowInformationReplyReview {
                start_group_file_download: true,
                request_text: if request_text.is_empty() { normalized_source.to_string() } else { request_text },
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
            .filter(|item: &&str| !item.is_empty())
            .ok_or_else(|| anyhow::anyhow!("prompt 审核未返回有效 prompt"))?
            .to_string();
        let existing = self.runtime_config_store.get_group_qa_override(group_id).await;
        self.runtime_config_store
            .set_group_qa_override(GroupQaOverride {
                group_id: group_id.trim().to_string(),
                filter_prompt: if prompt_type == "filter" {
                    prompt.clone()
                } else {
                    existing.as_ref().map(|item| item.filter_prompt.clone()).unwrap_or_default()
                },
                answer_prompt: if prompt_type == "answer" {
                    prompt.clone()
                } else {
                    existing.as_ref().map(|item| item.answer_prompt.clone()).unwrap_or_default()
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
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn event_context_json(context: &EventContext) -> Value {
    json!({
        "messageType": context.message_type,
        "groupId": context.group_id,
        "userId": context.user_id,
        "selfId": context.self_id,
    })
}

fn chat_input_json(input: &ChatInput) -> Value {
    json!({
        "text": input.text,
        "images": input.images,
        "historyText": input.history_text,
        "runtimeContext": {
            "senderName": input.runtime_context.sender_name,
            "timelineText": input.runtime_context.timeline_text,
            "currentMessageId": input.runtime_context.current_message_id,
            "currentTime": input.runtime_context.current_time,
        }
    })
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
        "说错", "讲错", "不对", "不是", "纠正", "更正", "其实", "应该", "应为", "而是", "正确",
        "是指", "指的是",
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
