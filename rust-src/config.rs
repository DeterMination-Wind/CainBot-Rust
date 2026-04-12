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
    pub workflow_agent: WorkflowAgentConfig,
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
    pub forward_threshold_chars: usize,
    pub upload_retry_attempts: usize,
    pub upload_retry_delay_ms: u64,
    pub upload_stable_wait_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotConfig {
    pub owner_user_id: String,
    pub display_name: String,
    pub group_nickname: String,
    pub log_level: String,
    pub log_dir: Option<PathBuf>,
    pub state_file: PathBuf,
    pub runtime_config_file: PathBuf,
    pub reply_errors_to_chat: bool,
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
    pub owner_name: String,
    pub codex_root: Option<PathBuf>,
    pub codex_command: String,
    pub model: String,
    pub classify_model: String,
    pub consent_model: String,
    pub followup_model: String,
    pub satisfaction_model: String,
    pub publish_group_id: String,
    pub offer_group_ids: Vec<String>,
    pub codex_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowAgentConfig {
    pub enabled: bool,
    pub owner_only: bool,
    pub allow_private: bool,
    pub auto_trigger_on_mention: bool,
    pub trigger_group_ids: Vec<String>,
    pub work_dir: Option<PathBuf>,
    pub codex_command: String,
    pub model: String,
    pub classify_model: String,
    pub followup_model: String,
    pub satisfaction_model: String,
    pub codex_timeout_ms: u64,
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
    pub enabled: bool,
    pub enabled_group_ids: Vec<String>,
    pub external_exclusive_groups_file: Option<PathBuf>,
    pub external_exclusive_groups_refresh_ms: u64,
    pub external_exclusive_groups_stale_ms: u64,
    pub pass_hint_text: String,
    pub client: ChatClientConfig,
    pub filter: QaFilterConfig,
    pub prompt_review: QaPromptReviewConfig,
    pub answer: QaAnswerConfig,
    pub topic_closure: QaTopicClosureConfig,
    pub hallucination_check: QaHallucinationCheckConfig,
    pub shutdown_vote_filter_model: String,
    pub low_information_filter_model: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaFilterConfig {
    pub model: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaPromptReviewConfig {
    pub model: String,
    pub system_prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaAnswerConfig {
    pub model: String,
    pub temperature: f64,
    pub max_context_chars: usize,
    pub max_tool_rounds: usize,
    pub session_ttl_ms: u64,
    pub max_timeline_messages: usize,
    pub context_window_messages: usize,
    pub system_prompt: String,
    pub prompt_image_root: Option<PathBuf>,
    pub codex_root: Option<PathBuf>,
    pub local_build_root: Option<PathBuf>,
    pub vanilla_repo_root: Option<PathBuf>,
    pub x_repo_root: Option<PathBuf>,
    pub memory_file: Option<PathBuf>,
    pub structured_memory_file: Option<PathBuf>,
    pub knowledge_dir: Option<PathBuf>,
    pub memory_model: String,
    pub record_group_memory: bool,
    pub enable_codex_readonly_tools: bool,
    pub web_search: QaWebSearchConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaWebSearchConfig {
    pub enabled: bool,
    pub engine: String,
    pub base_url: String,
    pub request_timeout_ms: u64,
    pub max_results: usize,
    pub user_agent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaTopicClosureConfig {
    pub model: String,
    pub temperature: f64,
    pub idle_minutes: u64,
    pub message_window: usize,
    pub system_prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaHallucinationCheckConfig {
    pub enabled: bool,
    pub model: String,
    pub max_tool_rounds: usize,
    pub temperature: f64,
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

    let translation_prompt = read_prompt_file(
        resolve_maybe_relative(
            &config_dir,
            get_string(&raw, &["translation", "promptFile"])
                .unwrap_or_else(|| "./prompts/translation-system-prompt.txt".to_string()),
        )
        .as_ref(),
        get_string(&raw, &["translation", "systemPrompt"]).unwrap_or_else(|| {
            "你是专业翻译助手。请识别用户提供的文本或图片中的文字，并翻译成简体中文。只返回译文，不要添加说明。".to_string()
        }),
    )
    .await?;
    let filter_prompt = read_prompt_file(
        resolve_maybe_relative(
            &config_dir,
            get_string(&raw, &["qa", "filter", "promptFile"])
                .unwrap_or_else(|| "./prompts/qa-filter-prompt.txt".to_string()),
        )
        .as_ref(),
        get_string(&raw, &["qa", "filter", "prompt"])
            .unwrap_or_else(|| "判断这条群消息是否需要 Cain 回答。".to_string()),
    )
    .await?;
    let prompt_review_prompt = read_prompt_file(
        resolve_maybe_relative(
            &config_dir,
            get_string(&raw, &["qa", "promptReview", "promptFile"])
                .unwrap_or_else(|| "./prompts/qa-prompt-review.txt".to_string()),
        )
        .as_ref(),
        get_string(&raw, &["qa", "promptReview", "systemPrompt"]).unwrap_or_else(|| {
            "将群管理者给 Cain 的自然语言指令润色成安全可执行的 prompt。".to_string()
        }),
    )
    .await?;
    let answer_prompt = read_prompt_file(
        resolve_maybe_relative(
            &config_dir,
            get_string(&raw, &["qa", "answer", "promptFile"])
                .unwrap_or_else(|| "./prompts/chat-system-prompt.txt".to_string()),
        )
        .as_ref(),
        get_string(&raw, &["qa", "answer", "systemPrompt"])
            .unwrap_or_else(|| "你是 Cain。".to_string()),
    )
    .await?;
    let topic_closure_prompt = read_prompt_file(
        resolve_maybe_relative(
            &config_dir,
            get_string(&raw, &["qa", "topicClosure", "promptFile"])
                .unwrap_or_else(|| "./prompts/qa-topic-closure.txt".to_string()),
        )
        .as_ref(),
        get_string(&raw, &["qa", "topicClosure", "systemPrompt"])
            .unwrap_or_else(|| "判断群话题是否应该结束。".to_string()),
    )
    .await?;

    let config = Config {
        napcat: NapcatConfig {
            base_url: get_string(&raw, &["napcat", "baseUrl"])
                .unwrap_or_else(|| "http://127.0.0.1:3000".to_string()),
            event_base_url: get_string(&raw, &["napcat", "eventBaseUrl"])
                .or_else(|| get_string(&raw, &["napcat", "baseUrl"]))
                .unwrap_or_else(|| "http://127.0.0.1:3000".to_string()),
            event_path: get_string(&raw, &["napcat", "eventPath"])
                .unwrap_or_else(|| "/_events".to_string()),
            request_timeout_ms: get_i64(&raw, &["napcat", "requestTimeoutMs"])
                .unwrap_or(20_000)
                .max(1) as u64,
            max_concurrent_events: get_i64(&raw, &["napcat", "maxConcurrentEvents"])
                .unwrap_or(24)
                .max(1) as usize,
            headers: get_object_strings(&raw, &["napcat", "headers"]),
            forward_threshold_chars: get_i64(&raw, &["napcat", "forwardThresholdChars"])
                .unwrap_or(100)
                .max(1) as usize,
            upload_retry_attempts: get_i64(&raw, &["napcat", "uploadRetryAttempts"])
                .unwrap_or(6)
                .max(1) as usize,
            upload_retry_delay_ms: get_i64(&raw, &["napcat", "uploadRetryDelayMs"])
                .unwrap_or(2_500)
                .max(200) as u64,
            upload_stable_wait_ms: get_i64(&raw, &["napcat", "uploadStableWaitMs"])
                .unwrap_or(1_500)
                .max(200) as u64,
        },
        bot: BotConfig {
            owner_user_id: get_string(&raw, &["bot", "ownerUserId"])
                .unwrap_or_else(|| "2712706502".to_string()),
            display_name: get_string(&raw, &["bot", "displayName"])
                .unwrap_or_else(|| "[Bot]Cain".to_string()),
            group_nickname: get_string(&raw, &["bot", "groupNickname"])
                .unwrap_or_else(|| "[Bot]Cain".to_string()),
            log_level: get_string(&raw, &["bot", "logLevel"]).unwrap_or_else(|| "info".to_string()),
            log_dir: resolve_maybe_relative(
                &config_dir,
                get_string(&raw, &["bot", "logDir"]).unwrap_or_else(|| "./data/logs".to_string()),
            ),
            state_file: resolve_required_path(
                &config_dir,
                get_string(&raw, &["bot", "stateFile"])
                    .unwrap_or_else(|| "./data/state.json".to_string()),
            )?,
            runtime_config_file: resolve_required_path(
                &config_dir,
                get_string(&raw, &["bot", "runtimeConfigFile"])
                    .unwrap_or_else(|| "./data/runtime-config.json".to_string()),
            )?,
            reply_errors_to_chat: get_bool(&raw, &["bot", "replyErrorsToChat"]).unwrap_or(false),
        },
        codex_bridge: CodexBridgeConfig {
            enabled: get_bool(&raw, &["codexBridge", "enabled"]).unwrap_or(true),
            host: get_string(&raw, &["codexBridge", "host"])
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            port: get_i64(&raw, &["codexBridge", "port"])
                .unwrap_or(3186)
                .clamp(1, 65535) as u16,
            token: get_string(&raw, &["codexBridge", "token"]).unwrap_or_default(),
        },
        issue_repair: IssueRepairConfig {
            enabled: get_bool(&raw, &["issueRepair", "enabled"]).unwrap_or(true),
            owner_name: get_string(&raw, &["issueRepair", "ownerName"])
                .unwrap_or_else(|| "DeterMination".to_string()),
            codex_root: resolve_maybe_relative(
                &config_dir,
                get_string(&raw, &["issueRepair", "codexRoot"])
                    .or_else(|| get_string(&raw, &["qa", "answer", "codexRoot"]))
                    .unwrap_or_else(|| "../codex".to_string()),
            ),
            codex_command: get_string(&raw, &["issueRepair", "codexCommand"])
                .unwrap_or_else(|| "codex".to_string()),
            model: get_string(&raw, &["issueRepair", "model"])
                .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
            classify_model: get_string(&raw, &["issueRepair", "classifyModel"])
                .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
            consent_model: get_string(&raw, &["issueRepair", "consentModel"])
                .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
            followup_model: get_string(&raw, &["issueRepair", "followupModel"])
                .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
            satisfaction_model: get_string(&raw, &["issueRepair", "satisfactionModel"])
                .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
            publish_group_id: get_string(&raw, &["issueRepair", "publishGroupId"])
                .unwrap_or_default(),
            offer_group_ids: get_array_of_strings(&raw, &["issueRepair", "offerGroupIds"]),
            codex_timeout_ms: get_i64(&raw, &["issueRepair", "codexTimeoutMs"])
                .unwrap_or(1_800_000)
                .max(60_000) as u64,
        },
        workflow_agent: WorkflowAgentConfig {
            enabled: get_bool(&raw, &["workflowAgent", "enabled"]).unwrap_or(true),
            owner_only: get_bool(&raw, &["workflowAgent", "ownerOnly"]).unwrap_or(true),
            allow_private: get_bool(&raw, &["workflowAgent", "allowPrivate"]).unwrap_or(true),
            auto_trigger_on_mention: get_bool(&raw, &["workflowAgent", "autoTriggerOnMention"])
                .unwrap_or(true),
            trigger_group_ids: get_array_of_strings(&raw, &["workflowAgent", "triggerGroupIds"]),
            work_dir: resolve_maybe_relative(
                &config_dir,
                get_string(&raw, &["workflowAgent", "workDir"]).unwrap_or_else(|| ".".to_string()),
            ),
            codex_command: get_string(&raw, &["workflowAgent", "codexCommand"])
                .unwrap_or_else(|| "codex".to_string()),
            model: get_string(&raw, &["workflowAgent", "model"])
                .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
            classify_model: get_string(&raw, &["workflowAgent", "classifyModel"])
                .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
            followup_model: get_string(&raw, &["workflowAgent", "followupModel"])
                .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
            satisfaction_model: get_string(&raw, &["workflowAgent", "satisfactionModel"])
                .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
            codex_timeout_ms: get_i64(&raw, &["workflowAgent", "codexTimeoutMs"])
                .unwrap_or(1_800_000)
                .max(60_000) as u64,
        },
        translation: TranslationConfig {
            enabled: get_bool(&raw, &["translation", "enabled"]).unwrap_or(true),
            model: get_string(&raw, &["translation", "model"])
                .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
            target_language: get_string(&raw, &["translation", "targetLanguage"])
                .unwrap_or_else(|| "简体中文".to_string()),
            temperature: get_f64(&raw, &["translation", "temperature"]).unwrap_or(0.2),
            system_prompt: translation_prompt,
        },
        qa: QaConfig {
            enabled: get_bool(&raw, &["qa", "enabled"]).unwrap_or(true),
            enabled_group_ids: get_array_of_strings(&raw, &["qa", "enabledGroupIds"]),
            external_exclusive_groups_file: resolve_maybe_relative(
                &config_dir,
                get_string(&raw, &["qa", "externalExclusiveGroupsFile"]).unwrap_or_default(),
            ),
            external_exclusive_groups_refresh_ms: get_i64(
                &raw,
                &["qa", "externalExclusiveGroupsRefreshMs"],
            )
            .unwrap_or(5_000)
            .max(250) as u64,
            external_exclusive_groups_stale_ms: get_i64(
                &raw,
                &["qa", "externalExclusiveGroupsStaleMs"],
            )
            .unwrap_or(90_000)
            .max(1_000) as u64,
            pass_hint_text: get_string(&raw, &["qa", "passHintText"]).unwrap_or_else(|| {
                "如果此问题无人回答，可以试试 at 我再问，或者输入 /chat 来询问 bot。".to_string()
            }),
            client: ChatClientConfig {
                enabled: true,
                base_url: get_string(&raw, &["qa", "baseUrl"])
                    .unwrap_or_else(|| shared_ai_base_url.clone()),
                api_key: get_string(&raw, &["qa", "apiKey"])
                    .unwrap_or_else(|| shared_ai_api_key.clone()),
                model: get_string(&raw, &["qa", "answer", "model"])
                    .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
                temperature: get_f64(&raw, &["qa", "answer", "temperature"]).unwrap_or(0.4),
                request_timeout_ms: get_i64(&raw, &["qa", "requestTimeoutMs"]).unwrap_or(90_000)
                    as u64,
                retry_attempts: get_i64(&raw, &["qa", "retryAttempts"]).unwrap_or(3).max(1)
                    as usize,
                retry_delay_ms: get_i64(&raw, &["qa", "retryDelayMs"])
                    .unwrap_or(1_500)
                    .max(200) as u64,
                failure_cooldown_ms: get_i64(&raw, &["qa", "failureCooldownMs"])
                    .unwrap_or(60_000)
                    .max(1_000) as u64,
                failure_cooldown_threshold: get_i64(&raw, &["qa", "failureCooldownThreshold"])
                    .unwrap_or(2)
                    .max(1) as usize,
            },
            filter: QaFilterConfig {
                model: get_string(&raw, &["qa", "filter", "model"])
                    .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
                prompt: filter_prompt,
            },
            prompt_review: QaPromptReviewConfig {
                model: get_string(&raw, &["qa", "promptReview", "model"])
                    .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
                system_prompt: prompt_review_prompt,
            },
            answer: QaAnswerConfig {
                model: get_string(&raw, &["qa", "answer", "model"])
                    .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
                temperature: get_f64(&raw, &["qa", "answer", "temperature"]).unwrap_or(0.4),
                max_context_chars: get_i64(&raw, &["qa", "answer", "maxContextChars"])
                    .unwrap_or(80_000)
                    .max(2_000) as usize,
                max_tool_rounds: get_i64(&raw, &["qa", "answer", "maxToolRounds"])
                    .unwrap_or(4)
                    .max(1) as usize,
                session_ttl_ms: get_i64(&raw, &["qa", "answer", "sessionTtlMs"])
                    .unwrap_or(86_400_000)
                    .max(60_000) as u64,
                max_timeline_messages: get_i64(&raw, &["qa", "answer", "maxTimelineMessages"])
                    .unwrap_or(80)
                    .max(2) as usize,
                context_window_messages: get_i64(&raw, &["qa", "answer", "contextWindowMessages"])
                    .unwrap_or(30)
                    .max(1) as usize,
                system_prompt: answer_prompt,
                prompt_image_root: resolve_maybe_relative(
                    &config_dir,
                    get_string(&raw, &["qa", "answer", "promptImageRoot"])
                        .unwrap_or_else(|| "./prompts".to_string()),
                ),
                codex_root: resolve_maybe_relative(
                    &config_dir,
                    get_string(&raw, &["qa", "answer", "codexRoot"])
                        .unwrap_or_else(|| "../codex".to_string()),
                ),
                local_build_root: resolve_maybe_relative(
                    &config_dir,
                    get_string(&raw, &["qa", "answer", "localBuildRoot"])
                        .unwrap_or_else(|| "../codex/builds".to_string()),
                ),
                vanilla_repo_root: resolve_maybe_relative(
                    &config_dir,
                    get_string(&raw, &["qa", "answer", "vanillaRepoRoot"])
                        .unwrap_or_else(|| "../codex/Mindustry-master".to_string()),
                ),
                x_repo_root: resolve_maybe_relative(
                    &config_dir,
                    get_string(&raw, &["qa", "answer", "xRepoRoot"])
                        .unwrap_or_else(|| "../codex/MindustryX-main".to_string()),
                ),
                memory_file: resolve_maybe_relative(
                    &config_dir,
                    get_string(&raw, &["qa", "answer", "memoryFile"])
                        .unwrap_or_else(|| "./data/cain-longterm-memory.txt".to_string()),
                ),
                structured_memory_file: resolve_maybe_relative(
                    &config_dir,
                    get_string(&raw, &["qa", "answer", "structuredMemoryFile"])
                        .unwrap_or_else(|| "./data/memory.json".to_string()),
                ),
                knowledge_dir: resolve_maybe_relative(
                    &config_dir,
                    get_string(&raw, &["qa", "answer", "knowledgeDir"])
                        .unwrap_or_else(|| "./data/Knowledge".to_string()),
                ),
                memory_model: get_string(&raw, &["qa", "answer", "memoryModel"])
                    .unwrap_or_default(),
                record_group_memory: get_bool(&raw, &["qa", "answer", "recordGroupMemory"])
                    .unwrap_or(true),
                enable_codex_readonly_tools: get_bool(
                    &raw,
                    &["qa", "answer", "enableCodexReadonlyTools"],
                )
                .unwrap_or(true),
                web_search: QaWebSearchConfig {
                    enabled: get_bool(&raw, &["qa", "answer", "webSearch", "enabled"])
                        .unwrap_or(true),
                    engine: get_string(&raw, &["qa", "answer", "webSearch", "engine"])
                        .unwrap_or_else(|| "bing".to_string()),
                    base_url: get_string(&raw, &["qa", "answer", "webSearch", "baseUrl"])
                        .unwrap_or_else(|| "https://www.bing.com/search".to_string()),
                    request_timeout_ms: get_i64(
                        &raw,
                        &["qa", "answer", "webSearch", "requestTimeoutMs"],
                    )
                    .unwrap_or(15_000)
                    .max(1_000) as u64,
                    max_results: get_i64(&raw, &["qa", "answer", "webSearch", "maxResults"])
                        .unwrap_or(6)
                        .clamp(1, 10) as usize,
                    user_agent: get_string(&raw, &["qa", "answer", "webSearch", "userAgent"])
                        .unwrap_or_else(|| {
                            "Mozilla/5.0 (compatible; NapCatCainBot/0.1; +https://github.com/DeterMination-Wind/CainBot-Rust)".to_string()
                        }),
                },
            },
            topic_closure: QaTopicClosureConfig {
                model: get_string(&raw, &["qa", "topicClosure", "model"])
                    .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
                temperature: get_f64(&raw, &["qa", "topicClosure", "temperature"]).unwrap_or(0.2),
                idle_minutes: get_i64(&raw, &["qa", "topicClosure", "idleMinutes"])
                    .unwrap_or(15)
                    .max(1) as u64,
                message_window: get_i64(&raw, &["qa", "topicClosure", "messageWindow"])
                    .unwrap_or(30)
                    .max(1) as usize,
                system_prompt: topic_closure_prompt,
            },
            hallucination_check: QaHallucinationCheckConfig {
                enabled: get_bool(&raw, &["qa", "hallucinationCheck", "enabled"]).unwrap_or(false),
                model: get_string(&raw, &["qa", "hallucinationCheck", "model"])
                    .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
                max_tool_rounds: get_i64(&raw, &["qa", "hallucinationCheck", "maxToolRounds"])
                    .unwrap_or(3)
                    .max(1) as usize,
                temperature: get_f64(&raw, &["qa", "hallucinationCheck", "temperature"])
                    .unwrap_or(0.1),
            },
            shutdown_vote_filter_model: get_string(&raw, &["qa", "shutdownVoteFilterModel"])
                .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
            low_information_filter_model: get_string(&raw, &["qa", "lowInformationFilterModel"])
                .unwrap_or_else(|| "gpt-5.3-codex".to_string()),
        },
    };

    ensure_dir(config.bot.state_file.parent().unwrap_or(Path::new("."))).await?;
    ensure_dir(
        config
            .bot
            .runtime_config_file
            .parent()
            .unwrap_or(Path::new(".")),
    )
    .await?;
    if let Some(log_dir) = config.bot.log_dir.as_ref() {
        ensure_dir(log_dir).await?;
    }
    if let Some(memory_file) = config.qa.answer.memory_file.as_ref()
        && let Some(parent) = memory_file.parent()
    {
        ensure_dir(parent).await?;
    }
    if let Some(structured_memory_file) = config.qa.answer.structured_memory_file.as_ref()
        && let Some(parent) = structured_memory_file.parent()
    {
        ensure_dir(parent).await?;
    }
    if let Some(knowledge_dir) = config.qa.answer.knowledge_dir.as_ref() {
        ensure_dir(knowledge_dir).await?;
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
                .filter_map(|(key, value)| {
                    value
                        .as_str()
                        .map(|item| (key.clone(), item.trim().to_string()))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default()
}
