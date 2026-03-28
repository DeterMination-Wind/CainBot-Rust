use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::logger::Logger;

#[derive(Clone)]
pub struct WorkerSupervisor {
    exe_path: PathBuf,
    logger: Logger,
}

impl WorkerSupervisor {
    pub fn new(exe_path: PathBuf, logger: Logger) -> Self {
        Self { exe_path, logger }
    }

    // 重负载模块后续一律从这里起独立进程，避免再把文件扫描、Codex、下载链路塞回主事件循环。
    pub async fn ensure_worker(&self, kind: WorkerKind) -> Result<WorkerProcessHandle> {
        let mut command = Command::new(&self.exe_path);
        command
            .arg("worker")
            .arg(kind.as_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().with_context(|| format!("启动 {} worker 失败", kind.as_str()))?;
        let stdin = child.stdin.take().context("worker stdin 不可用")?;
        let stdout = child.stdout.take().context("worker stdout 不可用")?;
        let mut handle = WorkerProcessHandle {
            kind,
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };
        let pong = handle.request("ping").await?;
        if pong.trim() != "pong" {
            bail!("worker 握手失败: {}", pong.trim());
        }
        self.logger
            .info(format!("已启动隔离 worker：{}", kind.as_str()))
            .await;
        Ok(handle)
    }
}

pub struct WorkerProcessHandle {
    kind: WorkerKind,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl WorkerProcessHandle {
    pub async fn request(&mut self, message: &str) -> Result<String> {
        self.stdin.write_all(message.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;

        let mut line = String::new();
        self.stdout.read_line(&mut line).await?;
        if line.is_empty() {
            bail!("worker {} 提前退出", self.kind.as_str());
        }
        Ok(line)
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        let _ = self.request("shutdown").await;
        let _ = self.child.wait().await;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub enum WorkerKind {
    File,
    Codex,
}

impl WorkerKind {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "file" => Ok(Self::File),
            "codex" => Ok(Self::Codex),
            other => bail!("未知 worker 类型: {other}"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Codex => "codex",
        }
    }
}

pub async fn run_worker_mode(kind: WorkerKind) -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin).lines();
    let mut writer = tokio::io::BufWriter::new(stdout);

    // 先只提供最小握手协议，后续重模块迁进来时直接挂到这条进程边界上。
    while let Some(line) = reader.next_line().await? {
        let line: String = line;
        let response = match line.trim() {
            "ping" => "pong",
            "shutdown" => {
                writer.write_all(b"bye\n").await?;
                writer.flush().await?;
                return Ok(());
            }
            other => match kind {
                WorkerKind::File => {
                    let _ = other;
                    "file-worker-ready"
                }
                WorkerKind::Codex => {
                    let _ = other;
                    "codex-worker-ready"
                }
            },
        };
        writer.write_all(response.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    Ok(())
}
