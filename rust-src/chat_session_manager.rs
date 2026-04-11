use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{Local, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::task;
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

const TOOL_REQUEST_START: &str = "<<<CAIN_CODEX_TOOL_START>>>";
const TOOL_REQUEST_END: &str = "<<<CAIN_CODEX_TOOL_END>>>";
const CHAT_REPAIR_TOOL_ROUND_LIMIT: usize = 10;
const DEFAULT_GITHUB_API_BASE_URL: &str = "https://api.github.com";
const CODEX_MAX_DIRECTORY_ENTRIES: usize = 200;
const CODEX_MAX_SEARCH_RESULTS: usize = 60;
const CODEX_MAX_FILE_CHARS: usize = 20_000;
const CODEX_MAX_FILE_LINES: usize = 400;
const CODEX_MAX_PROJECT_HINT_CHARS: usize = 12_000;
const MAX_GITHUB_RELEASES: usize = 30;
const MAX_GITHUB_COMMITS: usize = 100;

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
enum ToolDrivenCompletion {
    Text(String),
    GroupFileDownload(GroupFileDownloadRequest),
}

#[derive(Debug, Clone)]
enum ChatToolExecution {
    Value(Value),
    GroupFileDownload(GroupFileDownloadRequest),
}

#[derive(Debug, Clone, Deserialize, Default)]
struct GithubReleaseToolPayload {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    target_commitish: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    published_at: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    assets: Vec<GithubReleaseToolAsset>,
    #[serde(default)]
    author: GithubReleaseToolAuthor,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct GithubReleaseToolAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    browser_download_url: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct GithubReleaseToolAuthor {
    #[serde(default)]
    login: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct GithubCommitToolPayload {
    #[serde(default)]
    sha: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    commit: GithubCommitToolMeta,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct GithubCommitToolMeta {
    #[serde(default)]
    message: String,
    author: Option<GithubCommitToolAuthor>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct GithubCommitToolAuthor {
    #[serde(default)]
    name: String,
    #[serde(default)]
    date: String,
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
        on_low_information: &str,
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
        let mut low_information_retry_attempts = 0usize;
        let mut hallucination_retry_attempts = 0usize;
        let mut tool_round_limit = self.config.answer.max_tool_rounds.max(1);

        loop {
            let completion = self
                .complete_chat_turn_with_tools(
                    context,
                    &mut working_messages,
                    tool_round_limit,
                )
                .await?;
            let answer_text = match completion {
                ToolDrivenCompletion::Text(text) => text,
                ToolDrivenCompletion::GroupFileDownload(handoff) => {
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
            };
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
                tool_round_limit = CHAT_REPAIR_TOOL_ROUND_LIMIT;
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
            tool_round_limit = CHAT_REPAIR_TOOL_ROUND_LIMIT;
        }
    }

    pub async fn render_markdown_image(&self, text: &str) -> Result<Option<String>> {
        render_reply_markdown_image(text).await
    }

    async fn complete_chat_turn_with_tools(
        &self,
        context: &EventContext,
        working_messages: &mut Vec<ChatMessage>,
        max_tool_rounds: usize,
    ) -> Result<ToolDrivenCompletion> {
        let tool_limit = max_tool_rounds.max(1);
        let mut used_tool_calls = 0usize;
        let mut forced_text_attempts = 0usize;

        loop {
            let completion = self
                .chat_client
                .complete(
                    working_messages,
                    CompleteOptions {
                        model: Some(self.config.answer.model.clone()),
                        temperature: Some(self.config.answer.temperature),
                    },
                )
                .await?;

            if let Some(handoff) = parse_group_file_download_handoff(&completion) {
                if context.message_type == "group" {
                    return Ok(ToolDrivenCompletion::GroupFileDownload(handoff));
                }
                working_messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: Value::String(completion),
                });
                working_messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: Value::String(
                        "文件下载流程只支持群聊。不要再调用下载工具，直接回复用户让他去对应群里 @ Cain。"
                            .to_string(),
                    ),
                });
                continue;
            }

            let tool_calls = parse_tool_calls(&completion);
            if tool_calls.is_empty() {
                return Ok(ToolDrivenCompletion::Text(completion.trim().to_string()));
            }

            if used_tool_calls >= tool_limit {
                forced_text_attempts += 1;
                self.logger
                    .info(format!(
                        "聊天工具额度已用完，强制主模型直接输出文本：attempt={}, limit={}",
                        forced_text_attempts, tool_limit
                    ))
                    .await;
                if forced_text_attempts >= 3 {
                    return Ok(ToolDrivenCompletion::Text(
                        strip_marked_tool_calls(&completion).trim().to_string(),
                    ));
                }
                working_messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: Value::String(completion),
                });
                working_messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: Value::String(build_tool_limit_retry_prompt(tool_limit)),
                });
                continue;
            }

            forced_text_attempts = 0;
            let remaining = tool_limit.saturating_sub(used_tool_calls);
            let tool_call_count = tool_calls.len();
            let mut tool_results = Vec::<Value>::new();

            for request in tool_calls.into_iter().take(remaining) {
                let tool_name = request
                    .get("tool")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .unwrap_or("unknown")
                    .to_string();
                self.logger
                    .info(format!(
                        "聊天工具调用：round={}/{}, tool={}",
                        used_tool_calls + 1,
                        tool_limit,
                        tool_name
                    ))
                    .await;
                used_tool_calls += 1;
                match self.execute_chat_tool(context, &request).await {
                    Ok(ChatToolExecution::Value(value)) => tool_results.push(value),
                    Ok(ChatToolExecution::GroupFileDownload(handoff)) => {
                        return Ok(ToolDrivenCompletion::GroupFileDownload(handoff));
                    }
                    Err(error) => tool_results.push(json!({
                        "tool": tool_name,
                        "ok": false,
                        "error": format!("{error:#}")
                    })),
                }
            }

            if tool_call_count > remaining {
                tool_results.push(json!({
                    "tool": "tool_budget",
                    "ok": false,
                    "error": format!(
                        "本轮工具额度不足，已执行 {} 次，剩余调用已拒绝。下一条请优先基于已有结果直接输出文本。",
                        remaining
                    )
                }));
            }

            working_messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: Value::String(completion),
            });
            working_messages.push(ChatMessage {
                role: "user".to_string(),
                content: Value::String(build_tool_result_prompt(
                    &tool_results,
                    used_tool_calls,
                    tool_limit,
                )),
            });
        }
    }

    async fn execute_chat_tool(
        &self,
        context: &EventContext,
        request: &Value,
    ) -> Result<ChatToolExecution> {
        let tool_name = request
            .get("tool")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .ok_or_else(|| anyhow::anyhow!("工具请求缺少 tool 字段"))?;
        match tool_name {
            "inspect_codex_project" => Ok(ChatToolExecution::Value(
                self.execute_inspect_codex_project(request).await?,
            )),
            "list_codex_directory" => Ok(ChatToolExecution::Value(
                self.execute_list_codex_directory(request).await?,
            )),
            "search_codex_files" => Ok(ChatToolExecution::Value(
                self.execute_search_codex_files(request).await?,
            )),
            "read_codex_file" => Ok(ChatToolExecution::Value(
                self.execute_read_codex_file(request).await?,
            )),
            "read_bot_memory" => Ok(ChatToolExecution::Value(
                self.execute_read_bot_memory(request).await?,
            )),
            "append_bot_memory" => Ok(ChatToolExecution::Value(
                self.execute_append_bot_memory(request).await?,
            )),
            "start_group_file_download" => self.execute_start_group_file_download(context, request).await,
            "read_github_repo_releases" => Ok(ChatToolExecution::Value(
                self.execute_read_github_repo_releases(request).await?,
            )),
            "read_github_repo_commits" => Ok(ChatToolExecution::Value(
                self.execute_read_github_repo_commits(request).await?,
            )),
            other => bail!("不支持的工具：{other}"),
        }
    }

    fn codex_root(&self) -> Result<PathBuf> {
        if !self.config.answer.enable_codex_readonly_tools {
            bail!("codex 只读工具未启用");
        }
        let root = self
            .config
            .answer
            .codex_root
            .clone()
            .context("qa.answer.codexRoot 未配置")?;
        if !root.exists() {
            bail!("codexRoot 不存在：{}", root.display());
        }
        Ok(root)
    }

    async fn execute_inspect_codex_project(&self, request: &Value) -> Result<Value> {
        let project = get_string_field(request, &["project", "name"]).unwrap_or_default();
        let path_hint = get_string_field(request, &["path"]).unwrap_or_default();
        let root = self.codex_root()?;
        task::spawn_blocking(move || inspect_codex_project_blocking(root, project, path_hint))
            .await
            .context("inspect_codex_project 执行失败")?
    }

    async fn execute_list_codex_directory(&self, request: &Value) -> Result<Value> {
        let root = self.codex_root()?;
        let requested_path = get_string_field(request, &["path", "dir"]).unwrap_or_else(|| ".".to_string());
        let max_entries = clamp_usize(
            get_usize_field(request, &["max_entries", "limit"]).unwrap_or(50),
            1,
            CODEX_MAX_DIRECTORY_ENTRIES,
        );
        let (absolute_path, relative_path) = resolve_codex_relative_path(&root, &requested_path)?;
        let metadata = fs::metadata(&absolute_path)
            .await
            .with_context(|| format!("读取目录失败：{}", absolute_path.display()))?;
        if !metadata.is_dir() {
            bail!("不是目录：{}", relative_path);
        }
        let mut reader = fs::read_dir(&absolute_path)
            .await
            .with_context(|| format!("列目录失败：{}", absolute_path.display()))?;
        let mut entries = Vec::<(bool, String, String)>::new();
        while let Some(entry) = reader.next_entry().await? {
            let file_name = entry.file_name().to_string_lossy().to_string();
            let entry_metadata = entry.metadata().await?;
            let entry_relative = relative_display_path(
                &root,
                &absolute_path.join(&file_name),
            );
            entries.push((entry_metadata.is_dir(), file_name, entry_relative));
        }
        entries.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.to_lowercase().cmp(&right.1.to_lowercase()))
        });
        let truncated = entries.len() > max_entries;
        let normalized = entries
            .into_iter()
            .take(max_entries)
            .map(|(is_dir, name, path)| {
                json!({
                    "name": name,
                    "path": path,
                    "kind": if is_dir { "dir" } else { "file" }
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "tool": "list_codex_directory",
            "ok": true,
            "path": relative_path,
            "returnedCount": normalized.len(),
            "truncated": truncated,
            "entries": normalized
        }))
    }

    async fn execute_search_codex_files(&self, request: &Value) -> Result<Value> {
        let query = get_string_field(request, &["query", "keyword"])
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .ok_or_else(|| anyhow::anyhow!("search_codex_files 缺少 query"))?;
        let base_path = get_string_field(request, &["path"]).unwrap_or_else(|| ".".to_string());
        let limit = clamp_usize(
            get_usize_field(request, &["limit", "max_results"]).unwrap_or(10),
            1,
            CODEX_MAX_SEARCH_RESULTS,
        );
        let root = self.codex_root()?;
        task::spawn_blocking(move || search_codex_files_blocking(root, base_path, query, limit))
            .await
            .context("search_codex_files 执行失败")?
    }

    async fn execute_read_codex_file(&self, request: &Value) -> Result<Value> {
        let root = self.codex_root()?;
        let requested_path = get_string_field(request, &["path", "file"])
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .ok_or_else(|| anyhow::anyhow!("read_codex_file 缺少 path"))?;
        let (absolute_path, relative_path) = resolve_codex_relative_path(&root, &requested_path)?;
        let bytes = fs::read(&absolute_path)
            .await
            .with_context(|| format!("读取文件失败：{}", absolute_path.display()))?;
        let text = String::from_utf8_lossy(&bytes).to_string();
        let requested_start = get_usize_field(request, &["start_line", "startLine"]).unwrap_or(1);
        let requested_end = get_usize_field(request, &["end_line", "endLine"]);
        let max_chars = clamp_usize(
            get_usize_field(request, &["max_chars", "maxChars"]).unwrap_or(8_000),
            200,
            CODEX_MAX_FILE_CHARS,
        );
        let (content, start_line, end_line, truncated) =
            slice_file_excerpt(&text, requested_start, requested_end, max_chars, CODEX_MAX_FILE_LINES);
        Ok(json!({
            "tool": "read_codex_file",
            "ok": true,
            "path": relative_path,
            "startLine": start_line,
            "endLine": end_line,
            "truncated": truncated,
            "content": content
        }))
    }

    async fn execute_read_bot_memory(&self, request: &Value) -> Result<Value> {
        let memory_path = self
            .config
            .answer
            .memory_file
            .as_ref()
            .context("memoryFile 未配置")?;
        let max_chars = clamp_usize(
            get_usize_field(request, &["max_chars", "maxChars"]).unwrap_or(8_000),
            200,
            CODEX_MAX_FILE_CHARS,
        );
        let raw = fs::read_to_string(memory_path)
            .await
            .with_context(|| format!("读取长期记忆失败：{}", memory_path.display()))?;
        let normalized = raw
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        let content = trim_text(
            &normalized.join("\n"),
            max_chars,
            "\n...(长期记忆已截断)",
        );
        Ok(json!({
            "tool": "read_bot_memory",
            "ok": true,
            "path": memory_path.display().to_string(),
            "lineCount": normalized.len(),
            "truncated": content.len() < normalized.join("\n").len(),
            "content": content
        }))
    }

    async fn execute_append_bot_memory(&self, request: &Value) -> Result<Value> {
        let memory_path = self
            .config
            .answer
            .memory_file
            .as_ref()
            .context("memoryFile 未配置")?;
        let memory = get_string_field(request, &["memory", "text"])
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .ok_or_else(|| anyhow::anyhow!("append_bot_memory 缺少 memory"))?;
        let appended = append_memory_entry(memory_path, &memory).await?;
        Ok(json!({
            "tool": "append_bot_memory",
            "ok": true,
            "path": memory_path.display().to_string(),
            "appended": appended,
            "memory": memory
        }))
    }

    async fn execute_start_group_file_download(
        &self,
        context: &EventContext,
        request: &Value,
    ) -> Result<ChatToolExecution> {
        if context.message_type != "group" {
            bail!("文件下载流程仅支持群聊");
        }
        if !self
            .runtime_config_store
            .is_qa_group_file_download_enabled(&context.group_id)
            .await
        {
            bail!("当前群未启用群文件下载流程");
        }
        let mut request_object = request
            .as_object()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("start_group_file_download 请求必须是 JSON 对象"))?;
        request_object.remove("tool");
        let request_text = get_string_field(
            &Value::Object(request_object.clone()),
            &["request_text", "requestText", "query", "text"],
        )
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .unwrap_or_else(|| build_group_download_request_text_from_tool_request(&request_object));
        if request_text.trim().is_empty() {
            bail!("start_group_file_download 缺少 request_text");
        }
        request_object.insert("request_text".to_string(), Value::String(request_text.clone()));
        Ok(ChatToolExecution::GroupFileDownload(GroupFileDownloadRequest {
            request_text,
            request: Value::Object(request_object),
        }))
    }

    async fn execute_read_github_repo_releases(&self, request: &Value) -> Result<Value> {
        let repo = parse_github_repo_specifier(
            get_string_field(request, &["repo", "repository", "url"]).unwrap_or_default().as_str(),
        )?;
        let max_releases = clamp_usize(
            get_usize_field(request, &["max_releases", "maxReleases"]).unwrap_or(10),
            1,
            MAX_GITHUB_RELEASES,
        );
        let max_body_chars = clamp_usize(
            get_usize_field(request, &["max_body_chars", "maxBodyChars"]).unwrap_or(4_000),
            200,
            CODEX_MAX_FILE_CHARS,
        );
        let mut releases = Vec::<GithubReleaseToolPayload>::new();
        let mut page = 1usize;
        while releases.len() < max_releases {
            let per_page = (max_releases - releases.len()).min(100);
            let page_items: Vec<GithubReleaseToolPayload> = self
                .github_api_get_json(
                    &format!("/repos/{}/{}/releases", repo.owner, repo.repo),
                    &[("per_page", per_page.to_string()), ("page", page.to_string())],
                )
                .await?;
            if page_items.is_empty() {
                break;
            }
            releases.extend(page_items);
            if per_page >= 100 {
                page += 1;
            } else {
                break;
            }
        }
        releases.truncate(max_releases);
        Ok(json!({
            "tool": "read_github_repo_releases",
            "ok": true,
            "repo": {
                "owner": repo.owner,
                "repo": repo.repo,
                "full_name": repo.full_name,
                "html_url": repo.html_url
            },
            "requestedCount": max_releases,
            "returnedCount": releases.len(),
            "latestTag": releases.first().map(|item| item.tag_name.clone()).unwrap_or_default(),
            "releases": releases
                .into_iter()
                .enumerate()
                .map(|(index, release)| {
                    json!({
                        "index": index + 1,
                        "id": release.id,
                        "tag_name": release.tag_name,
                        "name": release.name,
                        "target_commitish": release.target_commitish,
                        "prerelease": release.prerelease,
                        "draft": release.draft,
                        "author": if release.author.login.trim().is_empty() {
                            Value::Null
                        } else {
                            Value::String(release.author.login)
                        },
                        "published_at": if release.published_at.trim().is_empty() {
                            Value::Null
                        } else {
                            Value::String(release.published_at)
                        },
                        "created_at": if release.created_at.trim().is_empty() {
                            Value::Null
                        } else {
                            Value::String(release.created_at)
                        },
                        "html_url": if release.html_url.trim().is_empty() {
                            Value::Null
                        } else {
                            Value::String(release.html_url)
                        },
                        "body": trim_text(&release.body, max_body_chars, "\n...(release 正文已截断)"),
                        "assets": release
                            .assets
                            .into_iter()
                            .take(12)
                            .map(|asset| {
                                json!({
                                    "name": asset.name,
                                    "size": asset.size,
                                    "browser_download_url": asset.browser_download_url
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect::<Vec<_>>()
        }))
    }

    async fn execute_read_github_repo_commits(&self, request: &Value) -> Result<Value> {
        let repo = parse_github_repo_specifier(
            get_string_field(request, &["repo", "repository", "url"]).unwrap_or_default().as_str(),
        )?;
        let max_commits = clamp_usize(
            get_usize_field(request, &["max_commits", "maxCommits"]).unwrap_or(30),
            1,
            MAX_GITHUB_COMMITS,
        );
        let max_message_chars = clamp_usize(
            get_usize_field(request, &["max_message_chars", "maxMessageChars"]).unwrap_or(3_000),
            200,
            CODEX_MAX_FILE_CHARS,
        );
        let sha = get_string_field(request, &["sha", "branch", "ref"]).unwrap_or_default();
        let mut commits = Vec::<GithubCommitToolPayload>::new();
        let mut page = 1usize;
        while commits.len() < max_commits {
            let per_page = (max_commits - commits.len()).min(100);
            let mut query = vec![
                ("per_page", per_page.to_string()),
                ("page", page.to_string()),
            ];
            if !sha.trim().is_empty() {
                query.push(("sha", sha.trim().to_string()));
            }
            let page_items: Vec<GithubCommitToolPayload> = self
                .github_api_get_json(
                    &format!("/repos/{}/{}/commits", repo.owner, repo.repo),
                    &query,
                )
                .await?;
            if page_items.is_empty() {
                break;
            }
            commits.extend(page_items);
            if per_page >= 100 {
                page += 1;
            } else {
                break;
            }
        }
        commits.truncate(max_commits);
        Ok(json!({
            "tool": "read_github_repo_commits",
            "ok": true,
            "repo": {
                "owner": repo.owner,
                "repo": repo.repo,
                "full_name": repo.full_name,
                "html_url": repo.html_url
            },
            "requestedCount": max_commits,
            "returnedCount": commits.len(),
            "commits": commits
                .into_iter()
                .enumerate()
                .map(|(index, commit)| {
                    let (title, description) = split_commit_message_parts(&commit.commit.message);
                    json!({
                        "index": index + 1,
                        "sha": commit.sha,
                        "title": title,
                        "description": trim_text(&description, max_message_chars, "\n...(commit 正文已截断)"),
                        "html_url": if commit.html_url.trim().is_empty() {
                            Value::Null
                        } else {
                            Value::String(commit.html_url)
                        },
                        "author": commit.commit.author.as_ref().map(|item| item.name.clone()).filter(|item| !item.trim().is_empty()),
                        "date": commit.commit.author.as_ref().map(|item| item.date.clone()).filter(|item| !item.trim().is_empty())
                    })
                })
                .collect::<Vec<_>>()
        }))
    }

    async fn github_api_get_json<T>(&self, api_path: &str, query: &[(&str, String)]) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let base = std::env::var("CAINBOT_GITHUB_API_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_GITHUB_API_BASE_URL.to_string());
        let base = base.trim().trim_end_matches('/').to_string();
        let mut url = reqwest::Url::parse(&format!("{base}{api_path}"))
            .with_context(|| format!("GitHub API 地址无效：{base}{api_path}"))?;
        for (key, value) in query {
            if value.trim().is_empty() {
                continue;
            }
            url.query_pairs_mut().append_pair(key, value);
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(15_000))
            .build()
            .context("创建 GitHub API 客户端失败")?;
        let mut request_builder = client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "NapCatCainBot/0.1");
        let token = std::env::var("CAINBOT_GITHUB_TOKEN")
            .ok()
            .filter(|item| !item.trim().is_empty())
            .or_else(|| {
                std::env::var("GITHUB_TOKEN")
                    .ok()
                    .filter(|item| !item.trim().is_empty())
            });
        if let Some(token) = token {
            request_builder = request_builder.bearer_auth(token.trim());
        }
        let response = request_builder.send().await.context("GitHub API 请求失败")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = trim_text(
                &response.text().await.unwrap_or_default(),
                500,
                " ...(GitHub 错误正文已截断)",
            );
            bail!("GitHub API {status}: {body}");
        }
        response
            .json::<T>()
            .await
            .context("解析 GitHub API 响应失败")
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
        if let Some(tool_prompt) = self.build_answer_tool_prompt(context).await {
            parts.push(tool_prompt);
        }
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

    async fn build_answer_tool_prompt(&self, context: &EventContext) -> Option<String> {
        let mut lines = Vec::<String>::new();
        if self.config.answer.enable_codex_readonly_tools
            && self
                .config
                .answer
                .codex_root
                .as_ref()
                .map(|item| item.exists())
                .unwrap_or(false)
        {
            lines.push(
                "可用只读代码工具：inspect_codex_project、list_codex_directory、search_codex_files、read_codex_file。".to_string(),
            );
            lines.push(
                "如果要确认仓库、文件、字段、路径、源码实现，优先先调工具，不要直接凭记忆下结论。".to_string(),
            );
        }
        if self
            .config
            .answer
            .memory_file
            .as_ref()
            .map(|item| item.exists())
            .unwrap_or(false)
        {
            lines.push("可用记忆工具：read_bot_memory、append_bot_memory。".to_string());
        }
        lines.push(
            "可用 GitHub 工具：read_github_repo_releases、read_github_repo_commits。涉及版本、release、tag、pre-release、commit 时先查再答。"
                .to_string(),
        );
        if context.message_type == "group"
            && self
                .runtime_config_store
                .is_qa_group_file_download_enabled(&context.group_id)
                .await
        {
            lines.push(
                "可用下载工具：start_group_file_download。用户要安装包、apk、jar、zip、release 资产、最新版文件、指定 commit 编译包时，直接调用这个工具，不要先发口头承诺。".to_string(),
            );
            lines.push(
                "start_group_file_download 常用参数：request_text、repo_choice、version_query、platform_hint、folder_name、mode、commit_hash、release_channel、release_keyword。".to_string(),
            );
            lines.push(
                "如果要最新 pre-release，传 version_query:\"latest\" 且 release_channel:\"prerelease\"。如果要筛选标题、tag 或正文包含某个词的 (pre-)release，传 release_keyword。".to_string(),
            );
        }
        if lines.is_empty() {
            return None;
        }
        let mut parts = vec![
            "工具调用协议：".to_string(),
            format!(
                "严格输出 {}{{\"tool\":\"工具名\",\"参数\":\"值\"}}{}",
                TOOL_REQUEST_START, TOOL_REQUEST_END
            ),
            "优先一次调用一个工具；拿到结果后要么继续调下一个工具，要么直接输出最终回复。".to_string(),
        ];
        parts.extend(lines);
        Some(parts.join("\n"))
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

#[derive(Debug, Clone)]
struct GithubRepoSpecifier {
    owner: String,
    repo: String,
    full_name: String,
    html_url: String,
}

fn get_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToString::to_string)
    })
}

fn get_usize_field(value: &Value, keys: &[&str]) -> Option<usize> {
    keys.iter().find_map(|key| match value.get(*key) {
        Some(Value::Number(number)) => number.as_u64().map(|item| item as usize),
        Some(Value::String(text)) => text.trim().parse::<usize>().ok(),
        _ => None,
    })
}

fn clamp_usize(value: usize, min: usize, max: usize) -> usize {
    value.max(min).min(max)
}

fn trim_text(value: &str, max_chars: usize, suffix: &str) -> String {
    let normalized = value.trim();
    let total_chars = normalized.chars().count();
    if total_chars <= max_chars {
        return normalized.to_string();
    }
    format!(
        "{}{}",
        normalized.chars().take(max_chars).collect::<String>(),
        suffix
    )
}

fn relative_display_path(root: &Path, target: &Path) -> String {
    target
        .strip_prefix(root)
        .ok()
        .and_then(|item| {
            let text = item.to_string_lossy().replace('\\', "/");
            if text.trim().is_empty() {
                None
            } else {
                Some(text)
            }
        })
        .unwrap_or_else(|| ".".to_string())
}

fn resolve_codex_relative_path(root: &Path, requested_path: &str) -> Result<(PathBuf, String)> {
    let mut absolute = root.to_path_buf();
    let normalized = requested_path.trim();
    let requested = if normalized.is_empty() { "." } else { normalized };
    for component in Path::new(requested).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(segment) => absolute.push(segment),
            std::path::Component::ParentDir => bail!("路径超出 /codex 目录范围"),
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                bail!("路径必须是 /codex 内的相对路径")
            }
        }
    }
    let relative = relative_display_path(root, &absolute);
    Ok((absolute, relative))
}

fn build_tool_result_prompt(results: &[Value], used: usize, limit: usize) -> String {
    let pretty = serde_json::to_string_pretty(results)
        .unwrap_or_else(|_| "[]".to_string());
    let trimmed = trim_text(&pretty, 14_000, "\n...(工具结果已截断)");
    [
        format!("系统工具结果：本轮已执行 {used}/{limit} 次工具。"),
        "```json".to_string(),
        trimmed,
        "```".to_string(),
        if used < limit {
            "如果还缺关键信息，可以继续调用工具；如果信息已足够，直接输出新的完整回复。".to_string()
        } else {
            "本轮工具额度已经用完，下一条禁止继续调用工具，必须直接输出新的完整回复。".to_string()
        },
    ]
    .join("\n")
}

fn build_tool_limit_retry_prompt(limit: usize) -> String {
    format!(
        "本轮最多只能调用 {limit} 次工具，额度已经用完。下一条禁止再输出任何工具请求，必须直接给出新的完整回复。"
    )
}

fn parse_tool_calls(content: &str) -> Vec<Value> {
    let marked = extract_marked_tool_calls(content);
    if !marked.is_empty() {
        return marked;
    }
    extract_balanced_json_objects(content)
        .into_iter()
        .filter_map(|item| parse_single_tool_object(&item))
        .collect()
}

fn extract_marked_tool_calls(content: &str) -> Vec<Value> {
    let mut calls = Vec::<Value>::new();
    let mut cursor = 0usize;
    while let Some(start_rel) = content[cursor..].find(TOOL_REQUEST_START) {
        let start = cursor + start_rel + TOOL_REQUEST_START.len();
        let Some(end_rel) = content[start..].find(TOOL_REQUEST_END) else {
            break;
        };
        let end = start + end_rel;
        if let Some(request) = parse_single_tool_object(&content[start..end]) {
            calls.push(request);
        }
        cursor = end + TOOL_REQUEST_END.len();
    }
    calls
}

fn extract_balanced_json_objects(text: &str) -> Vec<String> {
    let source = strip_code_fence(text);
    let mut results = Vec::<String>::new();
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
            {
                results.push(source[start..=index].to_string());
                start_index = None;
            }
        }
    }
    results
}

