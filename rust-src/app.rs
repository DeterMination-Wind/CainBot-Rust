use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::chat_session_manager::ChatSessionManager;
use crate::config::{Config, load_config};
use crate::codex_bridge_server::CodexBridgeServer;
use crate::event_utils::{
    build_help_text, create_context_from_event, ensure_message_event, event_mentions_other_user,
    event_mentions_self, get_sender_name, is_question_intent_text, parse_command_from_event,
    plain_text_from_event,
};
use crate::group_file_download_worker::GroupFileDownloadWorker;
use crate::issue_repair_manager::IssueRepairManager;
use crate::logger::Logger;
use crate::message_input::{BuildChatInputOptions, build_chat_input, build_translation_input};
use crate::napcat_client::{NapCatClient, NapCatClientConfig};
use crate::openai_chat_client::{OpenAiChatClient, OpenAiChatClientConfig};
use crate::openai_translator::{OpenAiTranslator, OpenAiTranslatorConfig};
use crate::runtime_config_store::{RuntimeConfigDefaults, RuntimeConfigStore};
use crate::state_store::StateStore;
use crate::utils::path_exists;
use crate::webui_sync_store::WebUiSyncStore;
use crate::worker_process::WorkerSupervisor;

const GROUP_CARD_SYNC_RETRY_MS: u64 = 10 * 60 * 1000;
const GROUP_INVITE_POLL_INTERVAL_MS: u64 = 60 * 1000;
const SHUTDOWN_VOTE_REQUIRED_COUNT: usize = 3;
const SHUTDOWN_VOTE_TTL_MS: u64 = 10 * 60 * 1000;
const SHUTDOWN_VOTE_PROMPT: &str = "确定要关闭此bot的功能吗，大于两个人回复本消息\"Y\"将确认此操作";

pub struct AppRuntime {
    pub config: Config,
    pub logger: Logger,
    pub napcat_client: NapCatClient,
    pub runtime_config_store: RuntimeConfigStore,
    pub enabled_static_groups: Vec<String>,
    pub owner_user_id: String,
    pub bot_display_name: String,
    pub state_store: StateStore,
    pub _webui_sync_store: WebUiSyncStore,
    pub qa_client: Option<OpenAiChatClient>,
    pub translator: Option<OpenAiTranslator>,
    pub chat_session_manager: Option<ChatSessionManager>,
    pub group_file_download_worker: GroupFileDownloadWorker,
    pub _worker_supervisor: WorkerSupervisor,
}

impl AppRuntime {
    pub async fn bootstrap(project_root: PathBuf, config_path: PathBuf) -> Result<Self> {
        let exe_path = std::env::current_exe()?;
        ensure_config_exists(&config_path).await?;

        let loaded = load_config(&config_path).await?;
        let config = loaded.config;
        let logger = Logger::new(&config.bot.log_level, config.bot.log_dir.clone()).await?;

        let state_store = StateStore::new(config.bot.state_file.clone(), logger.clone());
        state_store.load().await?;

        let runtime_config_store = RuntimeConfigStore::new(
            config.bot.runtime_config_file.clone(),
            loaded.config_dir.clone(),
            RuntimeConfigDefaults {
                qa_external_exclusive_groups_file: config.qa.external_exclusive_groups_file.clone(),
                qa_external_exclusive_groups_refresh_ms: config.qa.external_exclusive_groups_refresh_ms,
                qa_external_exclusive_groups_stale_ms: config.qa.external_exclusive_groups_stale_ms,
            },
            logger.clone(),
        );
        runtime_config_store.load().await?;

        let webui_sync_store = WebUiSyncStore::new(project_root.join("data").join("webui-sync.json"));
        webui_sync_store.load().await?;

        let napcat_client = NapCatClient::new(
            NapCatClientConfig {
                base_url: config.napcat.base_url.clone(),
                event_base_url: config.napcat.event_base_url.clone(),
                event_path: config.napcat.event_path.clone(),
                request_timeout_ms: config.napcat.request_timeout_ms,
                headers: config.napcat.headers.clone(),
                max_concurrent_events: config.napcat.max_concurrent_events,
                forward_threshold_chars: config.napcat.forward_threshold_chars,
                upload_retry_attempts: config.napcat.upload_retry_attempts,
                upload_retry_delay_ms: config.napcat.upload_retry_delay_ms,
                upload_stable_wait_ms: config.napcat.upload_stable_wait_ms,
            },
            logger.clone(),
        )?;

        if !config.bot.owner_user_id.trim().is_empty() {
            let owner_id = config.bot.owner_user_id.clone();
            let notify_client = napcat_client.clone();
            logger
                .set_non_info_notifier(move |payload| {
                    let owner_id = owner_id.clone();
                    let notify_client = notify_client.clone();
                    async move {
                        let _ = notify_client.send_private_message(&owner_id, payload.text).await;
                    }
                })
                .await;
        }

        // 可选服务按配置启用，避免“入口一启动就把所有重量级模块常驻进内存”。
        let qa_client = if config.qa.client.enabled {
            Some(OpenAiChatClient::new(
                OpenAiChatClientConfig {
                    enabled: config.qa.client.enabled,
                    base_url: config.qa.client.base_url.clone(),
                    api_key: config.qa.client.api_key.clone(),
                    model: config.qa.client.model.clone(),
                    temperature: config.qa.client.temperature,
                    request_timeout_ms: config.qa.client.request_timeout_ms,
                    retry_attempts: config.qa.client.retry_attempts,
                    retry_delay_ms: config.qa.client.retry_delay_ms,
                    failure_cooldown_ms: config.qa.client.failure_cooldown_ms,
                    failure_cooldown_threshold: config.qa.client.failure_cooldown_threshold,
                },
                logger.clone(),
            )?)
        } else {
            None
        };

        let translator = if config.translation.enabled {
            qa_client.clone().map(|chat_client| {
                OpenAiTranslator::new(
                    OpenAiTranslatorConfig {
                        enabled: config.translation.enabled,
                        model: config.translation.model.clone(),
                        target_language: config.translation.target_language.clone(),
                        temperature: config.translation.temperature,
                        system_prompt: config.translation.system_prompt.clone(),
                    },
                    chat_client,
                )
            })
        } else {
            None
        };
        let worker_supervisor = WorkerSupervisor::new(exe_path, logger.clone());
        let group_file_download_worker =
            GroupFileDownloadWorker::start(&project_root, &config_path, logger.clone()).await?;

        logger
            .info(format!(
                "Cain Rust 运行时已启动，当前已接管基础层：config/logger/state/runtime/openai/napcat。配置文件：{}",
                loaded.config_path.display()
            ))
            .await;
        let chat_session_manager = if qa_client.is_some() {
            Some(
                ChatSessionManager::start(
                    &project_root,
                    &config_path,
                    config.qa.clone(),
                    qa_client.clone().expect("qa_client must exist when starting chat_session_manager"),
                    state_store.clone(),
                    logger.clone(),
                    runtime_config_store.clone(),
                )
                .await?,
            )
        } else {
            None
        };

        logger
            .warn("业务层仍在迁移：当前已接入 codex bridge、issueRepair 和文件下载兼容 worker；msav 与纯 Rust 业务重写尚未完成。")
            .await;

        if !config.bot.owner_user_id.trim().is_empty() {
            let _ = napcat_client
                .send_private_message(
                    &config.bot.owner_user_id,
                    format!(
                        "Cain Rust 运行时已启动。\n当前已接管主事件循环，并接入 codex bridge、issueRepair、文件下载兼容 worker；其余业务层迁移仍在进行中。\n配置：{}",
                        loaded.config_path.display()
                    ),
                )
                .await;
        }

        Ok(Self {
            config: config.clone(),
            logger,
            napcat_client,
            runtime_config_store,
            enabled_static_groups: config.qa.enabled_group_ids,
            owner_user_id: config.bot.owner_user_id,
            bot_display_name: config.bot.display_name,
            state_store,
            _webui_sync_store: webui_sync_store,
            qa_client,
            translator,
            chat_session_manager,
            group_file_download_worker,
            _worker_supervisor: worker_supervisor,
        })
    }

