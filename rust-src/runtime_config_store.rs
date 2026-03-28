use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::sync::Mutex;

use crate::logger::Logger;
use crate::utils::{ensure_dir, now_iso};

const DEFAULT_FILTER_HEARTBEAT_INTERVAL: u64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QaGroup {
    #[serde(rename = "groupId")]
    pub group_id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true", rename = "proactiveReplyEnabled")]
    pub proactive_reply_enabled: bool,
    #[serde(default, rename = "filterHeartbeatEnabled")]
    pub filter_heartbeat_enabled: bool,
    #[serde(default = "default_filter_heartbeat_interval", rename = "filterHeartbeatInterval")]
    pub filter_heartbeat_interval: u64,
    #[serde(default, rename = "fileDownloadEnabled")]
    pub file_download_enabled: bool,
    #[serde(default, rename = "fileDownloadFolderName")]
    pub file_download_folder_name: String,
    #[serde(default, rename = "createdAt")]
    pub created_at: String,
    #[serde(default, rename = "updatedAt")]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GroupQaOverride {
    #[serde(rename = "groupId")]
    pub group_id: String,
    #[serde(default, rename = "filterPrompt")]
    pub filter_prompt: String,
    #[serde(default, rename = "answerPrompt")]
    pub answer_prompt: String,
    #[serde(default, rename = "createdAt")]
    pub created_at: String,
    #[serde(default, rename = "updatedAt")]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfigData {
    pub version: u32,
    #[serde(default, rename = "qaGroups")]
    pub qa_groups: Vec<QaGroup>,
    #[serde(default, rename = "groupQaOverrides")]
    pub group_qa_overrides: Vec<GroupQaOverride>,
}

