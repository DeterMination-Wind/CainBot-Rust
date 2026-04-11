use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::codex_bridge_server::CodexBridgeInfo;
use crate::config::IssueRepairConfig;
use crate::event_utils::{EventContext, get_sender_name};
use crate::logger::Logger;
use crate::napcat_client::NapCatClient;
use crate::openai_chat_client::{ChatMessage, CompleteOptions, OpenAiChatClient};
use crate::state_store::StateStore;
use crate::utils::{ensure_dir, now_iso, sha1_hex};

const OFFER_TTL_MS: u64 = 20 * 60 * 1000;
const DEFAULT_CODEX_TIMEOUT_MS: u64 = 30 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModInfo {
    id: String,
    #[serde(rename = "projectRoot")]
    project_root: String,
    #[serde(rename = "projectFolderName")]
    project_folder_name: String,
    name: String,
    #[serde(rename = "displayName")]
    display_name: String,
    version: String,
    description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredContext {
    #[serde(rename = "messageType")]
    message_type: String,
    #[serde(rename = "groupId")]
    group_id: String,
    #[serde(rename = "userId")]
    user_id: String,
    #[serde(rename = "selfId")]
    self_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OfferRecord {
    id: String,
    #[serde(rename = "scopeKey")]
    scope_key: String,
    context: StoredContext,
    #[serde(rename = "targetUserId")]
    target_user_id: String,
    #[serde(rename = "modId")]
    mod_id: String,
    #[serde(rename = "modDisplayName")]
    mod_display_name: String,
    #[serde(rename = "issueSummary")]
    issue_summary: String,
    #[serde(rename = "expiresAt")]
    expires_at: String,
    #[serde(rename = "botMessageIds")]
    bot_message_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMessage {
    role: String,
    speaker: String,
    text: String,
    #[serde(rename = "createdAt")]
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IssueRepairSession {
    id: String,
    #[serde(rename = "scopeKey")]
    scope_key: String,
    status: String,
    #[serde(rename = "codexThreadId")]
    codex_thread_id: String,
    context: StoredContext,
    #[serde(rename = "targetUserId")]
    target_user_id: String,
    #[serde(rename = "issueSummary")]
    issue_summary: String,
    #[serde(rename = "modInfo")]
    mod_info: ModInfo,
    #[serde(rename = "publishFolderName")]
    publish_folder_name: String,
    #[serde(rename = "latestArtifactPath")]
    latest_artifact_path: String,
    #[serde(rename = "latestArtifactName")]
    latest_artifact_name: String,
    #[serde(rename = "latestArtifactVersion")]
    latest_artifact_version: String,
    messages: Vec<SessionMessage>,
    #[serde(rename = "botMessageIds")]
    bot_message_ids: Vec<String>,
    #[serde(rename = "pendingRerun")]
    pending_rerun: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CodexOutput {
    status: String,
    #[serde(rename = "assistantMessage")]
    assistant_message: String,
    #[serde(rename = "issueSummary")]
    issue_summary: String,
    #[serde(rename = "artifactPath")]
    artifact_path: String,
    #[serde(rename = "artifactName")]
    artifact_name: String,
    #[serde(rename = "artifactVersion")]
    artifact_version: String,
    #[serde(rename = "publishFolderName")]
    publish_folder_name: String,
}

#[derive(Clone)]
pub struct IssueRepairManager {
    config: IssueRepairConfig,
    chat_client: OpenAiChatClient,
    napcat_client: NapCatClient,
    state_store: StateStore,
    logger: Logger,
    bridge_info: Option<CodexBridgeInfo>,
    running_sessions: Arc<Mutex<HashSet<String>>>,
    mod_index: Arc<Mutex<Vec<ModInfo>>>,
    last_refresh_ms: Arc<Mutex<u64>>,
}

impl IssueRepairManager {
    pub fn new(
        config: IssueRepairConfig,
        chat_client: OpenAiChatClient,
        napcat_client: NapCatClient,
        state_store: StateStore,
        logger: Logger,
        bridge_info: Option<CodexBridgeInfo>,
    ) -> Self {
        Self {
            config,
            chat_client,
            napcat_client,
            state_store,
            logger,
            bridge_info,
            running_sessions: Default::default(),
            mod_index: Default::default(),
            last_refresh_ms: Default::default(),
        }
    }

    pub async fn initialize(&self) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }
        self.refresh_mod_index(true).await?;
        Ok(())
    }

    pub async fn handle_incoming_message(
        &self,
        context: &EventContext,
        event: &Value,
        text: &str,
    ) -> Result<bool> {
        if !self.config.enabled {
            return Ok(false);
        }
        let normalized_text = normalize_text(text);
        if normalized_text.is_empty() {
            return Ok(false);
        }

        self.refresh_mod_index(false).await?;
        let scope_key = build_scope_key(context);

        if let Some(offer) = self.find_pending_offer(&scope_key).await? {
            if self
                .maybe_handle_offer_reply(offer, context, event, &normalized_text)
                .await?
            {
                return Ok(true);
            }
        }

        if let Some(session) = self.find_active_session(&scope_key).await? {
            if self
                .maybe_handle_session_reply(session, context, event, &normalized_text)
                .await?
            {
                return Ok(true);
            }
        }

        if !can_offer_issue_repair(&self.config, context) {
            return Ok(false);
        }

        let decision = self
            .classify_candidate(context, event, &normalized_text)
            .await?;
        if !decision
            .get("should_offer")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(false);
        }
        let project_key = normalize_text(opt_value_to_string(decision.get("project_key")));
        let mod_info = self
            .mod_index
            .lock()
            .await
            .clone()
            .into_iter()
            .find(|item| item.id == project_key);
        let Some(mod_info) = mod_info else {
            return Ok(false);
        };

        let issue_summary =
            normalize_issue_summary(opt_value_to_string(decision.get("issue_summary")));
        let mut offer = OfferRecord {
            id: format!(
                "offer-{}",
                sha1_hex(format!(
                    "{scope_key}\n{issue_summary}\n{}",
                    current_time_ms()
                ))
            ),
            scope_key,
            context: stored_context(context),
            target_user_id: context.user_id.clone(),
            mod_id: mod_info.id.clone(),
            mod_display_name: if mod_info.display_name.trim().is_empty() {
                mod_info.project_folder_name.clone()
            } else {
                mod_info.display_name.clone()
            },
            issue_summary,
            expires_at: chrono::Utc::now()
                .checked_add_signed(chrono::TimeDelta::milliseconds(OFFER_TTL_MS as i64))
                .map(|item| item.to_rfc3339())
                .unwrap_or_else(now_iso),
            bot_message_ids: Vec::new(),
        };
        let results = self
            .napcat_client
            .reply_text(
                &context.message_type,
                target_id(context),
                event.get("message_id").map(value_to_string).as_deref(),
                &format!(
                    "看起来像是 {} 的 bug / 体验问题。要不要我直接跟进修一下或顺手优化体验？",
                    offer.mod_display_name
                ),
            )
            .await?;
        offer.bot_message_ids = extract_message_ids(&results);
        self.state_store
            .set_issue_repair_offer(serde_json::to_value(&offer)?)
            .await?;
        self.state_store.save().await?;
        Ok(true)
    }

    async fn refresh_mod_index(&self, force: bool) -> Result<()> {
        let now = current_time_ms();
        let last = *self.last_refresh_ms.lock().await;
        if !force && now.saturating_sub(last) < 5 * 60 * 1000 {
            return Ok(());
        }
        let Some(codex_root) = self.config.codex_root.clone() else {
            *self.mod_index.lock().await = Vec::new();
            *self.last_refresh_ms.lock().await = now;
            return Ok(());
        };
        let owner_name = self.config.owner_name.clone();
        let mods = tokio::task::spawn_blocking(move || scan_owned_mods(&codex_root, &owner_name))
            .await
            .context("扫描模组目录任务失败")??;
        *self.mod_index.lock().await = mods;
        *self.last_refresh_ms.lock().await = now;
        Ok(())
    }

    async fn find_pending_offer(&self, scope_key: &str) -> Result<Option<OfferRecord>> {
        let now = current_time_ms();
        let mut expired = false;
        let mut matched = None;
        for value in self.state_store.list_issue_repair_offers().await {
            let Ok(offer) = serde_json::from_value::<OfferRecord>(value) else {
                continue;
            };
            if offer.scope_key != scope_key {
                continue;
            }
            let expires_ms = chrono::DateTime::parse_from_rfc3339(&offer.expires_at)
                .ok()
                .map(|item| item.timestamp_millis() as u64)
                .unwrap_or(u64::MAX);
            if expires_ms < now {
                self.state_store.delete_issue_repair_offer(&offer.id).await;
                expired = true;
                continue;
            }
            matched = Some(offer);
            break;
        }
        if expired {
            self.state_store.save().await?;
        }
        Ok(matched)
    }

    async fn find_active_session(&self, scope_key: &str) -> Result<Option<IssueRepairSession>> {
        for value in self.state_store.list_issue_repair_sessions().await {
            let Ok(session) = serde_json::from_value::<IssueRepairSession>(value) else {
                continue;
            };
            if session.scope_key == scope_key
                && session.status != "completed"
                && session.status != "failed"
            {
                return Ok(Some(session));
            }
        }
        Ok(None)
    }

    async fn classify_candidate(
        &self,
        context: &EventContext,
        event: &Value,
        text: &str,
    ) -> Result<Value> {
        let catalog = self
            .mod_index
            .lock()
            .await
            .iter()
            .map(|item| {
                format!(
                    "projectKey={} | folder={} | displayName={} | version={} | description={}",
                    item.id,
                    item.project_folder_name,
                    if item.display_name.is_empty() {
                        "-"
                    } else {
                        &item.display_name
                    },
                    if item.version.is_empty() {
                        "-"
                    } else {
                        &item.version
                    },
                    if item.description.is_empty() {
                        "-"
                    } else {
                        &item.description
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let raw = self
            .chat_client
            .complete(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: Value::String(
                            [
                                "你负责判断一条 QQ 消息是否在反馈作者自己的模组问题，并且应该主动询问“要不要我直接跟进修/优化”。",
                                "只在明确匹配到下列模组之一，且消息是在反馈 bug / 报错 / 体验差 / 需要优化时，should_offer 才为 true。",
                                "输出 JSON：{\"should_offer\":boolean,\"project_key\":\"匹配到的projectKey或空字符串\",\"issue_summary\":\"一句话概括\"}。",
                                "",
                                "可匹配模组目录：",
                                if catalog.is_empty() { "(空)" } else { &catalog },
                            ]
                            .join("\n"),
                        ),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: Value::String(
                            [
                                format!(
                                    "消息来源：{}",
                                    if context.message_type == "group" {
                                        format!("群 {}", context.group_id)
                                    } else {
                                        format!("私聊 {}", context.user_id)
                                    }
                                ),
                                format!("发送者：{}", get_sender_name(event)),
                                format!("消息内容：{text}"),
                            ]
                            .join("\n"),
                        ),
                    },
                ],
                CompleteOptions {
                    model: Some(self.config.classify_model.clone()),
                    temperature: Some(0.1),
                    allow_expensive_fallback: false,
                },
            )
            .await?;
        Ok(parse_json_value(&raw).unwrap_or_else(|| json!({ "should_offer": false })))
    }

    async fn maybe_handle_offer_reply(
        &self,
        offer: OfferRecord,
        context: &EventContext,
        event: &Value,
        text: &str,
    ) -> Result<bool> {
        let raw = self
            .chat_client
            .complete(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: Value::String(
                            [
                                "你负责判断用户是否是在回应机器人刚刚提出的“要不要我直接跟进修/优化”。",
                                "输出 JSON：{\"decision\":\"accept|decline|ignore\"}。",
                            ]
                            .join("\n"),
                        ),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: Value::String(
                            [
                                format!("上一条机器人提议：要不要我直接跟进修 {}。", offer.mod_display_name),
                                format!("用户刚发的新消息：{text}"),
                            ]
                            .join("\n"),
                        ),
                    },
                ],
                CompleteOptions {
                    model: Some(self.config.consent_model.clone()),
                    temperature: Some(0.1),
                    allow_expensive_fallback: false,
                },
            )
            .await?;
        let decision = normalize_text(opt_value_to_string(
            parse_json_value(&raw)
                .as_ref()
                .and_then(|item| item.get("decision")),
        ));
        if decision == "ignore" {
            return Ok(false);
        }

        self.state_store.delete_issue_repair_offer(&offer.id).await;
        if decision != "accept" {
            self.state_store.save().await?;
            return Ok(true);
        }

        let mod_info = self
            .mod_index
            .lock()
            .await
            .clone()
            .into_iter()
            .find(|item| item.id == offer.mod_id);
        let Some(mod_info) = mod_info else {
            self.state_store.save().await?;
            return Ok(true);
        };

        let mut session = IssueRepairSession {
            id: format!(
                "session-{}",
                sha1_hex(format!(
                    "{}\n{}\n{}",
                    offer.scope_key,
                    offer.issue_summary,
                    current_time_ms()
                ))
            ),
            scope_key: offer.scope_key.clone(),
            status: "running".to_string(),
            codex_thread_id: String::new(),
            context: offer.context.clone(),
            target_user_id: offer.target_user_id.clone(),
            issue_summary: offer.issue_summary.clone(),
            mod_info: mod_info.clone(),
            publish_folder_name: default_publish_folder_name(&mod_info),
            latest_artifact_path: String::new(),
            latest_artifact_name: String::new(),
            latest_artifact_version: String::new(),
            messages: vec![SessionMessage {
                role: "user".to_string(),
                speaker: get_sender_name(event),
                text: text.to_string(),
                created_at: now_iso(),
            }],
            bot_message_ids: Vec::new(),
            pending_rerun: false,
        };
        self.state_store
            .set_issue_repair_session(serde_json::to_value(&session)?)
            .await?;
        self.state_store.save().await?;

        let results = self
            .napcat_client
            .reply_text(
                &context.message_type,
                target_id(context),
                event.get("message_id").map(value_to_string).as_deref(),
                &format!(
                    "行，我开始跟这个 {} 问题，先直接修。",
                    if mod_info.display_name.is_empty() {
                        &mod_info.project_folder_name
                    } else {
                        &mod_info.display_name
                    }
                ),
            )
            .await?;
        session.bot_message_ids = extract_message_ids(&results);
        self.state_store
            .set_issue_repair_session(serde_json::to_value(&session)?)
            .await?;
        self.state_store.save().await?;
        self.spawn_session(session.id.clone(), "accepted");
        Ok(true)
    }

    async fn maybe_handle_session_reply(
        &self,
        mut session: IssueRepairSession,
        context: &EventContext,
        event: &Value,
        text: &str,
    ) -> Result<bool> {
        let reply_id = extract_reply_id(
            event.get("message").unwrap_or(&Value::Null),
            event.get("raw_message").and_then(Value::as_str),
        );
        let directly_replying = reply_id
            .as_ref()
            .map(|id| session.bot_message_ids.iter().any(|item| item == id))
            .unwrap_or(false);

        if !directly_replying {
            let raw = self
                .chat_client
                .complete(
                    &[
                        ChatMessage {
                            role: "system".to_string(),
                            content: Value::String(
                                [
                                    "你负责判断用户刚发的新消息，是否仍然是在和当前模组修复会话继续交流。",
                                    "输出 JSON：{\"is_followup\":boolean}。",
                                    "",
                                    "当前问题：",
                                    &session.issue_summary,
                                ]
                                .join("\n"),
                            ),
                        },
                        ChatMessage {
                            role: "user".to_string(),
                            content: Value::String(format!("用户最新消息：{text}")),
                        },
                    ],
                    CompleteOptions {
                        model: Some(self.config.followup_model.clone()),
                        temperature: Some(0.1),
                        allow_expensive_fallback: false,
                    },
                )
                .await?;
            if !parse_json_value(&raw)
                .and_then(|item| item.get("is_followup").and_then(Value::as_bool))
                .unwrap_or(false)
            {
                return Ok(false);
            }
        }

        session.messages.push(SessionMessage {
            role: "user".to_string(),
            speaker: get_sender_name(event),
            text: text.to_string(),
            created_at: now_iso(),
        });
        if session.messages.len() > 24 {
            let start = session.messages.len().saturating_sub(24);
            session.messages = session.messages[start..].to_vec();
        }
        self.state_store
            .set_issue_repair_session(serde_json::to_value(&session)?)
            .await?;
        self.state_store.save().await?;

        if session.status == "waiting-user-feedback" && self.is_satisfied(&session).await? {
            self.publish_and_close(
                session,
                context,
                &opt_value_to_string(event.get("message_id")),
            )
            .await?;
            return Ok(true);
        }

        if self.running_sessions.lock().await.contains(&session.id) {
            session.pending_rerun = true;
            self.state_store
                .set_issue_repair_session(serde_json::to_value(&session)?)
                .await?;
            self.state_store.save().await?;
            self.napcat_client
                .reply_text(
                    &context.message_type,
                    target_id(context),
                    event.get("message_id").map(value_to_string).as_deref(),
                    "我把这条也带上，当前这轮跑完后继续接。",
                )
                .await?;
            return Ok(true);
        }

        session.status = "running".to_string();
        self.state_store
            .set_issue_repair_session(serde_json::to_value(&session)?)
            .await?;
        self.state_store.save().await?;
        self.napcat_client
            .reply_text(
                &context.message_type,
                target_id(context),
                event.get("message_id").map(value_to_string).as_deref(),
                "继续看。",
            )
            .await?;
        self.spawn_session(session.id.clone(), "user-followup");
        Ok(true)
    }

    async fn is_satisfied(&self, session: &IssueRepairSession) -> Result<bool> {
        let raw = self
            .chat_client
            .complete(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: Value::String(
                            [
                                "你负责判断用户最新一条消息，是否意味着这轮修复已经可以收口。",
                                "输出 JSON：{\"accepted\":boolean}。",
                                "",
                                "当前问题：",
                                &session.issue_summary,
                            ]
                            .join("\n"),
                        ),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: Value::String(format!(
                            "用户最新消息：{}",
                            session
                                .messages
                                .last()
                                .map(|item| item.text.as_str())
                                .unwrap_or_default()
                        )),
                    },
                ],
                CompleteOptions {
                    model: Some(self.config.satisfaction_model.clone()),
                    temperature: Some(0.1),
                    allow_expensive_fallback: false,
                },
            )
            .await?;
        Ok(parse_json_value(&raw)
            .and_then(|item| item.get("accepted").and_then(Value::as_bool))
            .unwrap_or(false))
    }

    fn spawn_session(&self, session_id: String, reason: &'static str) {
        let manager = self.clone();
        tokio::spawn(async move {
            {
                let mut running = manager.running_sessions.lock().await;
                if running.contains(&session_id) {
                    return;
                }
                running.insert(session_id.clone());
            }
            if let Err(error) = manager.execute_session(&session_id, reason).await {
                manager
                    .logger
                    .warn(format!("修复会话执行失败 {session_id}: {error:#}"))
                    .await;
            }
            manager.running_sessions.lock().await.remove(&session_id);
        });
    }

    async fn execute_session(&self, session_id: &str, reason: &str) -> Result<()> {
        let Some(value) = self.state_store.get_issue_repair_session(session_id).await else {
            return Ok(());
        };
        let mut session: IssueRepairSession = serde_json::from_value(value)?;
        let work_dir = PathBuf::from(&session.mod_info.project_root);
        if session.mod_info.project_root.trim().is_empty() {
            bail!("会话缺少 mod projectRoot");
        }

        let session_dir = std::env::temp_dir()
            .join("napcat-cain-repair")
            .join(&session.id);
        ensure_dir(&session_dir).await?;
        let prompt_path = session_dir.join(format!("prompt-{}.txt", current_time_ms()));
        let schema_path = session_dir.join("output-schema.json");
        let output_path = session_dir.join(format!("last-message-{}.json", current_time_ms()));
        fs::write(&schema_path, serde_json::to_string_pretty(&codex_schema())?).await?;
        fs::write(
            &prompt_path,
            build_codex_prompt(&session, self.bridge_info.as_ref()),
        )
        .await?;

        let exec_result = run_codex(
            &self.config.codex_command,
            &self.config.model,
            &session.codex_thread_id,
            &schema_path,
            &output_path,
            &prompt_path,
            &work_dir,
            self.config.codex_timeout_ms.max(DEFAULT_CODEX_TIMEOUT_MS),
        )
        .await?;
        self.logger
            .info(format!(
                "修复会话 {} ({reason}) Codex 结束: code={} stdout={} stderr={}",
                session.id,
                exec_result.exit_code,
                truncate_text(&exec_result.stdout, 240),
                truncate_text(&exec_result.stderr, 240),
            ))
            .await;
        if !exec_result.thread_id.is_empty() {
            session.codex_thread_id = exec_result.thread_id;
        }
        if exec_result.exit_code != 0 {
            session.status = "failed".to_string();
            self.state_store
                .set_issue_repair_session(serde_json::to_value(&session)?)
                .await?;
            self.state_store.save().await?;
            return Ok(());
        }

        let parsed: CodexOutput = serde_json::from_str(
            &fs::read_to_string(&exec_result.output_path)
                .await
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        if !parsed.issue_summary.trim().is_empty() {
            session.issue_summary = normalize_issue_summary(&parsed.issue_summary);
        }
        if !parsed.publish_folder_name.trim().is_empty() {
            session.publish_folder_name = normalize_text(&parsed.publish_folder_name);
        }

        match parsed.status.trim() {
            "artifact_ready" => {
                session.status = "waiting-user-feedback".to_string();
                session.latest_artifact_path = normalize_text(&parsed.artifact_path);
                session.latest_artifact_name = if parsed.artifact_name.trim().is_empty() {
                    Path::new(&session.latest_artifact_path)
                        .file_name()
                        .and_then(|item| item.to_str())
                        .unwrap_or_default()
                        .to_string()
                } else {
                    normalize_text(&parsed.artifact_name)
                };
                session.latest_artifact_version = normalize_text(&parsed.artifact_version);
                if !parsed.assistant_message.trim().is_empty() {
                    session.messages.push(SessionMessage {
                        role: "assistant".to_string(),
                        speaker: "Cain".to_string(),
                        text: parsed.assistant_message.clone(),
                        created_at: now_iso(),
                    });
                }
                self.state_store
                    .set_issue_repair_session(serde_json::to_value(&session)?)
                    .await?;
                self.state_store.save().await?;
                self.send_artifact_for_feedback(session).await?;
            }
            "needs_user_reply" => {
                session.status = "waiting-user-input".to_string();
                if !parsed.assistant_message.trim().is_empty() {
                    session.messages.push(SessionMessage {
                        role: "assistant".to_string(),
                        speaker: "Cain".to_string(),
                        text: parsed.assistant_message.clone(),
                        created_at: now_iso(),
                    });
                }
                self.state_store
                    .set_issue_repair_session(serde_json::to_value(&session)?)
                    .await?;
                self.state_store.save().await?;
                let message = if parsed.assistant_message.trim().is_empty() {
                    "我还差一点信息，你补一句。".to_string()
                } else {
                    parsed.assistant_message.clone()
                };
                let result = self
                    .napcat_client
                    .send_context_message(&event_context(&session.context), Value::String(message))
                    .await?;
                session.bot_message_ids = extract_message_ids(&[result]);
                self.state_store
                    .set_issue_repair_session(serde_json::to_value(&session)?)
                    .await?;
                self.state_store.save().await?;
            }
            "done" => {
                session.status = "completed".to_string();
                self.state_store
                    .set_issue_repair_session(serde_json::to_value(&session)?)
                    .await?;
                self.state_store.save().await?;
                if !parsed.assistant_message.trim().is_empty() {
                    let result = self
                        .napcat_client
                        .send_context_message(
                            &event_context(&session.context),
                            Value::String(parsed.assistant_message.clone()),
                        )
                        .await?;
                    session.bot_message_ids = extract_message_ids(&[result]);
                    self.state_store
                        .set_issue_repair_session(serde_json::to_value(&session)?)
                        .await?;
                    self.state_store.save().await?;
                }
            }
            _ => {
                session.status = "failed".to_string();
                self.state_store
                    .set_issue_repair_session(serde_json::to_value(&session)?)
                    .await?;
                self.state_store.save().await?;
                let result = self
                    .napcat_client
                    .send_context_message(
                        &event_context(&session.context),
                        Value::String(if parsed.assistant_message.trim().is_empty() {
                            "现在还没法继续往下修。".to_string()
                        } else {
                            parsed.assistant_message.clone()
                        }),
                    )
                    .await?;
                session.bot_message_ids = extract_message_ids(&[result]);
                self.state_store
                    .set_issue_repair_session(serde_json::to_value(&session)?)
                    .await?;
                self.state_store.save().await?;
            }
        }

        let Some(value) = self.state_store.get_issue_repair_session(session_id).await else {
            return Ok(());
        };
        let mut latest: IssueRepairSession = serde_json::from_value(value)?;
        if latest.pending_rerun && latest.status != "completed" && latest.status != "failed" {
            latest.pending_rerun = false;
            latest.status = "running".to_string();
            self.state_store
                .set_issue_repair_session(serde_json::to_value(&latest)?)
                .await?;
            self.state_store.save().await?;
            self.spawn_session(latest.id.clone(), "queued-followup");
        }
        Ok(())
    }

    async fn send_artifact_for_feedback(&self, mut session: IssueRepairSession) -> Result<()> {
        if session.latest_artifact_path.trim().is_empty() {
            return Ok(());
        }
        let context = event_context(&session.context);
        let file_result = self
            .napcat_client
            .send_local_file_to_context(
                &context.message_type,
                target_id(&context),
                &session.latest_artifact_path,
                Some(&session.latest_artifact_name),
                None,
            )
            .await?;
        let text = session
            .messages
            .last()
            .map(|item| item.text.clone())
            .filter(|item| !item.trim().is_empty())
            .unwrap_or_else(|| "先试这个构建，看问题还在不在。".to_string());
        let message_result = if context.message_type == "group" {
            self.napcat_client
                .send_group_message(
                    &context.group_id,
                    json!([
                        { "type": "at", "data": { "qq": session.target_user_id } },
                        { "type": "text", "data": { "text": format!(" {text}") } }
                    ]),
                )
                .await?
        } else {
            self.napcat_client
                .send_private_message(&context.user_id, Value::String(text))
                .await?
        };
        session.bot_message_ids = extract_message_ids(&[file_result, message_result]);
        self.state_store
            .set_issue_repair_session(serde_json::to_value(&session)?)
            .await?;
        self.state_store.save().await?;
        Ok(())
    }

    async fn publish_and_close(
        &self,
        mut session: IssueRepairSession,
        context: &EventContext,
        reply_id: &str,
    ) -> Result<()> {
        if !session.latest_artifact_path.trim().is_empty()
            && !self.config.publish_group_id.trim().is_empty()
        {
            self.napcat_client
                .send_local_file_to_group(
                    &self.config.publish_group_id,
                    &session.latest_artifact_path,
                    Some(&session.latest_artifact_name),
                    Some(&session.publish_folder_name),
                    Some("修复产物已同步。"),
                )
                .await?;
        }
        session.status = "completed".to_string();
        self.state_store
            .set_issue_repair_session(serde_json::to_value(&session)?)
            .await?;
        self.state_store.save().await?;
        let reply_text = if !self.config.publish_group_id.trim().is_empty()
            && !session.publish_folder_name.trim().is_empty()
        {
            format!(
                "那我就按这版收口，已经同步到 {} 的 {} 里了。",
                self.config.publish_group_id, session.publish_folder_name
            )
        } else {
            "那我就按这版收口。".to_string()
        };
        self.napcat_client
            .reply_text(
                &context.message_type,
                target_id(context),
                Some(reply_id),
                &reply_text,
            )
            .await?;
        Ok(())
    }
}

struct CodexRunResult {
    exit_code: i32,
    stdout: String,
    stderr: String,
    output_path: PathBuf,
    thread_id: String,
}

async fn run_codex(
    command: &str,
    model: &str,
    thread_id: &str,
    schema_path: &Path,
    output_path: &Path,
    prompt_path: &Path,
    work_dir: &Path,
    timeout_ms: u64,
) -> Result<CodexRunResult> {
    let prompt = fs::read_to_string(prompt_path).await?;
    let resolved = resolve_command(command).await;
    let mut args = Vec::<String>::new();
    if !thread_id.trim().is_empty() {
        args.extend([
            "exec".to_string(),
            "resume".to_string(),
            thread_id.trim().to_string(),
        ]);
    } else {
        args.push("exec".to_string());
    }
    args.extend([
        "-m".to_string(),
        model.trim().to_string(),
        "--dangerously-bypass-approvals-and-sandbox".to_string(),
        "--skip-git-repo-check".to_string(),
        "--json".to_string(),
        "--color".to_string(),
        "never".to_string(),
        "--output-schema".to_string(),
        schema_path.display().to_string(),
        "-o".to_string(),
        output_path.display().to_string(),
        "-C".to_string(),
        work_dir.display().to_string(),
        "-".to_string(),
    ]);

    let mut cmd = spawnable_command(&resolved, &args);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("启动 Codex 失败: {resolved}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
        stdin.shutdown().await?;
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms.max(DEFAULT_CODEX_TIMEOUT_MS)),
        child.wait_with_output(),
    )
    .await
    .context("Codex 执行超时")??;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    Ok(CodexRunResult {
        exit_code: output.status.code().unwrap_or(-1),
        output_path: output_path.to_path_buf(),
        thread_id: extract_thread_id(&stdout),
        stdout,
        stderr,
    })
}