    pub async fn run(self) -> Result<()> {
        let group_file_download_worker = self.group_file_download_worker.clone();
        let mut codex_bridge_server =
            CodexBridgeServer::new(self.config.codex_bridge.clone(), self.napcat_client.clone(), self.logger.clone());
        let codex_bridge_info = codex_bridge_server.start().await?;
        let issue_repair_manager = self.qa_client.clone().map(|chat_client| {
            IssueRepairManager::new(
                self.config.issue_repair.clone(),
                chat_client,
                self.napcat_client.clone(),
                self.state_store.clone(),
                self.logger.clone(),
                codex_bridge_info.clone(),
            )
        });
        if let Some(manager) = issue_repair_manager.as_ref() {
            manager.initialize().await?;
        }
        let shutdown_votes_by_group = Arc::new(tokio::sync::Mutex::new(HashMap::<String, ShutdownVote>::new()));
        let shutdown_vote_message_to_group = Arc::new(tokio::sync::Mutex::new(HashMap::<String, String>::new()));
        let group_nickname_sync_state = Arc::new(tokio::sync::Mutex::new(HashMap::<String, GroupNicknameSyncState>::new()));
        let idle_activity_tokens = Arc::new(tokio::sync::Mutex::new(HashMap::<String, u64>::new()));
        let background_stop = Arc::new(AtomicBool::new(false));

        let invite_poll_logger = self.logger.clone();
        let invite_poll_runtime_store = self.runtime_config_store.clone();
        let invite_poll_napcat_client = self.napcat_client.clone();
        let invite_poll_groups = self.enabled_static_groups.clone();
        let invite_poll_stop = background_stop.clone();
        tokio::spawn(async move {
            while !invite_poll_stop.load(Ordering::SeqCst) {
                if let Err(error) = poll_pending_group_invites(
                    &invite_poll_logger,
                    &invite_poll_runtime_store,
                    &invite_poll_napcat_client,
                    &invite_poll_groups,
                )
                .await
                {
                    invite_poll_logger
                        .warn(format!("轮询待处理群邀请失败：{error:#}"))
                        .await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(GROUP_INVITE_POLL_INTERVAL_MS)).await;
            }
        });
        let event_logger = self.logger.clone();
        let event_runtime_store = self.runtime_config_store.clone();
        let event_napcat_client = self.napcat_client.clone();
        let enabled_static_groups = self.enabled_static_groups.clone();
        let event_qa_client = self.qa_client.clone();
        let event_translator = self.translator.clone();
        let event_state_store = self.state_store.clone();
        let event_config = self.config.clone();
        let event_chat_session_manager = self.chat_session_manager.clone();
        let event_group_file_download_worker = group_file_download_worker.clone();
        let event_issue_repair_manager = issue_repair_manager.clone();
        let event_shutdown_votes_by_group = shutdown_votes_by_group.clone();
        let event_shutdown_vote_message_to_group = shutdown_vote_message_to_group.clone();
        let event_group_nickname_sync_state = group_nickname_sync_state.clone();
        let event_idle_activity_tokens = idle_activity_tokens.clone();
        let owner_user_id = self.owner_user_id.clone();
        let bot_display_name = self.bot_display_name.clone();
        self.napcat_client
            .start_event_loop(move |event: Value| {
                let event_logger = event_logger.clone();
                let event_runtime_store = event_runtime_store.clone();
                let napcat_client = event_napcat_client.clone();
                let state_store = event_state_store.clone();
                let config = event_config.clone();
                let bot_display_name = bot_display_name.clone();
                let enabled_static_groups = enabled_static_groups.clone();
                let qa_client = event_qa_client.clone();
                let translator = event_translator.clone();
                let chat_session_manager = event_chat_session_manager.clone();
                let group_file_download_worker = event_group_file_download_worker.clone();
                let issue_repair_manager = event_issue_repair_manager.clone();
                let shutdown_votes_by_group = event_shutdown_votes_by_group.clone();
                let shutdown_vote_message_to_group = event_shutdown_vote_message_to_group.clone();
                let group_nickname_sync_state = event_group_nickname_sync_state.clone();
                let idle_activity_tokens = event_idle_activity_tokens.clone();
                let owner_user_id = owner_user_id.clone();
                async move {
                    log_event_summary(&event_logger, &event).await;
                    let post_type = event.get("post_type").and_then(Value::as_str).unwrap_or_default();
                    if post_type == "request" {
                        handle_group_invite_stub(
                            &event_logger,
                            &event_runtime_store,
                            &napcat_client,
                            &enabled_static_groups,
                            &event,
                        )
                        .await?;
                        return Ok(());
                    }
                    if post_type == "notice" {
                        handle_notice_event(
                            &shutdown_votes_by_group,
                            &shutdown_vote_message_to_group,
                            &event,
                        )
                        .await;
                        return Ok(());
                    }
                    handle_message_event(
                        &config,
                        &event_logger,
                        &napcat_client,
                        &event_runtime_store,
                        &state_store,
                        qa_client.as_ref(),
                        translator.as_ref(),
                        chat_session_manager.as_ref(),
                        &group_file_download_worker,
                        issue_repair_manager.as_ref(),
                        &shutdown_votes_by_group,
                        &shutdown_vote_message_to_group,
                        &group_nickname_sync_state,
                        &idle_activity_tokens,
                        &event,
                        &enabled_static_groups,
                        &owner_user_id,
                        &bot_display_name,
                    )
                    .await?;
                    Ok(())
                }
            })
            .await?;
        background_stop.store(true, Ordering::SeqCst);
        group_file_download_worker.stop().await?;
        if let Some(chat_session_manager) = self.chat_session_manager.as_ref() {
            chat_session_manager.stop().await?;
        }
        codex_bridge_server.stop().await?;
        self.logger.flush().await?;
        Ok(())
    }
}

pub fn resolve_config_path(project_root: &Path) -> PathBuf {
    match std::env::var("CAINBOT_CONFIG") {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => project_root.join("config.json"),
    }
}

async fn ensure_config_exists(config_path: &Path) -> Result<()> {
    if path_exists(config_path).await {
        return Ok(());
    }
    let example_path = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("config.example.json");
    bail!(
        "未找到配置文件：{}\n请先复制 {} 为 config.json 后再填写。",
        config_path.display(),
        example_path.display()
    );
}

