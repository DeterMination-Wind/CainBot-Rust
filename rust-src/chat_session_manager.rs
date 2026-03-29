use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::event_utils::EventContext;
use crate::logger::Logger;
use crate::message_input::ChatInput;
use crate::qa_session_worker::QaSessionWorker;

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
    worker: QaSessionWorker,
    logger: Logger,
}

impl ChatSessionManager {
    pub async fn start(project_root: &Path, config_path: &Path, logger: Logger) -> Result<Self> {
        Ok(Self {
            worker: QaSessionWorker::start(project_root, config_path, logger.clone()).await?,
            logger,
        })
    }

    pub async fn stop(&self) -> Result<()> {
        self.worker.stop().await
    }

    pub async fn is_group_enabled(&self, group_id: &str) -> bool {
        match self
            .worker
            .request("is_group_enabled", json!({ "groupId": group_id.trim() }))
            .await
        {
            Ok(payload) => payload.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            Err(error) => {
                self.logger
                    .warn(format!("查询群问答开关失败，回退为关闭：{error:#}"))
                    .await;
                false
            }
        }
    }

    pub async fn is_group_proactive_reply_enabled(&self, group_id: &str) -> bool {
        match self
            .worker
            .request(
                "is_group_proactive_reply_enabled",
                json!({ "groupId": group_id.trim() }),
            )
            .await
        {
            Ok(payload) => payload.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            Err(error) => {
                self.logger
                    .warn(format!("查询群主动回复开关失败，回退为关闭：{error:#}"))
                    .await;
                false
            }
        }
    }

    pub async fn get_group_prompt_status(&self, group_id: &str) -> GroupPromptStatus {
        match self
            .worker
            .request("get_group_prompt_status", json!({ "groupId": group_id.trim() }))
            .await
        {
            Ok(payload) => parse_group_prompt_status(&payload),
            Err(error) => {
                self.logger
                    .warn(format!("读取群提示词状态失败，回退为空状态：{error:#}"))
                    .await;
                GroupPromptStatus {
                    enabled: false,
                    proactive_reply_enabled: false,
                    filter_heartbeat_enabled: false,
                    filter_heartbeat_interval: 10,
                    file_download_enabled: false,
                    file_download_folder_name: String::new(),
                    filter_prompt: String::new(),
                    answer_prompt: String::new(),
                }
            }
        }
    }

    pub async fn should_run_group_proactive_filter(&self, group_id: &str) -> (bool, u64, u64) {
        match self
            .worker
            .request(
                "should_run_group_proactive_filter",
                json!({ "groupId": group_id.trim() }),
            )
            .await
        {
            Ok(payload) => (
                payload.get("allowed").and_then(Value::as_bool).unwrap_or(false),
                payload.get("counter").and_then(Value::as_u64).unwrap_or_default(),
                payload.get("interval").and_then(Value::as_u64).unwrap_or(1),
            ),
            Err(error) => {
                self.logger
                    .warn(format!("执行主动回复心跳过滤失败，回退为不触发：{error:#}"))
                    .await;
                (false, 0, 1)
            }
        }
    }

    pub async fn reset_group_filter_heartbeat(&self, group_id: &str) {
        let _ = self
            .worker
            .request(
                "reset_group_filter_heartbeat",
                json!({ "groupId": group_id.trim() }),
            )
            .await;
    }