fn spawnable_command(command: &str, args: &[String]) -> Command {
    if cfg!(windows) && (command.ends_with(".cmd") || command.ends_with(".bat")) {
        let mut wrapped = Command::new(
            std::env::var("ComSpec")
                .unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".to_string()),
        );
        let joined = std::iter::once(quote_for_cmd(command))
            .chain(args.iter().map(|item| quote_for_cmd(item)))
            .collect::<Vec<_>>()
            .join(" ");
        wrapped.args(["/d", "/s", "/c", &joined]);
        wrapped
    } else {
        let mut direct = Command::new(command);
        direct.args(args);
        direct
    }
}

async fn resolve_command(command: &str) -> String {
    let normalized = normalize_text(command);
    if normalized.is_empty()
        || Path::new(&normalized).is_absolute()
        || normalized.contains('/')
        || normalized.contains('\\')
    {
        return if normalized.is_empty() {
            "codex".to_string()
        } else {
            normalized
        };
    }
    if cfg!(windows)
        && let Ok(output) = Command::new("where.exe").arg(&normalized).output().await
        && output.status.success()
    {
        let candidates = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(normalize_text)
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>();
        if let Some(preferred) = candidates
            .iter()
            .find(|item| item.ends_with(".cmd") || item.ends_with(".exe") || item.ends_with(".bat"))
        {
            return preferred.clone();
        }
        if let Some(first) = candidates.first() {
            return first.clone();
        }
    }
    normalized
}

