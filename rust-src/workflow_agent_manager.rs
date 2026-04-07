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
use crate::config::WorkflowAgentConfig;
use crate::event_utils::{EventContext, get_sender_name};
use crate::logger::Logger;
use crate::napcat_client::NapCatClient;
use crate::openai_chat_client::{ChatMessage, CompleteOptions, OpenAiChatClient};
use crate::state_store::StateStore;
use crate::utils::{ensure_dir, now_iso, sha1_hex};

const DEFAULT_CODEX_TIMEOUT_MS: u64 = 30 * 60 * 1000;

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
struct WorkflowMessage {
    role: String,
    speaker: String,
    text: String,
    #[serde(rename = "createdAt")]
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkflowSession {
    id: String,
    #[serde(rename = "scopeKey")]
    scope_key: String,
    status: String,
    #[serde(rename = "codexThreadId")]
    codex_thread_id: String,
    context: StoredContext,
    #[serde(rename = "targetUserId")]
    target_user_id: String,
    #[serde(rename = "taskSummary")]
    task_summary: String,
    #[serde(rename = "latestArtifactPath")]
    latest_artifact_path: String,
    #[serde(rename = "latestArtifactName")]
    latest_artifact_name: String,
    messages: Vec<WorkflowMessage>,
    #[serde(rename = "botMessageIds")]
    bot_message_ids: Vec<String>,
    #[serde(rename = "pendingRerun")]
    pending_rerun: bool,
    #[serde(rename = "createdAt")]
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CodexOutput {
    status: String,
    #[serde(rename = "assistantMessage")]
    assistant_message: String,
    #[serde(rename = "taskSummary")]
    task_summary: String,
    #[serde(rename = "artifactPath")]
    artifact_path: String,
    #[serde(rename = "artifactName")]
    artifact_name: String,
}

#[derive(Clone)]
pub struct WorkflowAgentManager {
    config: WorkflowAgentConfig,
    chat_client: OpenAiChatClient,
    napcat_client: NapCatClient,
    state_store: StateStore,
    logger: Logger,
    bridge_info: Option<CodexBridgeInfo>,
    running_sessions: Arc<Mutex<HashSet<String>>>,
    owner_user_id: String,
    default_work_dir: PathBuf,
}

impl WorkflowAgentManager {
    pub fn new(
        config: WorkflowAgentConfig,
        chat_client: OpenAiChatClient,
        napcat_client: NapCatClient,
        state_store: StateStore,
        logger: Logger,
        bridge_info: Option<CodexBridgeInfo>,
        owner_user_id: String,
        default_work_dir: PathBuf,
    ) -> Self {
        Self {
            config,
            chat_client,
            napcat_client,
            state_store,
            logger,
            bridge_info,
            running_sessions: Default::default(),
            owner_user_id,
            default_work_dir,
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn allow_explicit_request(&self, context: &EventContext) -> bool {
        if !self.config.enabled {
            return false;
        }
        if self.config.owner_only && context.user_id != self.owner_user_id {
            return false;
        }
        if context.message_type == "private" {
            return self.config.allow_private;
        }
        true
    }

    pub async fn handle_explicit_request(
        &self,
        context: &EventContext,
        event: &Value,
        task_text: &str,
    ) -> Result<bool> {
        let normalized = normalize_task_summary(task_text);
        if normalized.is_empty() {
            bail!("/agent 后必须跟要执行的任务。");
        }
        if !self.allow_explicit_request(context) {
            bail!("当前上下文不允许启动通用 workflow agent。");
        }

        let scope_key = build_scope_key(context);
        if let Some(session) = self.find_active_session(&scope_key).await? {
            return self
                .maybe_handle_session_reply(session, context, event, &normalized, true)
                .await;
        }

        self.start_new_session(context, event, &normalized, Some("行，我开始处理这件事。"))
            .await?;
        Ok(true)
    }

    pub async fn handle_incoming_message(
        &self,
        context: &EventContext,
        event: &Value,
        text: &str,
        mentioned_self: bool,
    ) -> Result<bool> {
        if !self.config.enabled {
            return Ok(false);
        }
        let normalized = normalize_text(text);
        if normalized.is_empty() {
            return Ok(false);
        }

        let scope_key = build_scope_key(context);
        if let Some(session) = self.find_active_session(&scope_key).await?
            && self
                .maybe_handle_session_reply(session, context, event, &normalized, false)
                .await?
        {
            return Ok(true);
        }

        if !self.can_auto_trigger(context, mentioned_self) {
            return Ok(false);
        }

        let decision = self.classify_candidate(context, event, &normalized).await?;
        if !decision
            .get("should_start")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(false);
        }

        let summary = normalize_task_summary(opt_value_to_string(decision.get("task_summary")));
        let task_summary = if summary.is_empty() {
            normalize_task_summary(&normalized)
        } else {
            summary
        };
        self.start_new_session(
            context,
            event,
            &task_summary,
            Some("收到，我开始按这个任务跑工作流。"),
        )
        .await?;
        Ok(true)
    }

    pub async fn maybe_handoff_from_chat(
        &self,
        context: &EventContext,
        event: &Value,
        source_text: &str,
        assistant_draft: &str,
    ) -> Result<bool> {
        if !self.allow_explicit_request(context) {
            return Ok(false);
        }
        let normalized_source = normalize_text(source_text);
        let normalized_draft = normalize_text(assistant_draft);
        if normalized_source.is_empty() || normalized_draft.is_empty() {
            return Ok(false);
        }

        let raw = self
            .chat_client
            .complete(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: Value::String(
                            [
                                "你负责判断：当前这轮聊天是否应该从普通问答切换成主动 workflow agent。",
                                "只有当用户真正需要 Cain 主动执行、多步推进、检查环境、运行工具、修改内容、生成产物或持续跟进时，should_handoff 才为 true。",
                                "不要因为回复草稿用了未来式、口头承诺或语气含糊就轻易切换；核心看用户请求本身是否属于“需要主动做事”而不是“直接回答就够”。",
                                "输出 JSON：{\"should_handoff\":boolean,\"task_summary\":\"一句话任务摘要\",\"assistant_message\":\"切换时给用户的简短说明\"}。",
                                "如果 should_handoff=false，则 task_summary 和 assistant_message 都留空字符串。",
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
                                format!("用户原话：{normalized_source}"),
                                format!("当前聊天回复草稿：{normalized_draft}"),
                            ]
                            .join("\n"),
                        ),
                    },
                ],
                CompleteOptions {
                    model: Some(self.config.classify_model.clone()),
                    temperature: Some(0.1),
                },
            )
            .await?;
        let decision = parse_json_value(&raw).unwrap_or_else(|| json!({ "should_handoff": false }));
        if !decision
            .get("should_handoff")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(false);
        }

        let task_summary =
            normalize_task_summary(opt_value_to_string(decision.get("task_summary")));
        if task_summary.is_empty() {
            return Ok(false);
        }
        let announce = normalize_text(opt_value_to_string(decision.get("assistant_message")));
        let scope_key = build_scope_key(context);
        if let Some(session) = self.find_active_session(&scope_key).await? {
            return self
                .maybe_handle_session_reply(session, context, event, &task_summary, true)
                .await;
        }

        let ack = if announce.is_empty() {
            Some("这件事更适合直接拉起工作流，我开始处理。")
        } else {
            Some(announce.as_str())
        };
        self.start_new_session(context, event, &task_summary, ack)
            .await?;
        Ok(true)
    }