    // 会话、工具循环和纠错记忆都交给隔离 worker，Rust 主循环只保留路由和副作用发送。
    pub async fn record_incoming_message(
        &self,
        context: &EventContext,
        event: &Value,
        summary: &str,
    ) -> Result<()> {
        let _ = self
            .worker
            .request(
                "record_incoming_message",
                json!({
                    "context": event_context_json(context),
                    "event": event,
                    "summary": summary.trim(),
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn mark_hinted(&self, context: &EventContext, message_id: &str) -> Result<()> {
        let _ = self
            .worker
            .request(
                "mark_hinted",
                json!({
                    "context": event_context_json(context),
                    "messageId": message_id.trim(),
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn chat(&self, context: &EventContext, input: &ChatInput) -> Result<ChatResult> {
        let payload = self
            .worker
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
        _summary: &str,
    ) -> Result<(bool, String)> {
        let payload = self
            .worker
            .request(
                "should_suggest_reply",
                json!({
                    "context": event_context_json(context),
                    "event": event,
                }),
            )
            .await?;
        Ok((
            payload.get("shouldPrompt").and_then(Value::as_bool).unwrap_or(false),
            payload.get("reason").and_then(Value::as_str).unwrap_or_default().trim().to_string(),
        ))
    }

    pub async fn maybe_close_group_topic(&self, group_id: &str) -> Result<(bool, String)> {
        let payload = self
            .worker
            .request("maybe_close_group_topic", json!({ "groupId": group_id.trim() }))
            .await?;
        Ok((
            payload.get("shouldEnd").and_then(Value::as_bool).unwrap_or(false),
            payload.get("reason").and_then(Value::as_str).unwrap_or_default().trim().to_string(),
        ))
    }

    pub async fn disable_group_proactive_replies(&self, group_id: &str) -> Result<GroupPromptStatus> {
        let payload = self
            .worker
            .request(
                "disable_group_proactive_replies",
                json!({ "groupId": group_id.trim() }),
            )
            .await?;
        Ok(parse_group_prompt_status(&payload))
    }

    pub async fn update_filter_prompt(&self, group_id: &str, instruction: &str) -> Result<(String, String)> {
        self.update_prompt("update_filter_prompt", group_id, instruction).await
    }

    pub async fn update_answer_prompt(&self, group_id: &str, instruction: &str) -> Result<(String, String)> {
        self.update_prompt("update_answer_prompt", group_id, instruction).await
    }

    pub async fn maybe_capture_correction_memory(
        &self,
        context: &EventContext,
        event: &Value,
    ) -> Result<Option<String>> {
        let payload = self
            .worker
            .request(
                "maybe_capture_correction_memory",
                json!({
                    "context": event_context_json(context),
                    "event": event,
                }),
            )
            .await?;
        if payload.is_null() {
            return Ok(None);
        }
        Ok(Some(
            payload
                .get("memory")
                .or_else(|| payload.get("entry"))
                .or_else(|| payload.get("content"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string(),
        )
        .filter(|item| !item.is_empty()))
    }

    pub async fn review_low_information_reply(
        &self,
        source_text: &str,
        reply_text: &str,
        on_low_information: &str,
    ) -> Result<LowInformationReplyReview> {
        let payload = self
            .worker
            .request(
                "review_low_information_reply",
                json!({
                    "sourceText": source_text.trim(),
                    "replyText": reply_text.trim(),
                    "onLowInformation": on_low_information.trim(),
                }),
            )
            .await?;
        Ok(LowInformationReplyReview {
            text: payload.get("text").and_then(Value::as_str).unwrap_or_default().trim().to_string(),
            start_group_file_download: payload
                .get("startGroupFileDownload")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            request_text: payload
                .get("requestText")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string(),
            reason: payload
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string(),
        })
    }

    async fn update_prompt(&self, action: &str, group_id: &str, instruction: &str) -> Result<(String, String)> {
        let payload = self
            .worker
            .request(
                action,
                json!({
                    "groupId": group_id.trim(),
                    "instruction": instruction.trim(),
                }),
            )
            .await?;
        Ok((
            payload.get("prompt").and_then(Value::as_str).unwrap_or_default().trim().to_string(),
            payload.get("reason").and_then(Value::as_str).unwrap_or_default().trim().to_string(),
        ))
    }
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

fn parse_group_prompt_status(payload: &Value) -> GroupPromptStatus {
    GroupPromptStatus {
        enabled: payload.get("enabled").and_then(Value::as_bool).unwrap_or(false),
        proactive_reply_enabled: payload
            .get("proactiveReplyEnabled")
            .or_else(|| payload.get("proactive_reply_enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        filter_heartbeat_enabled: payload
            .get("filterHeartbeatEnabled")
            .or_else(|| payload.get("filter_heartbeat_enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        filter_heartbeat_interval: payload
            .get("filterHeartbeatInterval")
            .or_else(|| payload.get("filter_heartbeat_interval"))
            .and_then(Value::as_u64)
            .unwrap_or(10),
        file_download_enabled: payload
            .get("fileDownloadEnabled")
            .or_else(|| payload.get("file_download_enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        file_download_folder_name: payload
            .get("fileDownloadFolderName")
            .or_else(|| payload.get("file_download_folder_name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        filter_prompt: payload
            .get("filterPrompt")
            .or_else(|| payload.get("filter_prompt"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        answer_prompt: payload
            .get("answerPrompt")
            .or_else(|| payload.get("answer_prompt"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}