fn quote_for_cmd(value: &str) -> String {
    if value.is_empty() {
        return "\"\"".to_string();
    }
    let escaped = value.replace('"', "\"\"");
    if escaped
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '"' | '&' | '<' | '>' | '|' | '^'))
    {
        format!("\"{escaped}\"")
    } else {
        escaped
    }
}

fn build_codex_prompt(
    session: &IssueRepairSession,
    bridge_info: Option<&CodexBridgeInfo>,
) -> String {
    let mod_name = if session.mod_info.display_name.is_empty() {
        &session.mod_info.project_folder_name
    } else {
        &session.mod_info.display_name
    };
    let mut lines = vec![
        "你是一个持续跟踪 QQ 用户模组问题的主 agent。你的任务不是只给建议，而是尽可能直接修改代码、构建本地产物、让用户测试，直到问题解决或明确无法继续。".to_string(),
        "如果信息仍不足，直接向用户提一个最小必要的问题，不要空谈。".to_string(),
        "最终输出必须严格符合 JSON Schema，不要输出任何额外文本。".to_string(),
        String::new(),
        format!("当前模组目录：{}", session.mod_info.project_root),
        format!("当前模组：{mod_name}"),
        format!("当前版本：{}", if session.mod_info.version.is_empty() { "-" } else { &session.mod_info.version }),
        format!("建议发布文件夹：{}", if session.publish_folder_name.is_empty() { "mod" } else { &session.publish_folder_name }),
        String::new(),
        "当前问题摘要：".to_string(),
        session.issue_summary.clone(),
        String::new(),
        "最近用户会话记录：".to_string(),
        session
            .messages
            .iter()
            .rev()
            .take(16)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .enumerate()
            .map(|(index, item)| format!("{}. {}: {}", index + 1, item.speaker, item.text))
            .collect::<Vec<_>>()
            .join("\n"),
    ];
    if let Some(bridge) = bridge_info {
        lines.extend([
            String::new(),
            "本机 Cain bridge：".to_string(),
            format!("baseUrl: {}", bridge.base_url),
            format!("send group message: {}", bridge.send_group_message_url),
            format!("send private message: {}", bridge.send_private_message_url),
            format!("read group messages: {}", bridge.read_group_messages_url),
            format!(
                "read private messages: {}",
                bridge.read_private_messages_url
            ),
            format!("send group file: {}", bridge.send_group_file_url),
            format!(
                "send group file to folder: {}",
                bridge.send_group_file_to_folder_url
            ),
            format!("read file: {}", bridge.read_file_url),
        ]);
    }
    lines.join("\n")
}