    fn can_auto_trigger(&self, context: &EventContext, mentioned_self: bool) -> bool {
        if self.config.owner_only && context.user_id != self.owner_user_id {
            return false;
        }
        if context.message_type == "private" {
            return self.config.allow_private;
        }
        if !self.config.auto_trigger_on_mention || !mentioned_self {
            return false;
        }
        !self.config.trigger_group_ids.is_empty()
            && self
                .config
                .trigger_group_ids
                .iter()
                .map(normalize_text)
                .any(|item| item == context.group_id)
    }

    async fn start_new_session(
        &self,
        context: &EventContext,
        event: &Value,
        task_summary: &str,
        ack_text: Option<&str>,
    ) -> Result<()> {
        let normalized_task = normalize_task_summary(task_summary);
        let mut session = WorkflowSession {
            id: format!(
                "workflow-{}",
                sha1_hex(format!(
                    "{}\n{}\n{}",
                    build_scope_key(context),
                    normalized_task,
                    current_time_ms()
                ))
            ),
            scope_key: build_scope_key(context),
            status: "running".to_string(),
            codex_thread_id: String::new(),
            context: stored_context(context),
            target_user_id: context.user_id.clone(),
            task_summary: normalized_task.clone(),
            latest_artifact_path: String::new(),
            latest_artifact_name: String::new(),
            messages: vec![WorkflowMessage {
                role: "user".to_string(),
                speaker: get_sender_name(event),
                text: normalized_task.clone(),
                created_at: now_iso(),
            }],
            bot_message_ids: Vec::new(),
            pending_rerun: false,
            created_at: now_iso(),
        };
        self.state_store
            .set_workflow_agent_session(serde_json::to_value(&session)?)
            .await?;
        self.state_store.save().await?;

        if let Some(ack_text) = ack_text.map(str::trim).filter(|item| !item.is_empty()) {
            let results = self
                .napcat_client
                .reply_text(
                    &context.message_type,
                    target_id(context),
                    event.get("message_id").map(value_to_string).as_deref(),
                    &format!("{ack_text}\n任务摘要：{normalized_task}"),
                )
                .await?;
            session.bot_message_ids = extract_message_ids(&results);
            self.state_store
                .set_workflow_agent_session(serde_json::to_value(&session)?)
                .await?;
            self.state_store.save().await?;
        }

        self.spawn_session(session.id.clone(), "accepted");
        Ok(())
    }

