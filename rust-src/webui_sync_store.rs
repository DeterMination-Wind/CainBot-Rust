use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::utils::{ensure_dir, now_iso};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MsavTask {
    pub id: String,
    #[serde(default = "default_type")]
    pub r#type: String,
    #[serde(default, rename = "fileName")]
    pub file_name: String,
    #[serde(default, rename = "sourceMessageId")]
    pub source_message_id: String,
    #[serde(default, rename = "noticeMessageId")]
    pub notice_message_id: String,
    #[serde(default, rename = "replyMessageId")]
    pub reply_message_id: String,
    #[serde(default, rename = "messageType")]
    pub message_type: String,
    #[serde(default, rename = "groupId")]
    pub group_id: String,
    #[serde(default, rename = "userId")]
    pub user_id: String,
    #[serde(default = "default_running")]
    pub status: String,
    #[serde(default)]
    pub stage: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub error: String,
    #[serde(default, rename = "resultPreview")]
    pub result_preview: String,
    #[serde(default, rename = "createdAt")]
    pub created_at: String,
    #[serde(default, rename = "updatedAt")]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebUiSyncData {
    pub version: u32,
    #[serde(default, rename = "msavTasks")]
    pub msav_tasks: Vec<MsavTask>,
}

impl Default for WebUiSyncData {
    fn default() -> Self {
        Self {
            version: 2,
            msav_tasks: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct WebUiSyncStore {
    file_path: PathBuf,
    journal_path: PathBuf,
    state: Arc<Mutex<WebUiSyncData>>,
    journal_lock: Arc<Mutex<()>>,
}

impl WebUiSyncStore {
    pub fn new(file_path: PathBuf) -> Self {
        let journal_path = file_path.with_extension("journal.jsonl");
        Self {
            file_path,
            journal_path,
            state: Arc::new(Mutex::new(WebUiSyncData::default())),
            journal_lock: Arc::new(Mutex::new(())),
        }
    }

    pub async fn load(&self) -> Result<()> {
        match fs::read_to_string(&self.file_path).await {
            Ok(text) => {
                let parsed: WebUiSyncData = serde_json::from_str(&text).with_context(|| {
                    format!("解析 WebUI 同步文件失败: {}", self.file_path.display())
                })?;
                *self.state.lock().await = parsed;
                self.apply_journal().await?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.apply_journal().await?;
                self.save().await
            }
            Err(error) => Err(error)
                .with_context(|| format!("读取 WebUI 同步文件失败: {}", self.file_path.display())),
        }
    }

    // 这里只保留最近 200 条任务，和原版行为保持一致，防止面板状态无限增长。
    pub async fn upsert_msav_task(&self, task: MsavTask) -> Result<MsavTask> {
        let normalized = normalize_task(task)?;
        let mut state = self.state.lock().await;
        if let Some(index) = state
            .msav_tasks
            .iter()
            .position(|item| item.id == normalized.id)
        {
            let existing = &mut state.msav_tasks[index];
            *existing = MsavTask {
                updated_at: now_iso(),
                ..normalized.clone()
            };
        } else {
            state.msav_tasks.insert(
                0,
                MsavTask {
                    updated_at: now_iso(),
                    ..normalized.clone()
                },
            );
        }
        state
            .msav_tasks
            .sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        state.msav_tasks.truncate(200);
        drop(state);
        self.append_journal(&WebUiJournalOp::UpsertMsavTask {
            task: normalized.clone(),
        })
        .await?;
        Ok(normalized)
    }

    // 高峰期任务状态更新走 journal，只有 compact 时才回写整份 snapshot。
    pub async fn save(&self) -> Result<()> {
        if let Some(parent) = self.file_path.parent() {
            ensure_dir(parent).await?;
        }
        let snapshot = self.state.lock().await.clone();
        let text = serde_json::to_string_pretty(&snapshot)?;
        fs::write(&self.file_path, text)
            .await
            .with_context(|| format!("写入 WebUI 同步文件失败: {}", self.file_path.display()))?;
        match fs::remove_file(&self.journal_path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("删除 WebUI journal 失败: {}", self.journal_path.display())
                });
            }
        }
        Ok(())
    }

    async fn apply_journal(&self) -> Result<()> {
        let journal_file = match OpenOptions::new().read(true).open(&self.journal_path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("读取 WebUI journal 失败: {}", self.journal_path.display())
                });
            }
        };
        let reader = BufReader::new(journal_file);
        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let op: WebUiJournalOp = serde_json::from_str(&line).with_context(|| {
                format!("解析 WebUI journal 失败: {}", self.journal_path.display())
            })?;
            let mut state = self.state.lock().await;
            apply_webui_journal_op(&mut state, op)?;
        }
        Ok(())
    }

    async fn append_journal(&self, op: &WebUiJournalOp) -> Result<()> {
        let _guard = self.journal_lock.lock().await;
        if let Some(parent) = self.journal_path.parent() {
            ensure_dir(parent).await?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal_path)
            .await
            .with_context(|| format!("打开 WebUI journal 失败: {}", self.journal_path.display()))?;
        let line = serde_json::to_string(op)?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        Ok(())
    }
}

fn normalize_task(task: MsavTask) -> Result<MsavTask> {
    let id = task.id.trim().to_string();
    if id.is_empty() {
        anyhow::bail!("无效的 .msav 任务");
    }
    Ok(MsavTask {
        id,
        r#type: if task.r#type.trim().is_empty() {
            default_type()
        } else {
            task.r#type.trim().to_string()
        },
        file_name: if task.file_name.trim().is_empty() {
            "未知.msav".to_string()
        } else {
            task.file_name.trim().to_string()
        },
        source_message_id: task.source_message_id.trim().to_string(),
        notice_message_id: task.notice_message_id.trim().to_string(),
        reply_message_id: task.reply_message_id.trim().to_string(),
        message_type: task.message_type.trim().to_string(),
        group_id: task.group_id.trim().to_string(),
        user_id: task.user_id.trim().to_string(),
        status: if task.status.trim().is_empty() {
            default_running()
        } else {
            task.status.trim().to_string()
        },
        stage: task.stage.trim().to_string(),
        message: task.message.trim().to_string(),
        error: task.error.trim().to_string(),
        result_preview: task.result_preview.trim().to_string(),
        created_at: if task.created_at.trim().is_empty() {
            now_iso()
        } else {
            task.created_at.trim().to_string()
        },
        updated_at: if task.updated_at.trim().is_empty() {
            now_iso()
        } else {
            task.updated_at.trim().to_string()
        },
    })
}

fn default_type() -> String {
    "msav-analysis".to_string()
}

fn default_running() -> String {
    "running".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum WebUiJournalOp {
    UpsertMsavTask { task: MsavTask },
}

fn apply_webui_journal_op(state: &mut WebUiSyncData, op: WebUiJournalOp) -> Result<()> {
    match op {
        WebUiJournalOp::UpsertMsavTask { task } => {
            let normalized = normalize_task(task)?;
            if let Some(index) = state
                .msav_tasks
                .iter()
                .position(|item| item.id == normalized.id)
            {
                state.msav_tasks[index] = normalized;
            } else {
                state.msav_tasks.insert(0, normalized);
            }
            state
                .msav_tasks
                .sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
            state.msav_tasks.truncate(200);
            Ok(())
        }
    }
}