fn codex_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["status", "assistantMessage", "issueSummary", "artifactPath", "artifactName", "artifactVersion", "publishFolderName"],
        "properties": {
            "status": { "type": "string", "enum": ["needs_user_reply", "artifact_ready", "done", "failed"] },
            "assistantMessage": { "type": "string" },
            "issueSummary": { "type": "string" },
            "artifactPath": { "type": "string" },
            "artifactName": { "type": "string" },
            "artifactVersion": { "type": "string" },
            "publishFolderName": { "type": "string" }
        }
    })
}

fn event_context(context: &StoredContext) -> EventContext {
    EventContext {
        message_type: context.message_type.clone(),
        group_id: context.group_id.clone(),
        user_id: context.user_id.clone(),
        self_id: context.self_id.clone(),
    }
}

fn stored_context(context: &EventContext) -> StoredContext {
    StoredContext {
        message_type: context.message_type.clone(),
        group_id: context.group_id.clone(),
        user_id: context.user_id.clone(),
        self_id: context.self_id.clone(),
    }
}

fn target_id(context: &EventContext) -> &str {
    if context.message_type == "group" {
        &context.group_id
    } else {
        &context.user_id
    }
}

fn build_scope_key(context: &EventContext) -> String {
    if context.message_type == "group" {
        format!("group:{}:user:{}", context.group_id, context.user_id)
    } else {
        format!("private:{}", context.user_id)
    }
}

