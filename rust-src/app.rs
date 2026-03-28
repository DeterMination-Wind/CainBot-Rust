use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::Value;

use crate::config::load_config;
use crate::logger::Logger;
use crate::napcat_client::{NapCatClient, NapCatClientConfig};
use crate::openai_chat_client::{OpenAiChatClient, OpenAiChatClientConfig};
use crate::openai_translator::{OpenAiTranslator, OpenAiTranslatorConfig};
use crate::runtime_config_store::{RuntimeConfigDefaults, RuntimeConfigStore};
use crate::state_store::StateStore;
use crate::utils::path_exists;
use crate::webui_sync_store::WebUiSyncStore;
use crate::worker_process::WorkerSupervisor;

pub struct AppRuntime {
    pub logger: Logger,
    pub napcat_client: NapCatClient,
    pub runtime_config_store: RuntimeConfigStore,
    pub enabled_static_groups: Vec<String>,
    pub owner_user_id: String,
    pub _state_store: StateStore,
    pub _webui_sync_store: WebUiSyncStore,
    pub _qa_client: Option<OpenAiChatClient>,
    pub _translator: Option<OpenAiTranslator>,
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

        logger
            .info(format!(
                "Cain Rust 运行时已启动，当前已接管基础层：config/logger/state/runtime/openai/napcat。配置文件：{}",
                loaded.config_path.display()
            ))
            .await;
        logger
            .warn("业务层仍在迁移：聊天会话、文件下载、msav、issueRepair、codex bridge 还未接入。")
            .await;

        if !config.bot.owner_user_id.trim().is_empty() {
            let _ = napcat_client
                .send_private_message(
                    &config.bot.owner_user_id,
                    format!(
                        "Cain Rust 运行时已启动。\n当前仅接管基础层，业务层迁移进行中。\n配置：{}",
                        loaded.config_path.display()
                    ),
                )
                .await;
        }

        Ok(Self {
            logger,
            napcat_client,
            runtime_config_store,
            enabled_static_groups: config.qa.enabled_group_ids,
            owner_user_id: config.bot.owner_user_id,
            _state_store: state_store,
            _webui_sync_store: webui_sync_store,
            _qa_client: qa_client,
            _translator: translator,
            _worker_supervisor: worker_supervisor,
        })
    }

    pub async fn run(self) -> Result<()> {
        let event_logger = self.logger.clone();
        let event_runtime_store = self.runtime_config_store.clone();
        let enabled_static_groups = self.enabled_static_groups.clone();
        self.napcat_client
            .start_event_loop(move |event: Value| {
                let event_logger = event_logger.clone();
                let event_runtime_store = event_runtime_store.clone();
                let enabled_static_groups = enabled_static_groups.clone();
                async move {
                    log_event_summary(&event_logger, &event).await;
                    handle_group_invite_stub(&event_logger, &event_runtime_store, &enabled_static_groups, &event).await?;
                    Ok(())
                }
            })
            .await?;
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
    logger
        .info(format!(
            "收到群邀请请求：groupId={group_id}，当前群问答{}",
            if enabled { "已启用" } else { "未启用" }
        ))
        .await;
    Ok(())
}