async fn log_event_summary(logger: &Logger, event: &Value) {
    let post_type = event.get("post_type").and_then(Value::as_str).unwrap_or("-");
    let detail_type = event
        .get("message_type")
        .or_else(|| event.get("request_type"))
        .or_else(|| event.get("notice_type"))
        .and_then(Value::as_str)
        .unwrap_or("-");
    logger
        .debug(format!("收到事件：post_type={post_type}, detail={detail_type}"))
        .await;
}

async fn handle_group_invite_stub(
    logger: &Logger,
    runtime_config_store: &RuntimeConfigStore,
    napcat_client: &NapCatClient,
    static_group_ids: &[String],
    event: &Value,
) -> Result<()> {
    let post_type = event.get("post_type").and_then(Value::as_str).unwrap_or_default();
    let request_type = event
        .get("request_type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if post_type != "request" || request_type != "group" {
        return Ok(());
    }
    let group_id = event
        .get("group_id")
        .and_then(Value::as_i64)
        .map(|item| item.to_string())
        .unwrap_or_default();
    if group_id.is_empty() {
        logger.warn(format!("收到群邀请请求但缺少群号：{event}")).await;
        return Ok(());
    }
    let enabled = runtime_config_store
        .is_qa_group_enabled(&group_id, static_group_ids)
        .await;
    let flag = event
        .get("flag")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let sub_type = event
        .get("sub_type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    logger
        .info(format!(
            "收到群邀请请求：groupId={group_id}，当前群问答{}",
            if enabled { "已启用" } else { "未启用" }
        ))
        .await;
    if flag.is_empty() {
        logger
            .warn(format!("群邀请请求缺少 flag，无法自动通过：groupId={group_id}"))
            .await;
        return Ok(());
    }
    match napcat_client
        .set_group_add_request(&flag, true, "", 100, &sub_type)
        .await
    {
        Ok(_) => {
            logger
                .info(format!(
                    "已自动通过群邀请：groupId={group_id}, flag={flag}, subType={sub_type}"
                ))
                .await;
        }
        Err(error) => {
            logger
                .warn(format!(
                    "自动通过群邀请失败：groupId={group_id}, flag={flag}, error={error:#}"
                ))
                .await;
        }
    }
    Ok(())
}

async fn handle_message_event(
    config: &Config,
    logger: &Logger,
    napcat_client: &NapCatClient,
    runtime_config_store: &RuntimeConfigStore,
    _state_store: &StateStore,
    qa_client: Option<&OpenAiChatClient>,
    translator: Option<&OpenAiTranslator>,
    chat_session_manager: Option<&ChatSessionManager>,
    group_file_download_worker: &GroupFileDownloadWorker,
    issue_repair_manager: Option<&IssueRepairManager>,
    shutdown_votes_by_group: &Arc<tokio::sync::Mutex<HashMap<String, ShutdownVote>>>,
    shutdown_vote_message_to_group: &Arc<tokio::sync::Mutex<HashMap<String, String>>>,
    group_nickname_sync_state: &Arc<tokio::sync::Mutex<HashMap<String, GroupNicknameSyncState>>>,
    idle_activity_tokens: &Arc<tokio::sync::Mutex<HashMap<String, u64>>>,
    event: &Value,
    static_group_ids: &[String],
    owner_user_id: &str,
    bot_display_name: &str,
) -> Result<()> {
    if !ensure_message_event(event)? {
        return Ok(());
    }
    let context = create_context_from_event(event);
    if context.user_id == context.self_id && !context.self_id.is_empty() {
        return Ok(());
    }
    let text = plain_text_from_event(event);
    let command = parse_command_from_event(event);
    let reply_message_id = event
        .get("message_id")
        .map(value_to_string)
        .filter(|item| !item.is_empty());

    // 先把显式命令单独切出来，保证后续迁移会话逻辑时入口稳定。
    if let Some(command) = command {
        let target_id = if context.message_type == "group" {
            context.group_id.as_str()
        } else {
            context.user_id.as_str()
        };
        if let Err(error) = execute_command(
            logger,
            napcat_client,
            runtime_config_store,
            qa_client,
            translator,
            chat_session_manager,
            group_file_download_worker,
            &context,
            event,
            static_group_ids,
            owner_user_id,
            bot_display_name,
            &command,
            target_id,
            reply_message_id.as_deref(),
        )
        .await
        {
            logger.warn(format!("命令处理失败：{error:#}")).await;
            napcat_client
                .reply_text(
                    &context.message_type,
                    target_id,
                    reply_message_id.as_deref(),
                    &format!("命令执行失败：{error}"),
                )
                .await?;
        }
        return Ok(());
    }

    if context.message_type == "group" {
        ensure_group_nickname(
            napcat_client,
            logger,
            config,
            group_nickname_sync_state,
            &context.group_id,
            &context.self_id,
        )
        .await;
        if let Some(chat_session_manager) = chat_session_manager {
            touch_group_activity(idle_activity_tokens, chat_session_manager, context.group_id.clone(), logger.clone());
            if maybe_handle_shutdown_vote_reply(
                logger,
                napcat_client,
                chat_session_manager,
                shutdown_votes_by_group,
                shutdown_vote_message_to_group,
                &context,
                event,
                &text,
            )
            .await?
            {
                return Ok(());
            }
            if maybe_start_shutdown_vote(
                config,
                logger,
                napcat_client,
                qa_client,
                chat_session_manager,
                shutdown_votes_by_group,
                shutdown_vote_message_to_group,
                &context,
                event,
                &text,
            )
            .await?
            {
                return Ok(());
            }
        }
        if group_file_download_worker
            .handle_group_message(&context, event, &text)
            .await?
        {
            return Ok(());
        }
        if let Some(issue_repair_manager) = issue_repair_manager
            && issue_repair_manager
                .handle_incoming_message(&context, event, &text)
                .await?
        {
            return Ok(());
        }
        if event_mentions_other_user(event, bot_display_name) {
            return Ok(());
        }
        if let Some(chat_session_manager) = chat_session_manager {
            if chat_session_manager.is_group_enabled(&context.group_id).await {
                if event_mentions_self(event, bot_display_name) {
                    if let Some(qa_client) = qa_client {
                        let input = build_chat_input(
                            napcat_client,
                            event,
                            BuildChatInputOptions {
                                argument: text.clone(),
                                allow_current_text_fallback: true,
                                ai_runtime_prefix: format!(
                                    "当前 AI 身份：{}\n当前日期时间：{}",
                                    config.bot.display_name,
                                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
                                ),
                            },
                        )
                        .await?;
                        let result = chat_session_manager.chat(&context, &input).await?;
                        send_chat_result_if_present(
                            chat_session_manager,
                            group_file_download_worker,
                            napcat_client,
                            &context,
                            reply_message_id.as_deref(),
                            &result,
                            &text,
                            "fallback",
                        )
                        .await?;
                        let _ = qa_client;
                        return Ok(());
                    }
                }
                chat_session_manager.record_incoming_message(&context, event, &text).await?;
                if let Err(error) = chat_session_manager
                    .maybe_capture_correction_memory(&context, event)
                    .await
                {
                    logger
                        .warn(format!("后台长期记忆捕获失败：{error:#}"))
                        .await;
                }
                if chat_session_manager.is_group_proactive_reply_enabled(&context.group_id).await
                    && is_question_intent_text(&text)
                {
                    let (allowed, _, _) = chat_session_manager
                        .should_run_group_proactive_filter(&context.group_id)
                        .await;
                    if allowed {
                        let (should_prompt, reason) = chat_session_manager
                            .should_suggest_reply(&context, event, &text)
                            .await?;
                        if should_prompt {
                            logger.info(format!("群消息通过主动过滤：{reason}")).await;
                            let input = build_chat_input(
                                napcat_client,
                                event,
                                BuildChatInputOptions {
                                    argument: text.clone(),
                                    allow_current_text_fallback: true,
                                    ai_runtime_prefix: format!(
                                        "当前 AI 身份：{}\n当前日期时间：{}",
                                        config.bot.display_name,
                                        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
                                    ),
                                },
                            )
                            .await?;
                            let result = chat_session_manager.chat(&context, &input).await?;
                            send_chat_result_if_present(
                                chat_session_manager,
                                group_file_download_worker,
                                napcat_client,
                                &context,
                                reply_message_id.as_deref(),
                                &result,
                                &text,
                                "suppress",
                            )
                            .await?;
                            chat_session_manager
                                .mark_hinted(
                                    &context,
                                    &event.get("message_id").map(value_to_string).unwrap_or_default(),
                                )
                                .await?;
                            return Ok(());
                        }
                    }
                }
            }
        }
        if event_mentions_self(event, bot_display_name) || is_question_intent_text(&text) {
            logger
                .info(format!(
                    "捕获到后续可接管问答的候选消息：groupId={}, sender={}, text={}",
                    context.group_id,
                    get_sender_name(event),
                    text.chars().take(120).collect::<String>()
                ))
                .await;
        }
    }
    Ok(())
}

async fn execute_command(
    logger: &Logger,
    napcat_client: &NapCatClient,
    runtime_config_store: &RuntimeConfigStore,
    _qa_client: Option<&OpenAiChatClient>,
    translator: Option<&OpenAiTranslator>,
    chat_session_manager: Option<&ChatSessionManager>,
    group_file_download_worker: &GroupFileDownloadWorker,
    context: &crate::event_utils::EventContext,
    event: &Value,
    static_group_ids: &[String],
    owner_user_id: &str,
    bot_display_name: &str,
    command: &crate::commands::ParsedCommand,
    target_id: &str,
    reply_message_id: Option<&str>,
) -> Result<()> {
    match command.name.as_str() {
        "help" => {
            napcat_client
                .reply_text(
                    &context.message_type,
                    target_id,
                    reply_message_id,
                    &build_help_text(bot_display_name),
                )
                .await?;
        }
        "chat" => {
            if context.message_type == "group" && !runtime_config_store.is_qa_group_enabled(&context.group_id, static_group_ids).await {
                bail!("当前群未启用 Cain 问答。请先由 bot 主人执行 /e 启用，或把群号加入 qa.enabledGroupIds。");
            }
            let Some(chat_session_manager) = chat_session_manager else {
                bail!("当前未启用聊天会话管理器。");
            };
            let chat_input = build_chat_input(
                napcat_client,
                event,
                BuildChatInputOptions {
                    argument: command.argument.clone(),
                    allow_current_text_fallback: false,
                    ai_runtime_prefix: format!(
                        "当前 AI 身份：{bot_display_name}\n当前日期时间：{}",
                        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
                    ),
                },
            )
            .await?;
            if !chat_input.has_content() {
                bail!("没有可问答的内容；请直接写在命令后，或引用一条消息，或附带图片/文本文件。");
            }
            logger
                .info(format!(
                    "执行显式问答：source={}, sender={}, preview={}",
                    if context.message_type == "group" {
                        format!("group:{}", context.group_id)
                    } else {
                        format!("private:{}", context.user_id)
                    },
                    get_sender_name(event),
                    chat_input
                        .runtime_context
                        .timeline_text
                        .chars()
                        .take(120)
                        .collect::<String>()
                ))
                .await;
            let source_text = chat_input.runtime_context.timeline_text.clone();
            let result = chat_session_manager.chat(context, &chat_input).await?;
            send_chat_result_if_present(
                chat_session_manager,
                group_file_download_worker,
                napcat_client,
                context,
                reply_message_id,
                &result,
                &source_text,
                "fallback",
            )
            .await?;
        }
        "translate" => {
            let Some(translator) = translator else {
                bail!("当前未启用翻译客户端。");
            };
            let source = build_translation_input(napcat_client, event, &command.argument).await?;
            if !source.has_content() {
                bail!("没有可翻译的内容；请直接写在命令后，或引用一条消息，或附带图片/文本文件。");
            }
            let translated = translator.translate(source.into_translation_input()).await?;
            napcat_client
                .reply_text(&context.message_type, target_id, reply_message_id, &translated)
                .await?;
        }
        "edit" => {
            let subcommand = command.positionals.first().map(String::as_str).unwrap_or_default();
            if subcommand == "状态" {
                let group_id = require_group_id(command, context)?;
                let status = if let Some(chat_session_manager) = chat_session_manager {
                    format_group_status(&chat_session_manager.get_group_prompt_status(&group_id).await)
                } else {
                    build_group_status_text(runtime_config_store, &group_id, static_group_ids).await
                };
                napcat_client
                    .reply_text(&context.message_type, target_id, reply_message_id, &status)
                    .await?;
            } else if subcommand == "启用" || subcommand == "禁用" {
                if context.user_id != owner_user_id {
                    bail!("/e 启用 和 /e 禁用 仅 bot 主人可用。");
                }
                let group_id = require_group_id(command, context)?;
                let result = runtime_config_store
                    .set_qa_group_enabled(&group_id, subcommand == "启用", Some(subcommand == "启用"))
                    .await?;
                let status = if let Some(chat_session_manager) = chat_session_manager {
                    format_group_status(&chat_session_manager.get_group_prompt_status(&group_id).await)
                } else {
                    build_group_status_text(runtime_config_store, &group_id, static_group_ids).await
                };
                napcat_client
                    .reply_text(
                        &context.message_type,
                        target_id,
                        reply_message_id,
                        &format!(
                            "{}。\n\n{}",
                            if result.action == "created" {
                                "已创建群开关记录"
                            } else {
                                "已更新群开关记录"
                            },
                            status
                        ),
                    )
                    .await?;
            } else if subcommand == "文件下载" {
                let group_id = require_group_id(command, context)?;
                ensure_group_manager_permission(napcat_client, event, context, owner_user_id).await?;
                let action = command.positionals.get(1).map(String::as_str).unwrap_or_default();
                if action != "启用" && action != "关闭" {
                    bail!("用法：/e 文件下载 启用 [群文件夹名]|关闭");
                }
                let folder_name = if action == "启用" {
                    command.positionals.iter().skip(2).cloned().collect::<Vec<_>>().join(" ")
                } else {
                    String::new()
                };
                let result = runtime_config_store
                    .set_qa_group_file_download_enabled(&group_id, action == "启用", static_group_ids, &folder_name)
                    .await?;
                let status = if let Some(chat_session_manager) = chat_session_manager {
                    format_group_status(&chat_session_manager.get_group_prompt_status(&group_id).await)
                } else {
                    build_group_status_text(runtime_config_store, &group_id, static_group_ids).await
                };
                napcat_client
                    .reply_text(
                        &context.message_type,
                        target_id,
                        reply_message_id,
                        &format!(
                            "{}。\n\n{}",
                            if result.action == "created" {
                                "已创建文件下载开关记录"
                            } else {
                                "已更新文件下载开关记录"
                            },
                            status
                        ),
                    )
                    .await?;
            } else if subcommand == "过滤心跳" {
                let group_id = require_group_id(command, context)?;
                ensure_group_manager_permission(napcat_client, event, context, owner_user_id).await?;
                let action = command.positionals.get(1).map(String::as_str).unwrap_or_default();
                if action != "启用" && action != "关闭" {
                    bail!("用法：/e 过滤心跳 启用 [N]|关闭");
                }
                let interval = if action == "启用" {
                    command
                        .positionals
                        .get(2)
                        .and_then(|item| item.trim().parse::<u64>().ok())
                        .unwrap_or(10)
                } else {
                    10
                };
                if action == "启用" && !(1..=1000).contains(&interval) {
                    bail!("过滤心跳间隔必须是 1 到 1000 之间的整数。");
                }
                let result = runtime_config_store
                    .set_qa_group_filter_heartbeat(&group_id, action == "启用", interval, static_group_ids)
                    .await?;
                let status = if let Some(chat_session_manager) = chat_session_manager {
                    format_group_status(&chat_session_manager.get_group_prompt_status(&group_id).await)
                } else {
                    build_group_status_text(runtime_config_store, &group_id, static_group_ids).await
                };
                napcat_client
                    .reply_text(
                        &context.message_type,
                        target_id,
                        reply_message_id,
                        &format!(
                            "{}。\n\n{}",
                            if result.action == "created" {
                                "已创建过滤心跳记录"
                            } else {
                                "已更新过滤心跳记录"
                            },
                            status
                        ),
                    )
                    .await?;
            } else if subcommand == "过滤" || subcommand == "聊天" {
                let group_id = require_group_id(command, context)?;
                ensure_group_manager_permission(napcat_client, event, context, owner_user_id).await?;
                let instruction = get_edit_instruction(command, subcommand);
                if instruction.is_empty() {
                    bail!("/e {} 后必须跟修改要求。", subcommand);
                }
                let Some(chat_session_manager) = chat_session_manager else {
                    bail!("当前未启用聊天会话管理器。");
                };
                let (prompt, reason) = if subcommand == "过滤" {
                    chat_session_manager.update_filter_prompt(&group_id, &instruction).await?
                } else {
                    chat_session_manager.update_answer_prompt(&group_id, &instruction).await?
                };
                let status = format_group_status(&chat_session_manager.get_group_prompt_status(&group_id).await);
                napcat_client
                    .reply_text(
                        &context.message_type,
                        target_id,
                        reply_message_id,
                        &[
                            "修改完成。".to_string(),
                            if !reason.trim().is_empty() {
                                format!("审核说明：{reason}")
                            } else {
                                String::new()
                            },
                            String::new(),
                            if subcommand == "过滤" {
                                "当前过滤 prompt：".to_string()
                            } else {
                                "当前聊天 prompt：".to_string()
                            },
                            prompt,
                            String::new(),
                            "当前完整状态：".to_string(),
                            status,
                        ]
                        .into_iter()
                        .filter(|item| !item.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    )
                    .await?;
            } else {
                napcat_client
                    .reply_text(
                        &context.message_type,
                        target_id,
                        reply_message_id,
                        "Rust 版 /e 目前已接入只读“状态”查询；其余编辑能力仍在迁移中。",
                    )
                    .await?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn get_edit_instruction(command: &crate::commands::ParsedCommand, subcommand: &str) -> String {
    let raw_args = command.raw_args.trim();
    if raw_args.is_empty() {
        return String::new();
    }
    if raw_args.starts_with(subcommand) {
        return raw_args[subcommand.len()..].trim().to_string();
    }
    command.positionals.iter().skip(1).cloned().collect::<Vec<_>>().join(" ")
}

fn require_group_id(command: &crate::commands::ParsedCommand, context: &crate::event_utils::EventContext) -> Result<String> {
    let group_id = command
        .flags
        .get("group")
        .map(String::as_str)
        .unwrap_or(context.group_id.as_str())
        .trim()
        .to_string();
    if group_id.is_empty() {
        bail!("该命令需要群号；如果你在私聊里使用，请加 --group <群号>");
    }
    Ok(group_id)
}

async fn ensure_group_manager_permission(
    napcat_client: &NapCatClient,
    event: &Value,
    context: &crate::event_utils::EventContext,
    owner_user_id: &str,
) -> Result<()> {
    let role = get_user_group_role(napcat_client, event, context, owner_user_id).await;
    if role == "owner-bot" || role == "owner" || role == "admin" {
        return Ok(());
    }
    bail!("只有该群群主、管理员或 bot 主人可以修改该项配置。");
}

async fn get_user_group_role(
    napcat_client: &NapCatClient,
    event: &Value,
    context: &crate::event_utils::EventContext,
    owner_user_id: &str,
) -> String {
    if context.user_id == owner_user_id {
        return "owner-bot".to_string();
    }
    let sender_role = event
        .get("sender")
        .and_then(|sender| sender.get("role"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if sender_role == "owner" || sender_role == "admin" {
        return sender_role;
    }
    if context.message_type != "group" || context.group_id.is_empty() || context.user_id.is_empty() {
        return "member".to_string();
    }
    match napcat_client
        .get_group_member_info(&context.group_id, &context.user_id, true)
        .await
    {
        Ok(info) => info
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("member")
            .trim()
            .to_ascii_lowercase(),
        Err(_) => "member".to_string(),
    }
}

async fn build_group_status_text(
    runtime_config_store: &RuntimeConfigStore,
    group_id: &str,
    static_group_ids: &[String],
) -> String {
    let enabled = runtime_config_store
        .is_qa_group_enabled(group_id, static_group_ids)
        .await;
    let group_entry = runtime_config_store
        .get_qa_groups()
        .await
        .into_iter()
        .find(|item| item.group_id == group_id);
    let group_override = runtime_config_store.get_group_qa_override(group_id).await;
    [
        format!("当前群启用状态：{}", if enabled { "已启用" } else { "未启用" }),
        format!(
            "当前主动回复状态：{}",
            if group_entry.as_ref().map(|item| item.proactive_reply_enabled).unwrap_or(true) {
                "已启用"
            } else {
                "已关闭"
            }
        ),
        format!(
            "当前过滤心跳：{}",
            if group_entry.as_ref().map(|item| item.filter_heartbeat_enabled).unwrap_or(false) {
                format!(
                    "已启用（每 {} 条候选消息审核一次）",
                    group_entry
                        .as_ref()
                        .map(|item| item.filter_heartbeat_interval)
                        .unwrap_or(10)
                )
            } else {
                "已关闭".to_string()
            }
        ),
        format!(
            "当前文件下载状态：{}",
            if group_entry.as_ref().map(|item| item.file_download_enabled).unwrap_or(false) {
                "已启用"
            } else {
                "已关闭"
            }
        ),
        format!(
            "当前文件下载群文件夹：{}",
            group_entry
                .as_ref()
                .map(|item| item.file_download_folder_name.as_str())
                .filter(|item| !item.trim().is_empty())
                .unwrap_or("(根目录)")
        ),
        String::new(),
        "当前过滤 prompt：".to_string(),
        group_override
            .as_ref()
            .map(|item| item.filter_prompt.as_str())
            .filter(|item| !item.trim().is_empty())
            .unwrap_or("(空)")
            .to_string(),
        String::new(),
        "当前聊天 prompt：".to_string(),
        group_override
            .as_ref()
            .map(|item| item.answer_prompt.as_str())
            .filter(|item| !item.trim().is_empty())
            .unwrap_or("(空)")
            .to_string(),
    ]
    .join("\n")
}

fn format_group_status(status: &crate::chat_session_manager::GroupPromptStatus) -> String {
    [
        format!("当前群启用状态：{}", if status.enabled { "已启用" } else { "未启用" }),
        format!(
            "当前主动回复状态：{}",
            if status.proactive_reply_enabled { "已启用" } else { "已关闭" }
        ),
        format!(
            "当前过滤心跳：{}",
            if status.filter_heartbeat_enabled {
                format!("已启用（每 {} 条候选消息审核一次）", status.filter_heartbeat_interval)
            } else {
                "已关闭".to_string()
            }
        ),
        format!(
            "当前文件下载状态：{}",
            if status.file_download_enabled { "已启用" } else { "已关闭" }
        ),
        format!(
            "当前文件下载群文件夹：{}",
            if status.file_download_folder_name.trim().is_empty() {
                "(根目录)"
            } else {
                status.file_download_folder_name.as_str()
            }
        ),
        String::new(),
        "当前过滤 prompt：".to_string(),
        status.filter_prompt.clone(),
        String::new(),
        "当前聊天 prompt：".to_string(),
        status.answer_prompt.clone(),
    ]
    .join("\n")
}

async fn send_chat_result_if_present(
    chat_session_manager: &ChatSessionManager,
    group_file_download_worker: &GroupFileDownloadWorker,
    napcat_client: &NapCatClient,
    context: &crate::event_utils::EventContext,
    reply_message_id: Option<&str>,
    result: &crate::chat_session_manager::ChatResult,
    source_text: &str,
    on_low_information: &str,
) -> Result<()> {
    let target_id = if context.message_type == "group" {
        context.group_id.as_str()
    } else {
        context.user_id.as_str()
    };

    if result.notice == "group-file-download-started" && context.message_type == "group" {
        let request_text = result
            .group_file_download_request
            .as_ref()
            .map(|item| item.request_text.as_str())
            .filter(|item| !item.trim().is_empty())
            .unwrap_or(source_text)
            .trim()
            .to_string();
        let request = result
            .group_file_download_request
            .as_ref()
            .map(|item| item.request.clone())
            .filter(|item| !item.is_null())
            .unwrap_or_else(|| json!({ "request_text": request_text }));
        let handoff = group_file_download_worker
            .start_group_download_flow_from_tool(
                context,
                reply_message_id.unwrap_or_default(),
                &request_text,
                &request,
            )
            .await?;
        if handoff.get("started").and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        if let Some(reason) = handoff.get("reason").and_then(Value::as_str).map(str::trim).filter(|item| !item.is_empty()) {
            napcat_client
                .reply_text(&context.message_type, target_id, reply_message_id, reason)
                .await?;
        }
        return Ok(());
    }

    let review = chat_session_manager
        .review_low_information_reply(source_text, &result.text, on_low_information)
        .await?;
    if review.start_group_file_download && context.message_type == "group" {
        let request_text = if review.request_text.trim().is_empty() {
            source_text.trim().to_string()
        } else {
            review.request_text.trim().to_string()
        };
        let handoff = group_file_download_worker
            .start_group_download_flow_from_tool(
                context,
                reply_message_id.unwrap_or_default(),
                &request_text,
                &json!({ "request_text": request_text }),
            )
            .await?;
        if handoff.get("started").and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        if let Some(reason) = handoff.get("reason").and_then(Value::as_str).map(str::trim).filter(|item| !item.is_empty()) {
            napcat_client
                .reply_text(&context.message_type, target_id, reply_message_id, reason)
                .await?;
        }
        return Ok(());
    }

    if review.text.trim().is_empty() {
        return Ok(());
    }

    napcat_client
        .reply_text(&context.message_type, target_id, reply_message_id, &review.text)
        .await?;
    Ok(())
}

#[derive(Debug, Clone)]
struct GroupNicknameSyncState {
    status: String,
    nickname: String,
    updated_at_ms: u64,
}

#[derive(Debug, Clone)]
struct ShutdownVote {
    group_id: String,
    message_ids: HashSet<String>,
    voters: HashSet<String>,
    expires_at_ms: u64,
}

async fn ensure_group_nickname(
    napcat_client: &NapCatClient,
    logger: &Logger,
    config: &Config,
    state: &Arc<tokio::sync::Mutex<HashMap<String, GroupNicknameSyncState>>>,
    group_id: &str,
    self_id: &str,
) {
    let target_nickname = config.bot.group_nickname.trim();
    if group_id.trim().is_empty() || self_id.trim().is_empty() || target_nickname.is_empty() {
        return;
    }
    let key = format!("{}:{}", group_id.trim(), self_id.trim());
    {
        let guard = state.lock().await;
        if let Some(current) = guard.get(&key) {
            if current.status == "pending" {
                return;
            }
            if current.status == "ok" && current.nickname == target_nickname {
                return;
            }
            if current.status == "failed"
                && current.nickname == target_nickname
                && current_time_ms().saturating_sub(current.updated_at_ms) < GROUP_CARD_SYNC_RETRY_MS
            {
                return;
            }
        }
    }
    state.lock().await.insert(
        key.clone(),
        GroupNicknameSyncState {
            status: "pending".to_string(),
            nickname: target_nickname.to_string(),
            updated_at_ms: current_time_ms(),
        },
    );
    match napcat_client
        .get_group_member_info(group_id, self_id, true)
        .await
    {
        Ok(info)
            if info
                .get("card")
                .and_then(Value::as_str)
                .map(|item| item.trim() == target_nickname)
                .unwrap_or(false) =>
        {
            state.lock().await.insert(
                key,
                GroupNicknameSyncState {
                    status: "ok".to_string(),
                    nickname: target_nickname.to_string(),
                    updated_at_ms: current_time_ms(),
                },
            );
        }
        _ => match napcat_client.set_group_card(group_id, self_id, target_nickname).await {
            Ok(_) => {
                state.lock().await.insert(
                    key,
                    GroupNicknameSyncState {
                        status: "ok".to_string(),
                        nickname: target_nickname.to_string(),
                        updated_at_ms: current_time_ms(),
                    },
                );
                logger
                    .info(format!("已同步群 {group_id} 的 bot 群名片为 {target_nickname}"))
                    .await;
            }
            Err(error) => {
                state.lock().await.insert(
                    key,
                    GroupNicknameSyncState {
                        status: "failed".to_string(),
                        nickname: target_nickname.to_string(),
                        updated_at_ms: current_time_ms(),
                    },
                );
                logger
                    .warn(format!("同步群 {group_id} 的 bot 群名片失败：{error:#}"))
                    .await;
            }
        },
    }
}

fn touch_group_activity(
    idle_tokens: &Arc<tokio::sync::Mutex<HashMap<String, u64>>>,
    chat_session_manager: &ChatSessionManager,
    group_id: String,
    logger: Logger,
) {
    if group_id.trim().is_empty() {
        return;
    }
    let idle_tokens = idle_tokens.clone();
    let chat_session_manager = chat_session_manager.clone();
    tokio::spawn(async move {
        let token = current_time_ms();
        idle_tokens.lock().await.insert(group_id.clone(), token);
        tokio::time::sleep(std::time::Duration::from_secs(15 * 60)).await;
        let current = idle_tokens.lock().await.get(&group_id).copied();
        if current != Some(token) || !chat_session_manager.is_group_enabled(&group_id).await {
            return;
        }
        if let Ok((should_end, reason)) = chat_session_manager.maybe_close_group_topic(&group_id).await {
            logger
                .info(format!(
                    "群 {group_id} 空闲话题判断：{}{}",
                    if should_end { "结束" } else { "继续" },
                    if reason.is_empty() {
                        String::new()
                    } else {
                        format!(" ({reason})")
                    }
                ))
                .await;
        }
    });
}

async fn maybe_handle_shutdown_vote_reply(
    logger: &Logger,
    napcat_client: &NapCatClient,
    chat_session_manager: &ChatSessionManager,
    shutdown_votes_by_group: &Arc<tokio::sync::Mutex<HashMap<String, ShutdownVote>>>,
    shutdown_vote_message_to_group: &Arc<tokio::sync::Mutex<HashMap<String, String>>>,
    context: &crate::event_utils::EventContext,
    event: &Value,
    text: &str,
) -> Result<bool> {
    let Some(reply_id) = extract_reply_id_from_event(event) else {
        return Ok(false);
    };
    let Some(group_id) = shutdown_vote_message_to_group.lock().await.get(&reply_id).cloned() else {
        return Ok(false);
    };
    if !text_contains_shutdown_vote_approval(text) {
        return Ok(true);
    }
    let mut votes = shutdown_votes_by_group.lock().await;
    let Some(vote) = votes.get_mut(&group_id) else {
        return Ok(false);
    };
    if vote.expires_at_ms <= current_time_ms() {
        votes.remove(&group_id);
        drop(votes);
        clear_shutdown_vote_message_map(shutdown_vote_message_to_group, &group_id).await;
        return Ok(false);
    }
    if !context.user_id.trim().is_empty() {
        vote.voters.insert(context.user_id.clone());
    }
    if vote.voters.len() < SHUTDOWN_VOTE_REQUIRED_COUNT {
        return Ok(true);
    }
    votes.remove(&group_id);
    drop(votes);
    clear_shutdown_vote_message_map(shutdown_vote_message_to_group, &group_id).await;
    let _ = logger;
    let _ = event;
    chat_session_manager
        .disable_group_proactive_replies(&context.group_id)
        .await?;
    napcat_client
        .reply_text(
            &context.message_type,
            &context.group_id,
            event.get("message_id").map(value_to_string).as_deref(),
            "已根据投票关闭本群主动回复功能。仍可通过 @我 或 /chat 调用。",
        )
        .await?;
    Ok(true)
}

async fn maybe_start_shutdown_vote(
    config: &Config,
    logger: &Logger,
    napcat_client: &NapCatClient,
    qa_client: Option<&OpenAiChatClient>,
    chat_session_manager: &ChatSessionManager,
    shutdown_votes_by_group: &Arc<tokio::sync::Mutex<HashMap<String, ShutdownVote>>>,
    shutdown_vote_message_to_group: &Arc<tokio::sync::Mutex<HashMap<String, String>>>,
    context: &crate::event_utils::EventContext,
    event: &Value,
    text: &str,
) -> Result<bool> {
    if !chat_session_manager.is_group_enabled(&context.group_id).await
        || !chat_session_manager
            .is_group_proactive_reply_enabled(&context.group_id)
            .await
        || !event_mentions_self(event, &config.bot.display_name)
        || !text_looks_like_explicit_shutdown_request(text)
    {
        return Ok(false);
    }
    if let Some(vote) = shutdown_votes_by_group.lock().await.get(&context.group_id).cloned()
        && vote.expires_at_ms > current_time_ms()
    {
        napcat_client
            .reply_text(
                &context.message_type,
                &context.group_id,
                event.get("message_id").map(value_to_string).as_deref(),
                "当前群已有关闭投票在进行，直接回复那条投票消息 \"Y\" 即可。",
            )
            .await?;
        return Ok(true);
    }
    let should_start = if let Some(qa_client) = qa_client {
        classify_shutdown_vote_intent(qa_client, config, context, event, text).await?
    } else {
        looks_like_bot_opposition_candidate(text, &config.bot.display_name)
    };
    if !should_start {
        return Ok(false);
    }

    let results = napcat_client
        .reply_text(
            &context.message_type,
            &context.group_id,
            event.get("message_id").map(value_to_string).as_deref(),
            SHUTDOWN_VOTE_PROMPT,
        )
        .await?;
    let message_ids = extract_message_ids_from_send_results(&results);
    if message_ids.is_empty() {
        return Ok(true);
    }
    shutdown_votes_by_group.lock().await.insert(
        context.group_id.clone(),
        ShutdownVote {
            group_id: context.group_id.clone(),
            message_ids: message_ids.iter().cloned().collect(),
            voters: HashSet::new(),
            expires_at_ms: current_time_ms() + SHUTDOWN_VOTE_TTL_MS,
        },
    );
    let mut map = shutdown_vote_message_to_group.lock().await;
    for message_id in &message_ids {
        map.insert(message_id.clone(), context.group_id.clone());
    }
    drop(map);
    schedule_shutdown_vote_expiry(
        logger.clone(),
        napcat_client.clone(),
        shutdown_votes_by_group.clone(),
        shutdown_vote_message_to_group.clone(),
        context.group_id.clone(),
    );
    Ok(true)
}

fn schedule_shutdown_vote_expiry(
    logger: Logger,
    napcat_client: NapCatClient,
    shutdown_votes_by_group: Arc<tokio::sync::Mutex<HashMap<String, ShutdownVote>>>,
    shutdown_vote_message_to_group: Arc<tokio::sync::Mutex<HashMap<String, String>>>,
    group_id: String,
) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(SHUTDOWN_VOTE_TTL_MS)).await;
        let expired = {
            let mut votes = shutdown_votes_by_group.lock().await;
            match votes.get(&group_id) {
                Some(vote) if vote.expires_at_ms <= current_time_ms() => votes.remove(&group_id),
                _ => None,
            }
        };
        if expired.is_some() {
            clear_shutdown_vote_message_map(&shutdown_vote_message_to_group, &group_id).await;
            if let Err(error) = napcat_client
                .send_group_message(&group_id, Value::String("关闭投票 10 分钟内未通过，已自动关闭。".to_string()))
                .await
            {
                logger
                    .warn(format!("发送关闭投票超时提示失败：{error:#}"))
                    .await;
            }
        }
    });
}

async fn clear_shutdown_vote_message_map(
    shutdown_vote_message_to_group: &Arc<tokio::sync::Mutex<HashMap<String, String>>>,
    group_id: &str,
) {
    shutdown_vote_message_to_group
        .lock()
        .await
        .retain(|_, value| value != group_id);
}

async fn classify_shutdown_vote_intent(
    qa_client: &OpenAiChatClient,
    config: &Config,
    context: &crate::event_utils::EventContext,
    event: &Value,
    text: &str,
) -> Result<bool> {
    let raw = qa_client
        .complete(
            &[
                crate::openai_chat_client::ChatMessage {
                    role: "system".to_string(),
                    content: Value::String(
                        [
                            "你负责判断一条 QQ 群消息是否是在明确要求关闭当前 bot 的功能。",
                            "只有当消息明确要求关闭/停用/禁用这个 bot 时 should_start_vote 才为 true。",
                            "输出 JSON：{\"should_start_vote\":boolean}。",
                        ]
                        .join("\n"),
                    ),
                },
                crate::openai_chat_client::ChatMessage {
                    role: "user".to_string(),
                    content: Value::String(
                        [
                            format!("群号：{}", context.group_id),
                            format!("发送者：{}", get_sender_name(event)),
                            format!("消息内容：{text}"),
                            format!("bot 名称：{}", config.bot.display_name),
                        ]
                        .join("\n"),
                    ),
                },
            ],
            crate::openai_chat_client::CompleteOptions {
                model: Some(config.qa.shutdown_vote_filter_model.clone()),
                temperature: Some(0.1),
            },
        )
        .await?;
    Ok(parse_json_object_field_bool(&raw, "should_start_vote").unwrap_or(false))
}

async fn poll_pending_group_invites(
    logger: &Logger,
    runtime_config_store: &RuntimeConfigStore,
    napcat_client: &NapCatClient,
    static_group_ids: &[String],
) -> Result<()> {
    let system_messages = napcat_client.get_group_system_messages(100).await?;
    for invite in normalize_invited_requests(&system_messages) {
        let request_id = invite
            .get("request_id")
            .or_else(|| invite.get("flag"))
            .map(value_to_string)
            .unwrap_or_default();
        if request_id.is_empty() {
            continue;
        }
        let group_id = invite
            .get("group_id")
            .map(value_to_string)
            .unwrap_or_default();
        if invite.get("checked").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let _ = runtime_config_store
            .is_qa_group_enabled(&group_id, static_group_ids)
            .await;
        napcat_client
            .set_group_add_request(&request_id, true, "", 100, "invite")
            .await?;
        logger
            .info(format!("已通过系统消息轮询自动同意群邀请：group={group_id} request={request_id}"))
            .await;
    }
    Ok(())
}

async fn handle_notice_event(
    shutdown_votes_by_group: &Arc<tokio::sync::Mutex<HashMap<String, ShutdownVote>>>,
    shutdown_vote_message_to_group: &Arc<tokio::sync::Mutex<HashMap<String, String>>>,
    event: &Value,
) {
    let notice_type = event.get("notice_type").and_then(Value::as_str).unwrap_or_default();
    if notice_type != "group_recall" && notice_type != "friend_recall" {
        return;
    }
    let recalled_message_id = event
        .get("message_id")
        .or_else(|| event.get("msg_id"))
        .map(value_to_string)
        .unwrap_or_default();
    if recalled_message_id.is_empty() {
        return;
    }
    let removed_group = shutdown_vote_message_to_group.lock().await.remove(&recalled_message_id);
    if let Some(group_id) = removed_group {
        let should_remove_group = {
            let mut votes = shutdown_votes_by_group.lock().await;
            if let Some(vote) = votes.get_mut(&group_id) {
                vote.message_ids.remove(&recalled_message_id);
                vote.message_ids.is_empty()
            } else {
                false
            }
        };
        if should_remove_group {
            shutdown_votes_by_group.lock().await.remove(&group_id);
        }
    }
}

fn normalize_invited_requests(payload: &Value) -> Vec<Value> {
    let mut deduped = HashMap::<String, Value>::new();
    for item in payload
        .get("invited_requests")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(payload.get("InvitedRequest").and_then(Value::as_array).into_iter().flatten())
    {
        let request_id = item
            .get("request_id")
            .or_else(|| item.get("flag"))
            .map(value_to_string)
            .unwrap_or_default();
        if !request_id.is_empty() {
            deduped.entry(request_id).or_insert_with(|| item.clone());
        }
    }
    deduped.into_values().collect()
}

fn extract_reply_id_from_event(event: &Value) -> Option<String> {
    let message = event.get("message").unwrap_or(&Value::Null);
    if let Some(items) = message.as_array()
        && let Some(reply) = items.iter().find(|segment| segment.get("type").and_then(Value::as_str) == Some("reply"))
    {
        return reply
            .get("data")
            .and_then(|data| data.get("id"))
            .map(value_to_string)
            .filter(|item| !item.is_empty());
    }
    event.get("raw_message").and_then(Value::as_str).and_then(|raw| {
        let marker = "[CQ:reply,id=";
        let start = raw.find(marker)?;
        let remain = &raw[start + marker.len()..];
        let end = remain.find([',', ']']).unwrap_or(remain.len());
        let reply_id = remain[..end].trim();
        (!reply_id.is_empty()).then(|| reply_id.to_string())
    })
}

fn parse_json_object_field_bool(text: &str, field: &str) -> Option<bool> {
    let parsed = serde_json::from_str::<Value>(text.trim()).ok().or_else(|| {
        let start = text.find('{')?;
        let end = text.rfind('}')?;
        serde_json::from_str::<Value>(&text[start..=end]).ok()
    })?;
    parsed.get(field).and_then(Value::as_bool)
}

fn looks_like_bot_opposition_candidate(text: &str, display_name: &str) -> bool {
    let normalized = text.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return false;
    }
    normalized.contains("机器人")
        || normalized.contains("bot")
        || normalized.contains("自动回复")
        || normalized.contains("ai")
        || normalized.contains("闭嘴")
        || normalized.contains("别回复")
        || normalized.contains("太吵")
        || normalized.contains("关掉")
        || normalized.contains("关闭")
        || (!display_name.trim().is_empty() && normalized.contains(&display_name.trim().to_ascii_lowercase()))
}

fn text_looks_like_explicit_shutdown_request(text: &str) -> bool {
    let normalized = text.trim().to_ascii_lowercase();
    [
        "关闭这个机器人",
        "关闭此机器人",
        "关闭这个bot",
        "关掉这个机器人",
        "关掉这个bot",
        "停用这个机器人",
        "禁用这个机器人",
        "停止这个机器人",
        "关闭bot",
        "关掉bot",
    ]
    .iter()
    .any(|item| normalized.contains(item))
}

fn text_contains_shutdown_vote_approval(text: &str) -> bool {
    let normalized = text.trim();
    normalized == "Y" || normalized == "y" || normalized.split_whitespace().any(|item| item.eq_ignore_ascii_case("y"))
}

fn extract_message_ids_from_send_results(results: &[Value]) -> Vec<String> {
    let mut ids = Vec::new();
    for value in results {
        if let Some(message_id) = value
            .get("message_id")
            .or_else(|| value.get("messageId"))
            .map(value_to_string)
            .filter(|item| !item.is_empty())
        {
            ids.push(message_id);
        }
    }
    ids
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|item| item.as_millis() as u64)
        .unwrap_or_default()
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}