impl Default for RuntimeConfigData {
    fn default() -> Self {
        Self {
            version: 7,
            qa_groups: Vec::new(),
            group_qa_overrides: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExternalExclusiveGroupsPayload {
    pub version: u32,
    #[serde(default)]
    pub source: String,
    #[serde(default, rename = "updatedAt")]
    pub updated_at: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default, rename = "groupIds")]
    pub group_ids: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct ExternalExclusiveGroupsState {
    file_path: Option<PathBuf>,
    checked_at_ms: u64,
    refresh_ms: u64,
    mtime_ms: u64,
    payload: ExternalExclusiveGroupsPayload,
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeConfigDefaults {
    pub qa_external_exclusive_groups_file: Option<PathBuf>,
    pub qa_external_exclusive_groups_refresh_ms: u64,
}

#[derive(Clone)]
pub struct RuntimeConfigStore {
    file_path: PathBuf,
    config_dir: PathBuf,
    defaults: RuntimeConfigDefaults,
    logger: Logger,
    data: Arc<Mutex<RuntimeConfigData>>,
    external_exclusive_groups: Arc<Mutex<ExternalExclusiveGroupsState>>,
}

impl RuntimeConfigStore {
    pub fn new(
        file_path: PathBuf,
        config_dir: PathBuf,
        defaults: RuntimeConfigDefaults,
        logger: Logger,
    ) -> Self {
        Self {
            file_path,
            config_dir,
            defaults,
            logger,
            data: Arc::new(Mutex::new(RuntimeConfigData::default())),
            external_exclusive_groups: Arc::new(Mutex::new(ExternalExclusiveGroupsState {
                refresh_ms: 5_000,
                ..Default::default()
            })),
        }
    }

    pub async fn load(&self) -> Result<()> {
        match fs::read_to_string(&self.file_path).await {
            Ok(text) => {
                let mut parsed: RuntimeConfigData = serde_json::from_str(&text)
                    .with_context(|| format!("解析运行时配置失败: {}", self.file_path.display()))?;
                parsed.qa_groups = parsed
                    .qa_groups
                    .into_iter()
                    .map(normalize_qa_group)
                    .filter(|item| !item.group_id.is_empty())
                    .collect();
                parsed.group_qa_overrides = parsed
                    .group_qa_overrides
                    .into_iter()
                    .map(normalize_group_override)
                    .filter(|item| !item.group_id.is_empty())
                    .collect();
                *self.data.lock().await = parsed;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.save().await,
            Err(error) => Err(error).with_context(|| format!("读取运行时配置失败: {}", self.file_path.display())),
        }
    }

    pub async fn get_qa_groups(&self) -> Vec<QaGroup> {
        self.data
            .lock()
            .await
            .qa_groups
            .iter()
            .cloned()
            .map(normalize_qa_group)
            .filter(|item| !item.group_id.is_empty())
            .collect()
    }

    pub async fn is_qa_group_enabled(&self, group_id: &str, static_group_ids: &[String]) -> bool {
        let normalized = group_id.trim();
        if normalized.is_empty() {
            return false;
        }
        if self.is_qa_group_externally_excluded(normalized).await {
            return false;
        }
        if let Some(item) = self
            .get_qa_groups()
            .await
            .into_iter()
            .find(|item| item.group_id == normalized)
        {
            return item.enabled;
        }
        static_group_ids.iter().any(|item| item.trim() == normalized)
    }

    pub async fn is_qa_group_externally_excluded(&self, group_id: &str) -> bool {
        let normalized = group_id.trim();
        if normalized.is_empty() {
            return false;
        }
        self.refresh_external_exclusive_groups_if_needed().await;
        let state = self.external_exclusive_groups.lock().await;
        if state.payload.mode == "all" {
            return true;
        }
        state.payload.group_ids.iter().any(|item| item == normalized)
    }

    // 这块保持原版语义：运行时配置优先于静态配置，外部互斥群文件再做最终裁决。
    pub async fn list_enabled_qa_groups(&self, static_group_ids: &[String]) -> Vec<QaGroup> {
        let mut merged = BTreeMap::<String, QaGroup>::new();
        for group_id in static_group_ids {
            let normalized = group_id.trim();
            if normalized.is_empty() {
                continue;
            }
            merged.insert(
                normalized.to_string(),
                QaGroup {
                    group_id: normalized.to_string(),
                    enabled: true,
                    proactive_reply_enabled: true,
                    filter_heartbeat_enabled: false,
                    filter_heartbeat_interval: DEFAULT_FILTER_HEARTBEAT_INTERVAL,
                    file_download_enabled: false,
                    file_download_folder_name: String::new(),
                    created_at: String::new(),
                    updated_at: String::new(),
                },
            );
        }

        for item in self.get_qa_groups().await {
            if !item.enabled {
                merged.remove(&item.group_id);
                continue;
            }
            merged.insert(item.group_id.clone(), item);
        }

        let mut result = Vec::new();
        for item in merged.into_values() {
            if !self.is_qa_group_externally_excluded(&item.group_id).await {
                result.push(item);
            }
        }
        result
    }

    pub async fn get_group_qa_override(&self, group_id: &str) -> Option<GroupQaOverride> {
        let normalized = group_id.trim();
        if normalized.is_empty() {
            return None;
        }
        self.data
            .lock()
            .await
            .group_qa_overrides
            .iter()
            .find(|item| item.group_id == normalized)
            .cloned()
    }

    pub async fn set_group_qa_override(&self, entry: GroupQaOverride) -> Result<String> {
        let normalized = normalize_group_override(entry);
        if normalized.group_id.is_empty() {
            anyhow::bail!("groupId 不能为空");
        }

        let mut data = self.data.lock().await;
        let index = data
            .group_qa_overrides
            .iter()
            .position(|item| item.group_id == normalized.group_id);

        if normalized.filter_prompt.is_empty() && normalized.answer_prompt.is_empty() {
            if let Some(index) = index {
                data.group_qa_overrides.remove(index);
                drop(data);
                self.save().await?;
                return Ok("removed".to_string());
            }
            return Ok("noop".to_string());
        }

        match index {
            Some(index) => {
                let target = &mut data.group_qa_overrides[index];
                target.filter_prompt = normalized.filter_prompt;
                target.answer_prompt = normalized.answer_prompt;
                target.updated_at = now_iso();
                if target.created_at.is_empty() {
                    target.created_at = now_iso();
                }
                drop(data);
                self.save().await?;
                Ok("updated".to_string())
            }
            None => {
                let mut created = normalized;
                created.created_at = now_iso();
                created.updated_at = created.created_at.clone();
                data.group_qa_overrides.push(created);
                drop(data);
                self.save().await?;
                Ok("created".to_string())
            }
        }
    }

    pub async fn save(&self) -> Result<()> {
        if let Some(parent) = self.file_path.parent() {
            ensure_dir(parent).await?;
        }
        let snapshot = self.data.lock().await.clone();
        let text = serde_json::to_string_pretty(&snapshot)?;
        fs::write(&self.file_path, text)
            .await
            .with_context(|| format!("写入运行时配置失败: {}", self.file_path.display()))
    }

    async fn refresh_external_exclusive_groups_if_needed(&self) {
        let file_candidates = self.external_exclusive_candidates();
        if file_candidates.is_empty() {
            *self.external_exclusive_groups.lock().await = ExternalExclusiveGroupsState::default();
            return;
        }

        let refresh_ms = self.external_exclusive_refresh_ms();
        let now = current_time_ms();
        {
            let state = self.external_exclusive_groups.lock().await;
            if now.saturating_sub(state.checked_at_ms) < refresh_ms {
                return;
            }
        }

        for file_path in &file_candidates {
            match fs::metadata(file_path).await {
                Ok(metadata) => {
                    let mtime_ms = metadata
                        .modified()
                        .ok()
                        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|value| value.as_millis() as u64)
                        .unwrap_or_default();
                    let existing = self.external_exclusive_groups.lock().await.clone();
                    if existing.file_path.as_ref() == Some(file_path) && existing.mtime_ms == mtime_ms {
                        let mut state = self.external_exclusive_groups.lock().await;
                        state.checked_at_ms = now;
                        state.refresh_ms = refresh_ms;
                        return;
                    }
                    match fs::read_to_string(file_path).await {
                        Ok(text) => match serde_json::from_str::<ExternalExclusiveGroupsPayload>(&text) {
                            Ok(mut payload) => {
                                payload.mode = normalize_external_mode(&payload.mode);
                                payload.group_ids = dedupe_groups(payload.group_ids);
                                *self.external_exclusive_groups.lock().await = ExternalExclusiveGroupsState {
                                    file_path: Some(file_path.clone()),
                                    checked_at_ms: now,
                                    refresh_ms,
                                    mtime_ms,
                                    payload,
                                };
                                return;
                            }
                            Err(error) => {
                                self.logger.warn(format!("解析外部互斥群文件失败：{error}")).await;
                                break;
                            }
                        },
                        Err(error) => {
                            self.logger.warn(format!("读取外部互斥群文件失败：{error}")).await;
                            break;
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    self.logger.warn(format!("读取外部互斥群文件失败：{error}")).await;
                    break;
                }
            }
        }

        *self.external_exclusive_groups.lock().await = ExternalExclusiveGroupsState {
            file_path: file_candidates.first().cloned(),
            checked_at_ms: now,
            refresh_ms,
            mtime_ms: 0,
            payload: ExternalExclusiveGroupsPayload::default(),
        };
    }

    fn external_exclusive_candidates(&self) -> Vec<PathBuf> {
        let mut result = Vec::new();
        if let Some(configured) = self.defaults.qa_external_exclusive_groups_file.clone() {
            result.push(configured);
        }
        for candidate in [
            self.config_dir.join("../../OlivOSAIChatAssassin/data/cainbot-exclusive-groups.json"),
            self.config_dir.join("../../NapCatAIChatAssassin/data/cainbot-exclusive-groups.json"),
            self.config_dir.join("../OlivOSAIChatAssassin/data/cainbot-exclusive-groups.json"),
            self.config_dir.join("../NapCatAIChatAssassin/data/cainbot-exclusive-groups.json"),
            PathBuf::from("/OlivOSAIChatAssassin/data/cainbot-exclusive-groups.json"),
            PathBuf::from("/NapCatAIChatAssassin/data/cainbot-exclusive-groups.json"),
        ] {
            if !result.contains(&candidate) {
                result.push(candidate);
            }
        }
        result
    }

    fn external_exclusive_refresh_ms(&self) -> u64 {
        self.defaults.qa_external_exclusive_groups_refresh_ms.max(250)
    }
}

fn normalize_qa_group(entry: QaGroup) -> QaGroup {
    QaGroup {
        group_id: entry.group_id.trim().to_string(),
        enabled: entry.enabled,
        proactive_reply_enabled: entry.proactive_reply_enabled,
        filter_heartbeat_enabled: entry.filter_heartbeat_enabled,
        filter_heartbeat_interval: entry.filter_heartbeat_interval.clamp(1, 1_000),
        file_download_enabled: entry.file_download_enabled,
        file_download_folder_name: entry.file_download_folder_name.trim().to_string(),
        created_at: entry.created_at.trim().to_string(),
        updated_at: entry.updated_at.trim().to_string(),
    }
}

fn normalize_group_override(entry: GroupQaOverride) -> GroupQaOverride {
    GroupQaOverride {
        group_id: entry.group_id.trim().to_string(),
        filter_prompt: entry.filter_prompt.replace("\r\n", "\n").trim().to_string(),
        answer_prompt: entry.answer_prompt.replace("\r\n", "\n").trim().to_string(),
        created_at: entry.created_at.trim().to_string(),
        updated_at: entry.updated_at.trim().to_string(),
    }
}

fn normalize_external_mode(value: &str) -> String {
    if value.trim().eq_ignore_ascii_case("all") {
        "all".to_string()
    } else {
        "list".to_string()
    }
}

fn dedupe_groups(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for value in values {
        let normalized = value.trim().to_string();
        if !normalized.is_empty() && seen.insert(normalized.clone()) {
            result.push(normalized);
        }
    }
    result
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or_default()
}

const fn default_true() -> bool {
    true
}

const fn default_filter_heartbeat_interval() -> u64 {
    DEFAULT_FILTER_HEARTBEAT_INTERVAL
}
