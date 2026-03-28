use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::logger::Logger;
use crate::utils::{ensure_dir, now_iso};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatSession {
    #[serde(default)]
    pub messages: Vec<Value>,
    #[serde(default, rename = "updatedAt")]
    pub updated_at: String,
    #[serde(default, rename = "lastActivityAt")]
    pub last_activity_at: String,
    #[serde(default, rename = "lastHintedMessageId")]
    pub last_hinted_message_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IssueRepairState {
    #[serde(default)]
    pub offers: BTreeMap<String, Value>,
    #[serde(default)]
    pub sessions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateData {
    pub version: u32,
    #[serde(default, rename = "chatSessions")]
    pub chat_sessions: BTreeMap<String, ChatSession>,
    #[serde(default, rename = "msavReplyContexts")]
    pub msav_reply_contexts: BTreeMap<String, Value>,
    #[serde(default)]
    pub webui: BTreeMap<String, Value>,
    #[serde(default, rename = "issueRepair")]
    pub issue_repair: IssueRepairState,
}

impl Default for StateData {
    fn default() -> Self {
        Self {
            version: 6,
            chat_sessions: BTreeMap::new(),
            msav_reply_contexts: BTreeMap::new(),
            webui: BTreeMap::new(),
            issue_repair: IssueRepairState::default(),
        }
    }
}

#[derive(Clone)]
pub struct StateStore {
    file_path: PathBuf,
    journal_path: PathBuf,
    logger: Logger,
    state: Arc<Mutex<StateData>>,
    journal_lock: Arc<Mutex<()>>,
}

impl StateStore {
    pub fn new(file_path: PathBuf, logger: Logger) -> Self {
        let journal_path = file_path.with_extension("journal.jsonl");
        Self {
            file_path,
            journal_path,
            logger,
            state: Arc::new(Mutex::new(StateData::default())),
            journal_lock: Arc::new(Mutex::new(())),
        }
    }

    pub async fn load(&self) -> Result<()> {
        match fs::read_to_string(&self.file_path).await {
            Ok(text) => {
                let mut parsed: StateData = serde_json::from_str(&text)
                    .with_context(|| format!("解析状态文件失败: {}", self.file_path.display()))?;
                if parsed.version == 0 {
                    parsed.version = 6;
                }
                *self.state.lock().await = parsed;
                self.apply_journal().await?;
                self.logger.info("已加载状态文件。").await;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = self.file_path.parent() {
                    ensure_dir(parent).await?;
                }
                self.apply_journal().await?;
                self.logger.info("未发现状态文件，将在首次保存时创建。").await;
                Ok(())
            }
            Err(error) => Err(error).with_context(|| format!("读取状态文件失败: {}", self.file_path.display())),
        }
    }

    pub async fn snapshot(&self) -> StateData {
        self.state.lock().await.clone()
    }

    pub async fn get_chat_session(&self, session_key: &str) -> Result<ChatSession> {
        let normalized_key = session_key.trim();
        if normalized_key.is_empty() {
            anyhow::bail!("sessionKey 不能为空");
        }
        let mut state = self.state.lock().await;
        let session = state
            .chat_sessions
            .entry(normalized_key.to_string())
            .or_insert_with(ChatSession::default);
        Ok(session.clone())
    }

    // 保持与原版一致：会话始终只保留最近 N 条消息，避免状态文件无限增长。
    pub async fn append_chat_session_entry(
        &self,
        session_key: &str,
        entry: Value,
        max_messages: usize,
    ) -> Result<ChatSession> {
        let normalized_key = session_key.trim();
        if normalized_key.is_empty() {
            anyhow::bail!("sessionKey 不能为空");
        }
        let mut state = self.state.lock().await;
        let session = state
            .chat_sessions
            .entry(normalized_key.to_string())
            .or_insert_with(ChatSession::default);
        let journal_entry = entry.clone();
        session.messages.push(entry);
        if session.messages.len() > max_messages {
            let start = session.messages.len().saturating_sub(max_messages);
            session.messages = session.messages[start..].to_vec();
        }
        let timestamp = now_iso();
        session.updated_at = timestamp.clone();
        session.last_activity_at = timestamp;
        let snapshot = session.clone();
        drop(state);
        self.append_journal(&StateJournalOp::AppendChatSessionEntry {
            session_key: normalized_key.to_string(),
            entry: journal_entry,
            max_messages,
        })
        .await?;
        Ok(snapshot)
    }

    pub async fn set_chat_session_hinted_message(
        &self,
        session_key: &str,
        message_id: &str,
    ) -> Result<ChatSession> {
        let normalized_key = session_key.trim();
        if normalized_key.is_empty() {
            anyhow::bail!("sessionKey 不能为空");
        }
        let mut state = self.state.lock().await;
        let session = state
            .chat_sessions
            .entry(normalized_key.to_string())
            .or_insert_with(ChatSession::default);
        session.last_hinted_message_id = message_id.trim().to_string();
        session.updated_at = now_iso();
        let snapshot = session.clone();
        drop(state);
        self.append_journal(&StateJournalOp::SetChatSessionHintedMessage {
            session_key: normalized_key.to_string(),
            message_id: message_id.trim().to_string(),
        })
        .await?;
        Ok(snapshot)
    }

    pub async fn clear_chat_session(&self, session_key: &str) -> Result<()> {
        let normalized = session_key.trim().to_string();
        self.state.lock().await.chat_sessions.remove(&normalized);
        self.append_journal(&StateJournalOp::ClearChatSession {
            session_key: normalized,
        })
        .await
    }

    // 只有显式 compact 时才重写整份 snapshot，避免高频业务更新反复 JSON stringify。
    pub async fn save(&self) -> Result<()> {
        let snapshot = self.state.lock().await.clone();
        if let Some(parent) = self.file_path.parent() {
            ensure_dir(parent).await?;
        }
        let text = serde_json::to_string_pretty(&snapshot)?;
        fs::write(&self.file_path, text)
            .await
            .with_context(|| format!("写入状态文件失败: {}", self.file_path.display()))?;
        match fs::remove_file(&self.journal_path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("删除状态 journal 失败: {}", self.journal_path.display()));
            }
        }
        Ok(())
    }

    async fn apply_journal(&self) -> Result<()> {
        let journal_file = match OpenOptions::new().read(true).open(&self.journal_path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("读取状态 journal 失败: {}", self.journal_path.display()));
            }
        };
        let reader = BufReader::new(journal_file);
        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let op: StateJournalOp = serde_json::from_str(&line)
                .with_context(|| format!("解析状态 journal 失败: {}", self.journal_path.display()))?;
            let mut state = self.state.lock().await;
            apply_state_journal_op(&mut state, op);
        }
        Ok(())
    }

    async fn append_journal(&self, op: &StateJournalOp) -> Result<()> {
        let _guard = self.journal_lock.lock().await;
        if let Some(parent) = self.journal_path.parent() {
            ensure_dir(parent).await?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal_path)
            .await
            .with_context(|| format!("打开状态 journal 失败: {}", self.journal_path.display()))?;
        let line = serde_json::to_string(op)?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum StateJournalOp {
    AppendChatSessionEntry {
        session_key: String,
        entry: Value,
        max_messages: usize,
    },
    SetChatSessionHintedMessage {
        session_key: String,
        message_id: String,
    },
    ClearChatSession {
        session_key: String,
    },
}

fn apply_state_journal_op(state: &mut StateData, op: StateJournalOp) {
    match op {
        StateJournalOp::AppendChatSessionEntry {
            session_key,
            entry,
            max_messages,
        } => {
            let session = state.chat_sessions.entry(session_key).or_default();
            session.messages.push(entry);
            if session.messages.len() > max_messages {
                let start = session.messages.len().saturating_sub(max_messages);
                session.messages = session.messages[start..].to_vec();
            }
            let timestamp = now_iso();
            session.updated_at = timestamp.clone();
            session.last_activity_at = timestamp;
        }
        StateJournalOp::SetChatSessionHintedMessage {
            session_key,
            message_id,
        } => {
            let session = state.chat_sessions.entry(session_key).or_default();
            session.last_hinted_message_id = message_id;
            session.updated_at = now_iso();
        }
        StateJournalOp::ClearChatSession { session_key } => {
            state.chat_sessions.remove(&session_key);
        }
    }
}