fn strip_code_fence(text: &str) -> String {
    let normalized = text.trim();
    if let Some(inner) = normalized
        .strip_prefix("```json")
        .and_then(|item| item.strip_suffix("```"))
    {
        return inner.trim().to_string();
    }
    if let Some(inner) = normalized
        .strip_prefix("```")
        .and_then(|item| item.strip_suffix("```"))
    {
        return inner.trim().to_string();
    }
    normalized.to_string()
}

fn parse_single_tool_object(text: &str) -> Option<Value> {
    let parsed = serde_json::from_str::<Value>(&strip_code_fence(text)).ok()?;
    parsed
        .get("tool")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())?;
    Some(parsed)
}

fn strip_marked_tool_calls(content: &str) -> String {
    let mut result = String::new();
    let mut cursor = 0usize;
    while let Some(start_rel) = content[cursor..].find(TOOL_REQUEST_START) {
        let start = cursor + start_rel;
        result.push_str(&content[cursor..start]);
        let content_start = start + TOOL_REQUEST_START.len();
        let Some(end_rel) = content[content_start..].find(TOOL_REQUEST_END) else {
            cursor = content.len();
            break;
        };
        cursor = content_start + end_rel + TOOL_REQUEST_END.len();
    }
    if cursor < content.len() {
        result.push_str(&content[cursor..]);
    }
    result
}