fn can_offer_issue_repair(config: &IssueRepairConfig, context: &EventContext) -> bool {
    context.message_type == "group"
        && !config.offer_group_ids.is_empty()
        && config
            .offer_group_ids
            .iter()
            .map(normalize_text)
            .any(|item| item == context.group_id)
}

fn default_publish_folder_name(mod_info: &ModInfo) -> String {
    normalize_text(if mod_info.project_folder_name.is_empty() {
        "mod"
    } else {
        &mod_info.project_folder_name
    })
    .chars()
    .map(|ch| if "\\/:*?\"<>|".contains(ch) { '_' } else { ch })
    .collect()
}

fn normalize_issue_summary(value: impl AsRef<str>) -> String {
    let normalized = normalize_text(value);
    if normalized.is_empty() {
        "用户反馈该模组存在 bug 或体验问题".to_string()
    } else {
        normalized
    }
}

fn normalize_text(value: impl AsRef<str>) -> String {
    value.as_ref().replace("\r\n", "\n").trim().to_string()
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let normalized = normalize_text(text);
    if normalized.chars().count() <= max_chars {
        normalized
    } else {
        format!(
            "{}...(已截断)",
            normalized.chars().take(max_chars).collect::<String>()
        )
    }
}

fn parse_json_value(text: &str) -> Option<Value> {
    serde_json::from_str(text.trim()).ok().or_else(|| {
        let start = text.find('{')?;
        let end = text.rfind('}')?;
        serde_json::from_str(&text[start..=end]).ok()
    })
}