    async fn find_active_session(&self, scope_key: &str) -> Result<Option<WorkflowSession>> {
        for value in self.state_store.list_workflow_agent_sessions().await {
            let Ok(session) = serde_json::from_value::<WorkflowSession>(value) else {
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
        let raw = self
            .chat_client
            .complete(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: Value::String(
                            [
                                "你负责判断一条 QQ 消息，是否是在要求 Cain 主动发起多步工作流。",
                                "只有当用户是在要求 Cain 主动执行、多步推进、检查环境、运行工具、处理文件、生成产物、修改内容或持续跟进时，should_start 才为 true。",
                                "如果用户的问题可以被直接解释、回答、讨论或给建议解决，不需要真正动手执行，则 should_start=false。",
                                "输出 JSON：{\"should_start\":boolean,\"task_summary\":\"一句话任务摘要\"}。",
                                "如果 should_start=false，则 task_summary 置空字符串。",
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
                },
            )
            .await?;
        Ok(parse_json_value(&raw).unwrap_or_else(|| json!({ "should_start": false })))
    }

    async fn maybe_handle_session_reply(
        &self,
        mut session: WorkflowSession,
        context: &EventContext,
        event: &Value,
        text: &str,
        explicit: bool,
    ) -> Result<bool> {
        let reply_id = extract_reply_id(
            event.get("message").unwrap_or(&Value::Null),
            event.get("raw_message").and_then(Value::as_str),
        );
        let directly_replying = reply_id
            .as_ref()
            .map(|id| session.bot_message_ids.iter().any(|item| item == id))
            .unwrap_or(false);

        if !explicit && !directly_replying {
            let raw = self
                .chat_client
                .complete(
                    &[
                        ChatMessage {
                            role: "system".to_string(),
                            content: Value::String(
                                [
                                    "你负责判断用户刚发的新消息，是否仍然是在和当前通用工作流会话继续交流。",
                                    "输出 JSON：{\"is_followup\":boolean}。",
                                    "",
                                    "当前任务：",
                                    &session.task_summary,
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

        session.messages.push(WorkflowMessage {
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
            .set_workflow_agent_session(serde_json::to_value(&session)?)
            .await?;
        self.state_store.save().await?;

        if session.status == "waiting-user-feedback" && self.is_satisfied(&session).await? {
            self.close_session(
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
                .set_workflow_agent_session(serde_json::to_value(&session)?)
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
            .set_workflow_agent_session(serde_json::to_value(&session)?)
            .await?;
        self.state_store.save().await?;
        self.napcat_client
            .reply_text(
                &context.message_type,
                target_id(context),
                event.get("message_id").map(value_to_string).as_deref(),
                if explicit {
                    "这条我并到当前工作流里继续跑。"
                } else {
                    "继续处理。"
                },
            )
            .await?;
        self.spawn_session(session.id.clone(), "user-followup");
        Ok(true)
    }

    async fn is_satisfied(&self, session: &WorkflowSession) -> Result<bool> {
        let raw = self
            .chat_client
            .complete(
                &[
                    ChatMessage {
                        role: "system".to_string(),
                        content: Value::String(
                            [
                                "你负责判断用户最新一条消息，是否意味着当前工作流已经可以收口。",
                                "用户如果表示问题解决、结果可以、收到并确认结束，则 accepted=true。",
                                "如果用户表示还有问题、继续改、继续查、继续跟进，则 accepted=false。",
                                "输出 JSON：{\"accepted\":boolean}。",
                                "",
                                "当前任务：",
                                &session.task_summary,
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
                    .warn(format!("workflow 会话执行失败 {session_id}: {error:#}"))
                    .await;
            }
            manager.running_sessions.lock().await.remove(&session_id);
        });
    }
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
    session: &WorkflowSession,
    bridge_info: Option<&CodexBridgeInfo>,
    work_dir: &Path,
) -> String {
    let history = session
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
        .join("\n");
    let mut lines = vec![
        "你是 Cain 的通用主动工作流 agent。你的职责是把用户委托的事情往前推进，而不是停留在建议层。".to_string(),
        "优先自己检查文件、运行命令、写代码、下载工具、处理素材、构建、测试、整理结果。".to_string(),
        "能直接完成就直接完成，只有在真的缺少关键信息时才停下来问用户。".to_string(),
        "如果出现长耗时步骤，可以主动汇报关键里程碑，但不要碎碎念；通常一轮 2 到 4 条进度消息就够了。".to_string(),
        "如果你需要用户补信息，请在最终 JSON 中返回 status=needs_user_reply 和 assistantMessage，不要只靠中途发消息发问。".to_string(),
        "如果产出了要交付给用户查看、测试或接收的本地文件，不要自己上传，直接在最终 JSON 里返回 artifactPath 和 artifactName。artifactPath 尽量使用绝对路径。".to_string(),
        "最终输出必须严格符合 JSON Schema，不要输出任何额外文本。".to_string(),
        String::new(),
        format!("当前工作目录：{}", work_dir.display()),
        format!(
            "当前会话位置：{}",
            if session.context.message_type == "group" {
                format!("群 {} / 用户 {}", session.context.group_id, session.context.user_id)
            } else {
                format!("私聊 {}", session.context.user_id)
            }
        ),
        format!("当前任务摘要：{}", session.task_summary),
        String::new(),
        "最近会话记录：".to_string(),
        if history.is_empty() {
            "(空)".to_string()
        } else {
            history
        },
        String::new(),
        "最终 JSON 字段约定：".to_string(),
        "1. status=needs_user_reply：当前必须等待用户补信息。assistantMessage 写你要问的最小必要问题。".to_string(),
        "2. status=artifact_ready：已经产出本地文件给用户接收。assistantMessage 写简短说明和下一步测试/查看建议，并填写 artifactPath/artifactName。".to_string(),
        "3. status=done：任务已完成且不需要发送文件。assistantMessage 写完成摘要。".to_string(),
        "4. status=failed：当前无法继续。assistantMessage 写明确阻塞点。".to_string(),
    ];

    if let Some(bridge) = bridge_info {
        let progress_target = if session.context.message_type == "group" {
            json!({
                "groupId": session.context.group_id,
                "atUserIds": [session.target_user_id]
            })
        } else {
            json!({
                "userId": session.context.user_id
            })
        };
        let progress_example = if session.context.message_type == "group" {
            json!({
                "groupId": session.context.group_id,
                "atUserIds": [session.target_user_id],
                "text": "开始处理，先定位问题。"
            })
        } else {
            json!({
                "userId": session.context.user_id,
                "text": "开始处理，先定位问题。"
            })
        };
        lines.extend([
            String::new(),
            "Cain bridge 可用于主动汇报：".to_string(),
            format!("Authorization 头：{}", if bridge.authorization_header.trim().is_empty() { "(无需)" } else { &bridge.authorization_header }),
            format!("sendGroupMessage: {}", bridge.send_group_message_url),
            format!("sendPrivateMessage: {}", bridge.send_private_message_url),
            format!("readGroupMessages: {}", bridge.read_group_messages_url),
            format!("readPrivateMessages: {}", bridge.read_private_messages_url),
            format!("sendGroupFile: {}", bridge.send_group_file_url),
            format!("sendGroupFileToFolder: {}", bridge.send_group_file_to_folder_url),
            format!("readFile: {}", bridge.read_file_url),
            format!(
                "当前建议的进度消息目标 JSON：{}",
                serde_json::to_string_pretty(&progress_target).unwrap_or_default()
            ),
            format!(
                "示例进度消息 JSON：{}",
                serde_json::to_string_pretty(&progress_example).unwrap_or_default()
            ),
            "主动汇报适合这些时机：开始处理、进入长下载/长构建、发现关键结论、准备交付结果。不要为了每个小步骤发消息。".to_string(),
        ]);
    }
    lines.join("\n")
}

fn codex_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["status", "assistantMessage", "taskSummary", "artifactPath", "artifactName"],
        "properties": {
            "status": { "type": "string", "enum": ["needs_user_reply", "artifact_ready", "done", "failed"] },
            "assistantMessage": { "type": "string" },
            "taskSummary": { "type": "string" },
            "artifactPath": { "type": "string" },
            "artifactName": { "type": "string" }
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

fn normalize_task_summary(value: impl AsRef<str>) -> String {
    let normalized = normalize_text(value);
    if normalized.is_empty() {
        String::new()
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

struct CodexRunResult {
    exit_code: i32,
    stdout: String,
    stderr: String,
    output_path: PathBuf,
    thread_id: String,
}

impl WorkflowAgentManager {
    async fn execute_session(&self, session_id: &str, reason: &str) -> Result<()> {
        let Some(value) = self
            .state_store
            .get_workflow_agent_session(session_id)
            .await
        else {
            return Ok(());
        };
        let mut session: WorkflowSession = serde_json::from_value(value)?;
        let work_dir = self
            .config
            .work_dir
            .clone()
            .unwrap_or_else(|| self.default_work_dir.clone());
        if work_dir.as_os_str().is_empty() {
            bail!("workflow agent 缺少 work_dir");
        }
        ensure_dir(&work_dir).await?;

        let session_dir = std::env::temp_dir()
            .join("napcat-cain-workflow")
            .join(&session.id);
        ensure_dir(&session_dir).await?;
        let prompt_path = session_dir.join(format!("prompt-{}.txt", current_time_ms()));
        let schema_path = session_dir.join("output-schema.json");
        let output_path = session_dir.join(format!("last-message-{}.json", current_time_ms()));
        fs::write(&schema_path, serde_json::to_string_pretty(&codex_schema())?).await?;
        fs::write(
            &prompt_path,
            build_codex_prompt(&session, self.bridge_info.as_ref(), &work_dir),
        )
        .await?;

        let exec_result = match run_codex(
            &self.config.codex_command,
            &self.config.model,
            &session.codex_thread_id,
            &schema_path,
            &output_path,
            &prompt_path,
            &work_dir,
            self.config.codex_timeout_ms.max(DEFAULT_CODEX_TIMEOUT_MS),
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                session.status = "failed".to_string();
                self.state_store
                    .set_workflow_agent_session(serde_json::to_value(&session)?)
                    .await?;
                self.state_store.save().await?;
                let result = self
                    .napcat_client
                    .send_context_message(
                        &event_context(&session.context),
                        Value::String(format!(
                            "这轮工作流没跑起来，先停在这里了：{}",
                            truncate_text(&error.to_string(), 180)
                        )),
                    )
                    .await?;
                session.bot_message_ids = extract_message_ids(&[result]);
                self.state_store
                    .set_workflow_agent_session(serde_json::to_value(&session)?)
                    .await?;
                self.state_store.save().await?;
                return Ok(());
            }
        };
        self.logger
            .info(format!(
                "workflow 会话 {} ({reason}) Codex 结束: code={} stdout={} stderr={}",
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
                .set_workflow_agent_session(serde_json::to_value(&session)?)
                .await?;
            self.state_store.save().await?;
            let result = self
                .napcat_client
                .send_context_message(
                    &event_context(&session.context),
                    Value::String("这轮工作流中断了，执行器返回了非零退出码。你如果还要我继续，可以补一句继续或给更明确限制。".to_string()),
                )
                .await?;
            session.bot_message_ids = extract_message_ids(&[result]);
            self.state_store
                .set_workflow_agent_session(serde_json::to_value(&session)?)
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
        if !parsed.task_summary.trim().is_empty() {
            session.task_summary = normalize_task_summary(&parsed.task_summary);
        }

        match parsed.status.trim() {
            "artifact_ready" => {
                let raw_artifact_path = normalize_text(&parsed.artifact_path);
                if raw_artifact_path.is_empty() {
                    session.status = "failed".to_string();
                    self.state_store
                        .set_workflow_agent_session(serde_json::to_value(&session)?)
                        .await?;
                    self.state_store.save().await?;
                    let result = self
                        .napcat_client
                        .send_context_message(
                            &event_context(&session.context),
                            Value::String(if parsed.assistant_message.trim().is_empty() {
                                "这轮任务声称已经产出结果，但没有返回 artifactPath，没法自动发送文件。".to_string()
                            } else {
                                format!(
                                    "{}\n另外这轮没有返回 artifactPath，没法自动发送文件。",
                                    parsed.assistant_message.trim()
                                )
                            }),
                        )
                        .await?;
                    session.bot_message_ids = extract_message_ids(&[result]);
                    self.state_store
                        .set_workflow_agent_session(serde_json::to_value(&session)?)
                        .await?;
                    self.state_store.save().await?;
                } else {
                    session.status = "waiting-user-feedback".to_string();
                    session.latest_artifact_path = if Path::new(&raw_artifact_path).is_absolute() {
                        raw_artifact_path
                    } else {
                        work_dir.join(&raw_artifact_path).display().to_string()
                    };
                    session.latest_artifact_name = if parsed.artifact_name.trim().is_empty() {
                        Path::new(&session.latest_artifact_path)
                            .file_name()
                            .and_then(|item| item.to_str())
                            .unwrap_or_default()
                            .to_string()
                    } else {
                        normalize_text(&parsed.artifact_name)
                    };
                    if !parsed.assistant_message.trim().is_empty() {
                        session.messages.push(WorkflowMessage {
                            role: "assistant".to_string(),
                            speaker: "Cain".to_string(),
                            text: parsed.assistant_message.clone(),
                            created_at: now_iso(),
                        });
                    }
                    self.state_store
                        .set_workflow_agent_session(serde_json::to_value(&session)?)
                        .await?;
                    self.state_store.save().await?;
                    self.send_artifact_for_feedback(session).await?;
                }
            }
            "needs_user_reply" => {
                session.status = "waiting-user-input".to_string();
                if !parsed.assistant_message.trim().is_empty() {
                    session.messages.push(WorkflowMessage {
                        role: "assistant".to_string(),
                        speaker: "Cain".to_string(),
                        text: parsed.assistant_message.clone(),
                        created_at: now_iso(),
                    });
                }
                self.state_store
                    .set_workflow_agent_session(serde_json::to_value(&session)?)
                    .await?;
                self.state_store.save().await?;
                let message = if parsed.assistant_message.trim().is_empty() {
                    "我还差一点关键信息，你补一句。".to_string()
                } else {
                    parsed.assistant_message.clone()
                };
                let result = self
                    .napcat_client
                    .send_context_message(&event_context(&session.context), Value::String(message))
                    .await?;
                session.bot_message_ids = extract_message_ids(&[result]);
                self.state_store
                    .set_workflow_agent_session(serde_json::to_value(&session)?)
                    .await?;
                self.state_store.save().await?;
            }
            "done" => {
                session.status = "completed".to_string();
                if !parsed.assistant_message.trim().is_empty() {
                    session.messages.push(WorkflowMessage {
                        role: "assistant".to_string(),
                        speaker: "Cain".to_string(),
                        text: parsed.assistant_message.clone(),
                        created_at: now_iso(),
                    });
                }
                self.state_store
                    .set_workflow_agent_session(serde_json::to_value(&session)?)
                    .await?;
                self.state_store.save().await?;
                let result = self
                    .napcat_client
                    .send_context_message(
                        &event_context(&session.context),
                        Value::String(if parsed.assistant_message.trim().is_empty() {
                            "这轮任务已经处理完了。".to_string()
                        } else {
                            parsed.assistant_message.clone()
                        }),
                    )
                    .await?;
                session.bot_message_ids = extract_message_ids(&[result]);
                self.state_store
                    .set_workflow_agent_session(serde_json::to_value(&session)?)
                    .await?;
                self.state_store.save().await?;
            }
            "failed" | "" | _ => {
                session.status = "failed".to_string();
                if !parsed.assistant_message.trim().is_empty() {
                    session.messages.push(WorkflowMessage {
                        role: "assistant".to_string(),
                        speaker: "Cain".to_string(),
                        text: parsed.assistant_message.clone(),
                        created_at: now_iso(),
                    });
                }
                self.state_store
                    .set_workflow_agent_session(serde_json::to_value(&session)?)
                    .await?;
                self.state_store.save().await?;
                let result = self
                    .napcat_client
                    .send_context_message(
                        &event_context(&session.context),
                        Value::String(if parsed.assistant_message.trim().is_empty() {
                            "现在还没法继续往下推进。".to_string()
                        } else {
                            parsed.assistant_message.clone()
                        }),
                    )
                    .await?;
                session.bot_message_ids = extract_message_ids(&[result]);
                self.state_store
                    .set_workflow_agent_session(serde_json::to_value(&session)?)
                    .await?;
                self.state_store.save().await?;
            }
        }

        let Some(value) = self
            .state_store
            .get_workflow_agent_session(session_id)
            .await
        else {
            return Ok(());
        };
        let mut latest: WorkflowSession = serde_json::from_value(value)?;
        if latest.pending_rerun && latest.status != "completed" && latest.status != "failed" {
            latest.pending_rerun = false;
            latest.status = "running".to_string();
            self.state_store
                .set_workflow_agent_session(serde_json::to_value(&latest)?)
                .await?;
            self.state_store.save().await?;
            self.spawn_session(latest.id.clone(), "queued-followup");
        }
        Ok(())
    }

    async fn send_artifact_for_feedback(&self, mut session: WorkflowSession) -> Result<()> {
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
            .iter()
            .rev()
            .find(|item| item.role == "assistant" && !item.text.trim().is_empty())
            .map(|item| item.text.clone())
            .unwrap_or_else(|| "我先把这版结果发你，你看下是否符合预期。".to_string());
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
            .set_workflow_agent_session(serde_json::to_value(&session)?)
            .await?;
        self.state_store.save().await?;
        Ok(())
    }

    async fn close_session(
        &self,
        mut session: WorkflowSession,
        context: &EventContext,
        reply_id: &str,
    ) -> Result<()> {
        session.status = "completed".to_string();
        self.state_store
            .set_workflow_agent_session(serde_json::to_value(&session)?)
            .await?;
        self.state_store.save().await?;
        let results = self
            .napcat_client
            .reply_text(
                &context.message_type,
                target_id(context),
                (!reply_id.trim().is_empty()).then_some(reply_id),
                "那我就按这版收口。",
            )
            .await?;
        session.bot_message_ids = extract_message_ids(&results);
        self.state_store
            .set_workflow_agent_session(serde_json::to_value(&session)?)
            .await?;
        self.state_store.save().await?;
        Ok(())
    }
}