fn slice_file_excerpt(
    text: &str,
    requested_start: usize,
    requested_end: Option<usize>,
    max_chars: usize,
    max_lines: usize,
) -> (String, usize, usize, bool) {
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return (String::new(), 0, 0, false);
    }
    let start_line = clamp_usize(requested_start.max(1), 1, lines.len());
    let mut end_line = requested_end.unwrap_or(start_line + max_lines.saturating_sub(1));
    end_line = end_line.max(start_line).min(lines.len());
    if end_line.saturating_sub(start_line) + 1 > max_lines {
        end_line = (start_line + max_lines.saturating_sub(1)).min(lines.len());
    }
    let numbered = lines[start_line - 1..end_line]
        .iter()
        .enumerate()
        .map(|(index, line)| format!("{}: {}", start_line + index, line))
        .collect::<Vec<_>>()
        .join("\n");
    let truncated = end_line < lines.len() || numbered.len() > max_chars;
    (
        trim_text(&numbered, max_chars, "\n...(文件内容已截断)"),
        start_line,
        end_line,
        truncated,
    )
}

fn split_commit_message_parts(message: &str) -> (String, String) {
    let normalized = message.replace("\r\n", "\n");
    let mut parts = normalized.lines();
    let title = parts.next().unwrap_or_default().trim().to_string();
    let description = parts.collect::<Vec<_>>().join("\n").trim().to_string();
    (title, description)
}