fn extract_thread_id(text: &str) -> String {
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("thread.started")
            && let Some(thread_id) = value.get("thread_id").and_then(Value::as_str)
        {
            return normalize_text(thread_id);
        }
    }
    String::new()
}

fn extract_message_ids(results: &[Value]) -> Vec<String> {
    fn visit(value: &Value, ids: &mut Vec<String>) {
        match value {
            Value::Array(items) => {
                for item in items {
                    visit(item, ids);
                }
            }
            Value::Object(object) => {
                if let Some(message_id) = object
                    .get("message_id")
                    .or_else(|| object.get("messageId"))
                    .map(value_to_string)
                    .filter(|item| !item.is_empty())
                {
                    ids.push(message_id);
                }
                for value in object.values() {
                    visit(value, ids);
                }
            }
            _ => {}
        }
    }
    let mut ids = Vec::new();
    for value in results {
        visit(value, &mut ids);
    }
    ids.sort();
    ids.dedup();
    ids
}

fn extract_reply_id(message: &Value, raw_message: Option<&str>) -> Option<String> {
    if let Some(items) = message.as_array()
        && let Some(reply) = items
            .iter()
            .find(|segment| segment.get("type").and_then(Value::as_str) == Some("reply"))
    {
        return reply
            .get("data")
            .and_then(|data| data.get("id"))
            .map(value_to_string)
            .filter(|item| !item.is_empty());
    }
    raw_message.and_then(|raw| {
        let marker = "[CQ:reply,id=";
        let start = raw.find(marker)?;
        let remain = &raw[start + marker.len()..];
        let end = remain.find([',', ']']).unwrap_or(remain.len());
        let reply_id = remain[..end].trim();
        (!reply_id.is_empty()).then(|| reply_id.to_string())
    })
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

fn opt_value_to_string(value: Option<&Value>) -> String {
    value.map(value_to_string).unwrap_or_default()
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|item| item.as_millis() as u64)
        .unwrap_or_default()
}

