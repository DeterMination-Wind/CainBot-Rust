use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Local;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::utils::ensure_dir;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum LogLevel {
    Debug = 10,
    Info = 20,
    Warn = 30,
    Error = 40,
}

impl LogLevel {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "debug" => Self::Debug,
            "warn" => Self::Warn,
            "error" => Self::Error,
            _ => Self::Info,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

#[derive(Clone)]
pub struct LogNotification {
    pub level: LogLevel,
    pub text: String,
}

type NotifyHandler =
    Arc<dyn Fn(LogNotification) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

#[derive(Clone)]
pub struct Logger {
    level: LogLevel,
    log_dir: Option<PathBuf>,
    write_lock: Arc<Mutex<()>>,
    non_info_notifier: Arc<Mutex<Option<NotifyHandler>>>,
}

impl Logger {
    pub async fn new(level: &str, log_dir: Option<PathBuf>) -> Result<Self> {
        if let Some(dir) = log_dir.as_ref() {
            ensure_dir(dir).await?;
        }
        Ok(Self {
            level: LogLevel::parse(level),
            log_dir,
            write_lock: Arc::new(Mutex::new(())),
            non_info_notifier: Arc::new(Mutex::new(None)),
        })
    }

    pub async fn set_non_info_notifier<F, Fut>(&self, notifier: F)
    where
        F: Fn(LogNotification) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let wrapped: NotifyHandler = Arc::new(move |payload| Box::pin(notifier(payload)));
        *self.non_info_notifier.lock().await = Some(wrapped);
    }

    pub async fn debug(&self, text: impl Into<String>) {
        let _ = self.write(LogLevel::Debug, text.into()).await;
    }

    pub async fn info(&self, text: impl Into<String>) {
        let _ = self.write(LogLevel::Info, text.into()).await;
    }

    pub async fn warn(&self, text: impl Into<String>) {
        let _ = self.write(LogLevel::Warn, text.into()).await;
    }

    pub async fn error(&self, text: impl Into<String>) {
        let _ = self.write(LogLevel::Error, text.into()).await;
    }

    pub async fn flush(&self) -> Result<()> {
        Ok(())
    }

    async fn write(&self, level: LogLevel, text: String) -> Result<()> {
        if level < self.level {
            return Ok(());
        }

        let prefix = format!("[{}] [{}]", chrono::Utc::now().to_rfc3339(), level.label());
        let line = format!("{prefix} {text}");
        println!("{line}");
        self.write_to_files(&line).await?;

        if level > LogLevel::Info
            && let Some(handler) = self.non_info_notifier.lock().await.clone()
        {
            handler(LogNotification {
                level,
                text: line.clone(),
            })
            .await;
        }
        Ok(())
    }

    async fn write_to_files(&self, line: &str) -> Result<()> {
        let Some(log_dir) = self.log_dir.as_ref() else {
            return Ok(());
        };

        let _guard = self.write_lock.lock().await;
        let latest_path = log_dir.join("latest.log");
        let daily_path = log_dir.join(format!("{}.log", format_local_date()));
        append_line(&latest_path, line).await?;
        if daily_path != latest_path {
            append_line(&daily_path, line).await?;
        }
        Ok(())
    }
}

async fn append_line(path: &Path, line: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("创建日志目录失败: {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("打开日志文件失败: {}", path.display()))?;
    file.write_all(format!("{line}\n").as_bytes())
        .await
        .with_context(|| format!("写入日志文件失败: {}", path.display()))?;
    Ok(())
}

fn format_local_date() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}
