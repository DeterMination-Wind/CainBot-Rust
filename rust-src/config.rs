use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::fs;

use crate::utils::{ensure_dir, resolve_maybe_relative};

const DEFAULT_OPENAI_COMPAT_BASE_URL: &str = "http://127.0.0.1:15721/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedConfig {
    pub config: Config,
    pub config_dir: PathBuf,
    pub config_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub napcat: NapcatConfig,
    pub bot: BotConfig,
    pub codex_bridge: CodexBridgeConfig,
    pub issue_repair: IssueRepairConfig,
    pub translation: TranslationConfig,
    pub qa: QaConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NapcatConfig {
    pub base_url: String,
    pub event_base_url: String,
    pub event_path: String,
    pub request_timeout_ms: u64,
    pub max_concurrent_events: usize,
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotConfig {
    pub owner_user_id: String,
    pub display_name: String,
    pub log_level: String,
    pub log_dir: Option<PathBuf>,
    pub state_file: PathBuf,
    pub runtime_config_file: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexBridgeConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueRepairConfig {
    pub enabled: bool,
    pub codex_root: Option<PathBuf>,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranslationConfig {
    pub enabled: bool,
    pub model: String,
    pub target_language: String,
    pub temperature: f64,
    pub system_prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaConfig {
    pub enabled_group_ids: Vec<String>,
    pub external_exclusive_groups_file: Option<PathBuf>,
    pub external_exclusive_groups_refresh_ms: u64,
    pub client: ChatClientConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatClientConfig {
    pub enabled: bool,
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

pub async fn load_config(config_path: impl AsRef<Path>) -> Result<LoadedConfig> {
    let absolute_config_path = config_path.as_ref().to_path_buf();
    let config_dir = absolute_config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let config_text = fs::read_to_string(&absolute_config_path)
        .await
        .with_context(|| format!("读取配置失败: {}", absolute_config_path.display()))?;
    let raw: Value = serde_json::from_str(&config_text)
        .with_context(|| format!("配置 JSON 非法: {}", absolute_config_path.display()))?;

    let shared_ai_base_url = get_string(&raw, &["ai", "baseUrl"])
        .or_else(|| get_string(&raw, &["qa", "baseUrl"]))
        .or_else(|| get_string(&raw, &["translation", "baseUrl"]))
        .unwrap_or_else(|| DEFAULT_OPENAI_COMPAT_BASE_URL.to_string());
    let shared_ai_api_key = get_string(&raw, &["ai", "apiKey"])
        .or_else(|| get_string(&raw, &["qa", "apiKey"]))
        .or_else(|| get_string(&raw, &["translation", "apiKey"]))
        .unwrap_or_default();

    let translation_prompt_file = resolve_maybe_relative(
        &config_dir,
        get_string(&raw, &["translation", "promptFile"])
            .unwrap_or_else(|| "./prompts/translation-system-prompt.txt".to_string()),
    );
    let translation_prompt = read_prompt_file(
        translation_prompt_file.as_ref(),
        get_string(&raw, &["translation", "systemPrompt"]).unwrap_or_else(|| {
            "你是专业翻译助手。请识别用户提供的文本或图片中的文字，并翻译成简体中文。只返回译文，不要添加说明。".to_string()
        }),
    )
    .await?;

    let config = Config {
        napcat: NapcatConfig {
            base_url: get_string(&raw, &["napcat", "baseUrl"])
                .unwrap_or_else(|| "http://127.0.0.1:3000".to_string()),
            event_base_url: get_string(&raw, &["napcat", "eventBaseUrl"])
                .or_else(|| get_string(&raw, &["napcat", "baseUrl"]))
                .unwrap_or_else(|| "http://127.0.0.1:3000".to_string()),
            event_path: get_string(&raw, &["napcat", "eventPath"]).unwrap_or_else(|| "/_events".to_string()),
            request_timeout_ms: get_i64(&raw, &["napcat", "requestTimeoutMs"]).unwrap_or(20_000).max(1) as u64,
            max_concurrent_events: get_i64(&raw, &["napcat", "maxConcurrentEvents"]).unwrap_or(24).max(1) as usize,
            headers: get_object_strings(&raw, &["napcat", "headers"]),
        },
        bot: BotConfig {
            owner_user_id: get_string(&raw, &["bot", "ownerUserId"]).unwrap_or_else(|| "2712706502".to_string()),
            display_name: get_string(&raw, &["bot", "displayName"]).unwrap_or_else(|| "[Bot]Cain".to_string()),
            log_level: get_string(&raw, &["bot", "logLevel"]).unwrap_or_else(|| "info".to_string()),
            log_dir: resolve_maybe_relative(
                &config_dir,
                get_string(&raw, &["bot", "logDir"]).unwrap_or_else(|| "./data/logs".to_string()),
            ),
            state_file: resolve_required_path(
                &config_dir,
                get_string(&raw, &["bot", "stateFile"]).unwrap_or_else(|| "./data/state.json".to_string()),
            )?,
            runtime_config_file: resolve_required_path(
                &config_dir,
                get_string(&raw, &["bot", "runtimeConfigFile"]).unwrap_or_else(|| "./data/runtime-config.json".to_string()),
            )?,
        },
        codex_bridge: CodexBridgeConfig {
            enabled: get_bool(&raw, &["codexBridge", "enabled"]).unwrap_or(true),
            host: get_string(&raw, &["codexBridge", "host"]).unwrap_or_else(|| "127.0.0.1".to_string()),
            port: get_i64(&raw, &["codexBridge", "port"]).unwrap_or(3186).clamp(1, 65535) as u16,
            token: get_string(&raw, &["codexBridge", "token"]).unwrap_or_default(),
        },
        issue_repair: IssueRepairConfig {
            enabled: get_bool(&raw, &["issueRepair", "enabled"]).unwrap_or(true),
            codex_root: resolve_maybe_relative(
                &config_dir,
                get_string(&raw, &["issueRepair", "codexRoot"])
                    .or_else(|| get_string(&raw, &["qa", "answer", "codexRoot"]))
                    .unwrap_or_else(|| "../codex".to_string()),
            ),
            model: get_string(&raw, &["issueRepair", "model"]).unwrap_or_else(|| "gpt-5.4-high".to_string()),
        },
        translation: TranslationConfig {
            enabled: get_bool(&raw, &["translation", "enabled"]).unwrap_or(true),
            model: get_string(&raw, &["translation", "model"]).unwrap_or_else(|| "gpt-5.4-mini".to_string()),
            target_language: get_string(&raw, &["translation", "targetLanguage"]).unwrap_or_else(|| "简体中文".to_string()),
            temperature: get_f64(&raw, &["translation", "temperature"]).unwrap_or(0.2),
            system_prompt: translation_prompt,
        },
        qa: QaConfig {
            enabled_group_ids: get_array_of_strings(&raw, &["qa", "enabledGroupIds"]),
            external_exclusive_groups_file: resolve_maybe_relative(
                &config_dir,
                get_string(&raw, &["qa", "externalExclusiveGroupsFile"]).unwrap_or_default(),
            ),
            external_exclusive_groups_refresh_ms: get_i64(&raw, &["qa", "externalExclusiveGroupsRefreshMs"]).unwrap_or(5_000).max(250) as u64,
            client: ChatClientConfig {
                enabled: true,
                base_url: get_string(&raw, &["qa", "baseUrl"]).unwrap_or_else(|| shared_ai_base_url.clone()),
                api_key: get_string(&raw, &["qa", "apiKey"]).unwrap_or_else(|| shared_ai_api_key.clone()),
                model: get_string(&raw, &["qa", "answer", "model"]).unwrap_or_else(|| "gpt-5.4-mini".to_string()),
                temperature: get_f64(&raw, &["qa", "answer", "temperature"]).unwrap_or(0.4),
                request_timeout_ms: get_i64(&raw, &["qa", "requestTimeoutMs"]).unwrap_or(90_000) as u64,
                retry_attempts: get_i64(&raw, &["qa", "retryAttempts"]).unwrap_or(3).max(1) as usize,
                retry_delay_ms: get_i64(&raw, &["qa", "retryDelayMs"]).unwrap_or(1_500).max(200) as u64,
                failure_cooldown_ms: get_i64(&raw, &["qa", "failureCooldownMs"]).unwrap_or(60_000).max(1_000) as u64,
                failure_cooldown_threshold: get_i64(&raw, &["qa", "failureCooldownThreshold"]).unwrap_or(2).max(1) as usize,
            },
        },
    };

    ensure_dir(config.bot.state_file.parent().unwrap_or(Path::new("."))).await?;
    ensure_dir(config.bot.runtime_config_file.parent().unwrap_or(Path::new("."))).await?;
    if let Some(log_dir) = config.bot.log_dir.as_ref() {
        ensure_dir(log_dir).await?;
    }

    Ok(LoadedConfig {
        config,
        config_dir,
        config_path: absolute_config_path,
    })
}

async fn read_prompt_file(prompt_file: Option<&PathBuf>, fallback_text: String) -> Result<String> {
    let mut prompt_text = fallback_text.trim().to_string();
    let Some(path) = prompt_file else {
        return Ok(prompt_text);
    };
    match fs::read_to_string(path).await {
        Ok(text) => {
            let trimmed = text.trim().to_string();
            if !trimmed.is_empty() {
                prompt_text = trimmed;
            }
        }
        Err(_) if !prompt_text.is_empty() => {}
        Err(error) => {
            bail!("读取 prompt 失败 {}: {error}", path.display());
        }
    }
    Ok(prompt_text)
}

fn resolve_required_path(config_dir: &Path, value: String) -> Result<PathBuf> {
    resolve_maybe_relative(config_dir, value).context("配置路径不能为空")
}

fn get_value<'a>(root: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = root;
    for segment in path {
        current = current.get(*segment)?;
    }
    Some(current)
}

fn get_string(root: &Value, path: &[&str]) -> Option<String> {
    get_value(root, path)
        .and_then(Value::as_str)
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
}

fn get_bool(root: &Value, path: &[&str]) -> Option<bool> {
    get_value(root, path).and_then(Value::as_bool)
}

fn get_i64(root: &Value, path: &[&str]) -> Option<i64> {
    get_value(root, path).and_then(|item| match item {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    })
}

fn get_f64(root: &Value, path: &[&str]) -> Option<f64> {
    get_value(root, path).and_then(|item| match item {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    })
}

fn get_array_of_strings(root: &Value, path: &[&str]) -> Vec<String> {
    get_value(root, path)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match item {
                    Value::String(text) => {
                        let normalized = text.trim();
                        (!normalized.is_empty()).then(|| normalized.to_string())
                    }
                    Value::Number(number) => Some(number.to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn get_object_strings(root: &Value, path: &[&str]) -> BTreeMap<String, String> {
    get_value(root, path)
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .filter_map(|(key, value)| value.as_str().map(|item| (key.clone(), item.trim().to_string())))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default()
}