fn parse_github_repo_specifier(input: &str) -> Result<GithubRepoSpecifier> {
    let normalized = input
        .trim()
        .trim_start_matches("git+")
        .trim_end_matches(".git")
        .trim();
    if normalized.is_empty() {
        bail!("repo 不能为空，应为 owner/repo 或 GitHub 仓库链接");
    }
    if let Some((owner, repo)) = normalized.split_once('/') {
        if owner
            .chars()
            .all(|item| item.is_ascii_alphanumeric() || matches!(item, '.' | '_' | '-'))
            && repo
                .chars()
                .all(|item| item.is_ascii_alphanumeric() || matches!(item, '.' | '_' | '-'))
        {
            let repo = repo.trim_end_matches(".git");
            return Ok(GithubRepoSpecifier {
                owner: owner.to_string(),
                repo: repo.to_string(),
                full_name: format!("{owner}/{repo}"),
                html_url: format!("https://github.com/{owner}/{repo}"),
            });
        }
    }
    let repo_url = if normalized.starts_with("http://") || normalized.starts_with("https://") {
        normalized.to_string()
    } else {
        format!("https://{normalized}")
    };
    let parsed =
        reqwest::Url::parse(&repo_url).with_context(|| format!("无法解析 GitHub 仓库：{normalized}"))?;
    let parts = parsed
        .path_segments()
        .map(|segments| {
            segments
                .filter(|item| !item.trim().is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if (parsed.host_str() == Some("github.com") || parsed.host_str() == Some("www.github.com"))
        && parts.len() >= 2
    {
        let owner = parts[0].trim();
        let repo = parts[1].trim().trim_end_matches(".git");
        if !owner.is_empty() && !repo.is_empty() {
            return Ok(GithubRepoSpecifier {
                owner: owner.to_string(),
                repo: repo.to_string(),
                full_name: format!("{owner}/{repo}"),
                html_url: format!("https://github.com/{owner}/{repo}"),
            });
        }
    }
    bail!("无法解析 GitHub 仓库：{normalized}")
}

fn build_group_download_request_text_from_tool_request(
    request: &serde_json::Map<String, Value>,
) -> String {
    let read = |keys: &[&str]| -> String {
        keys.iter()
            .find_map(|key| {
                request
                    .get(*key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(ToString::to_string)
            })
            .unwrap_or_default()
    };
    let mode = read(&["mode"]);
    let repo = read(&["repo_choice", "repo"]);
    let version = read(&["version_query", "version", "tag"]);
    let keyword = read(&["release_keyword", "keyword"]);
    let channel = read(&["release_channel", "channel"]);
    let commit = read(&["commit_hash", "commit", "hash", "sha"]);
    let platform = read(&["platform_hint", "platform"]);
    let folder = read(&["folder_name", "folderName"]);
    let mut parts = Vec::<String>::new();
    if !repo.is_empty() {
        parts.push(format!("仓库={repo}"));
    }
    if !version.is_empty() {
        parts.push(format!("版本={version}"));
    }
    if !keyword.is_empty() {
        parts.push(format!("release_keyword={keyword}"));
    }
    if !channel.is_empty() {
        parts.push(format!("release_channel={channel}"));
    }
    if !commit.is_empty() {
        parts.push(format!("commit={commit}"));
    }
    if !platform.is_empty() {
        parts.push(format!("platform={platform}"));
    }
    if !folder.is_empty() {
        parts.push(format!("folder={folder}"));
    }
    if !mode.is_empty() {
        parts.push(format!("mode={mode}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("下载请求：{}", parts.join("，"))
    }
}

fn inspect_codex_project_blocking(root: PathBuf, project: String, path_hint: String) -> Result<Value> {
    let target_path = if !path_hint.trim().is_empty() {
        let (absolute, _) = resolve_codex_relative_path(&root, &path_hint)?;
        absolute
    } else if !project.trim().is_empty() {
        find_codex_project_dir(&root, &project)?
    } else {
        root.clone()
    };
    let relative_path = relative_display_path(&root, &target_path);
    let entries = collect_directory_entries(&root, &target_path, 80)?;
    let context_files = collect_project_context_files(&root, &target_path)?;
    Ok(json!({
        "tool": "inspect_codex_project",
        "ok": true,
        "project": if project.trim().is_empty() {
            target_path.file_name().map(|item| item.to_string_lossy().to_string()).unwrap_or_else(|| ".".to_string())
        } else {
            project
        },
        "path": relative_path,
        "entries": entries,
        "context_files": context_files
    }))
}

fn find_codex_project_dir(root: &Path, query: &str) -> Result<PathBuf> {
    let normalized = query.trim();
    let (direct_path, _) = resolve_codex_relative_path(root, normalized)?;
    if direct_path.exists() && direct_path.is_dir() {
        return Ok(direct_path);
    }
    let needle = normalized.to_lowercase();
    let mut best_match = None::<(i64, PathBuf)>;
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((current_dir, depth)) = stack.pop() {
        if depth > 4 {
            continue;
        }
        for entry in std::fs::read_dir(&current_dir)
            .with_context(|| format!("读取目录失败：{}", current_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let file_name = entry.file_name().to_string_lossy().to_string();
            if should_skip_codex_dir(&file_name) {
                continue;
            }
            let relative = relative_display_path(root, &path);
            let name_lower = file_name.to_lowercase();
            let relative_lower = relative.to_lowercase();
            let mut score = 0i64;
            if relative == normalized {
                score += 400;
            }
            if file_name == normalized {
                score += 320;
            }
            if name_lower == needle {
                score += 300;
            }
            if relative_lower == needle {
                score += 260;
            }
            if name_lower.contains(&needle) {
                score += 120;
            }
            if relative_lower.contains(&needle) {
                score += 80;
            }
            if score > 0 {
                let replace = best_match
                    .as_ref()
                    .map(|(best_score, best_path)| {
                        score > *best_score
                            || (score == *best_score
                                && relative_display_path(root, &path).len()
                                    < relative_display_path(root, best_path).len())
                    })
                    .unwrap_or(true);
                if replace {
                    best_match = Some((score, path.clone()));
                }
            }
            stack.push((path, depth + 1));
        }
    }
    best_match
        .map(|(_, path)| path)
        .ok_or_else(|| anyhow::anyhow!("未找到项目目录：{query}"))
}

fn collect_directory_entries(root: &Path, target_path: &Path, max_entries: usize) -> Result<Vec<Value>> {
    let mut entries = std::fs::read_dir(target_path)
        .with_context(|| format!("读取目录失败：{}", target_path.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| {
        let left_path = left.path();
        let right_path = right.path();
        right_path
            .is_dir()
            .cmp(&left_path.is_dir())
            .then_with(|| {
                left.file_name()
                    .to_string_lossy()
                    .to_lowercase()
                    .cmp(&right.file_name().to_string_lossy().to_lowercase())
            })
    });
    Ok(entries
        .into_iter()
        .take(max_entries)
        .map(|entry| {
            let path = entry.path();
            json!({
                "name": entry.file_name().to_string_lossy().to_string(),
                "path": relative_display_path(root, &path),
                "kind": if path.is_dir() { "dir" } else { "file" }
            })
        })
        .collect())
}

fn collect_project_context_files(root: &Path, target_path: &Path) -> Result<Vec<Value>> {
    let preferred = [
        "AGENTS.md",
        "README.md",
        "README",
        "package.json",
        "Cargo.toml",
        "mod.json",
        "plugin.json",
        "build.gradle",
        "settings.gradle",
        "gradle.properties",
    ];
    let mut results = Vec::<Value>::new();
    for name in preferred {
        let path = target_path.join(name);
        if !path.is_file() {
            continue;
        }
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let excerpt = trim_text(
            &content
                .lines()
                .take(120)
                .collect::<Vec<_>>()
                .join("\n"),
            CODEX_MAX_PROJECT_HINT_CHARS,
            "\n...(项目上下文已截断)",
        );
        results.push(json!({
            "path": relative_display_path(root, &path),
            "content": excerpt
        }));
    }
    Ok(results)
}

fn search_codex_files_blocking(
    root: PathBuf,
    base_path: String,
    query: String,
    limit: usize,
) -> Result<Value> {
    let (absolute_base, relative_base) = resolve_codex_relative_path(&root, &base_path)?;
    let lower_query = query.to_lowercase();
    let mut results = Vec::<Value>::new();
    let mut stack = vec![absolute_base.clone()];
    while let Some(current_path) = stack.pop() {
        if results.len() >= limit {
            break;
        }
        if current_path.is_dir() {
            for entry in std::fs::read_dir(&current_path)
                .with_context(|| format!("读取目录失败：{}", current_path.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                let file_name = entry.file_name().to_string_lossy().to_string();
                if path.is_dir() {
                    if should_skip_codex_dir(&file_name) {
                        continue;
                    }
                    stack.push(path);
                    continue;
                }
                let relative = relative_display_path(&root, &path);
                if file_name.to_lowercase().contains(&lower_query) {
                    results.push(json!({
                        "path": relative,
                        "match": "path",
                        "snippet": file_name
                    }));
                    if results.len() >= limit {
                        break;
                    }
                }
                if !is_probably_text_file(&path) {
                    continue;
                }
                if entry.metadata().map(|item| item.len()).unwrap_or(0) > 1_024 * 1_024 {
                    continue;
                }
                let content = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(_) => continue,
                };
                for (index, line) in content.lines().enumerate() {
                    if !line.to_lowercase().contains(&lower_query) {
                        continue;
                    }
                    results.push(json!({
                        "path": relative,
                        "match": "content",
                        "line": index + 1,
                        "snippet": build_line_snippet(line, &lower_query)
                    }));
                    if results.len() >= limit {
                        break;
                    }
                }
                if results.len() >= limit {
                    break;
                }
            }
            continue;
        }
        if current_path.is_file() && is_probably_text_file(&current_path) {
            let relative = relative_display_path(&root, &current_path);
            let content = std::fs::read_to_string(&current_path).unwrap_or_default();
            for (index, line) in content.lines().enumerate() {
                if !line.to_lowercase().contains(&lower_query) {
                    continue;
                }
                results.push(json!({
                    "path": relative,
                    "match": "content",
                    "line": index + 1,
                    "snippet": build_line_snippet(line, &lower_query)
                }));
                if results.len() >= limit {
                    break;
                }
            }
        }
    }
    Ok(json!({
        "tool": "search_codex_files",
        "ok": true,
        "query": query,
        "base_path": relative_base,
        "returnedCount": results.len(),
        "results": results
    }))
}

fn build_line_snippet(line: &str, query: &str) -> String {
    let lower_line = line.to_lowercase();
    let hit_index = lower_line.find(query).unwrap_or(0);
    let chars = line.chars().collect::<Vec<_>>();
    if chars.len() <= 180 {
        return line.to_string();
    }
    let hit_char_index = line[..hit_index].chars().count();
    let start = hit_char_index.saturating_sub(60);
    let end = (hit_char_index + query.chars().count() + 60).min(chars.len());
    format!(
        "{}{}{}",
        if start > 0 { "..." } else { "" },
        chars[start..end].iter().collect::<String>(),
        if end < chars.len() { "..." } else { "" }
    )
}

fn should_skip_codex_dir(name: &str) -> bool {
    [
        ".git",
        "node_modules",
        "dist",
        "build",
        "out",
        "bin",
        "obj",
        ".gradle",
        ".idea",
        ".next",
        ".cache",
        "coverage",
        "vendor",
        "Pods",
        "target",
    ]
    .iter()
    .any(|item| name.eq_ignore_ascii_case(item))
}

fn is_probably_text_file(path: &Path) -> bool {
    let file_name = path
        .file_name()
        .map(|item| item.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if matches!(
        file_name.as_str(),
        "readme"
            | "readme.md"
            | "agents.md"
            | "cargo.toml"
            | "package.json"
            | "mod.json"
            | "plugin.json"
            | ".gitignore"
            | ".gitattributes"
    ) {
        return true;
    }
    let extension = path
        .extension()
        .and_then(|item| item.to_str())
        .unwrap_or_default()
        .to_lowercase();
    matches!(
        extension.as_str(),
        "txt"
            | "md"
            | "markdown"
            | "json"
            | "jsonc"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "cfg"
            | "conf"
            | "js"
            | "mjs"
            | "cjs"
            | "ts"
            | "tsx"
            | "jsx"
            | "java"
            | "kt"
            | "kts"
            | "gradle"
            | "properties"
            | "xml"
            | "html"
            | "css"
            | "scss"
            | "less"
            | "py"
            | "rb"
            | "php"
            | "go"
            | "rs"
            | "cpp"
            | "c"
            | "h"
            | "hpp"
            | "cs"
            | "sh"
            | "ps1"
            | "bat"
            | "cmd"
            | "sql"
            | "csv"
            | "env"
            | "vue"
            | "svelte"
            | "lua"
    )
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