fn scan_owned_mods(codex_root: &Path, owner_name: &str) -> Result<Vec<ModInfo>> {
    let mut stack = vec![codex_root.to_path_buf()];
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.')
                    || [
                        "node_modules",
                        "build",
                        "bin",
                        "dist",
                        "release",
                        "out",
                        "target",
                        "构建",
                    ]
                    .contains(&name.as_str())
                {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let file_name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if file_name != "mod.json" && file_name != "mod.hjson" {
                continue;
            }
            let Some(project_root) = project_root_from_mod_file(&path) else {
                continue;
            };
            if !seen.insert(project_root.clone()) {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let author = read_loose_field(&raw, "author");
            if author.is_empty()
                || !author
                    .to_ascii_lowercase()
                    .contains(&owner_name.trim().to_ascii_lowercase())
            {
                continue;
            }
            results.push(ModInfo {
                id: project_root
                    .file_name()
                    .and_then(|item| item.to_str())
                    .unwrap_or_default()
                    .to_string(),
                project_root: project_root.display().to_string(),
                project_folder_name: project_root
                    .file_name()
                    .and_then(|item| item.to_str())
                    .unwrap_or_default()
                    .to_string(),
                name: read_loose_field(&raw, "name"),
                display_name: read_loose_field(&raw, "displayName"),
                version: read_loose_field(&raw, "version"),
                description: read_loose_field(&raw, "description"),
            });
        }
    }
    Ok(results)
}

fn project_root_from_mod_file(path: &Path) -> Option<PathBuf> {
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if normalized.contains("/node_modules/") || normalized.contains("/.git/") {
        return None;
    }
    if normalized.contains("/build/")
        || normalized.contains("/bin/")
        || normalized.contains("/dist/")
        || normalized.contains("/release/")
        || normalized.contains("/构建/")
    {
        return None;
    }
    if normalized.contains("/src/main/resources/") {
        return path.parent()?.parent()?.parent().map(Path::to_path_buf);
    }
    path.parent().map(Path::to_path_buf)
}

fn read_loose_field(text: &str, field_name: &str) -> String {
    let variants = [format!("\"{field_name}\""), field_name.to_string()];
    for key in variants {
        let Some(start) = text.find(&key) else {
            continue;
        };
        let tail = &text[start + key.len()..];
        let Some(colon) = tail.find(':') else {
            continue;
        };
        let mut value = tail[colon + 1..].trim_start();
        if let Some(rest) = value.strip_prefix('"') {
            if let Some(end) = rest.find('"') {
                return normalize_text(&rest[..end]);
            }
        }
        if let Some(rest) = value.strip_prefix('\'') {
            if let Some(end) = rest.find('\'') {
                return normalize_text(&rest[..end]);
            }
        }
        let end = value.find([',', '\n', '\r', '}']).unwrap_or(value.len());
        value = &value[..end];
        return normalize_text(value.trim_matches(['"', '\'']));
    }
    String::new()
}
