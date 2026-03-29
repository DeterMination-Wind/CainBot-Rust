use std::path::Path;
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::logger::Logger;

#[derive(Clone)]
pub struct QaSessionWorker {
    inner: Arc<Mutex<QaSessionWorkerHandle>>,
    next_request_id: Arc<AtomicU64>,
}

struct QaSessionWorkerHandle {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl QaSessionWorker {
    pub async fn start(project_root: &Path, config_path: &Path, logger: Logger) -> Result<Self> {
        let script_path = project_root.join("scripts").join("rust-qa-session-worker.mjs");
        let mut child = Command::new("node")
            .arg(&script_path)
            .arg(config_path)
            .current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("启动 QA session worker 失败: {}", script_path.display()))?;

        let stdin = child.stdin.take().context("QA session worker stdin 不可用")?;
        let stdout = child.stdout.take().context("QA session worker stdout 不可用")?;

        let worker = Self {
            inner: Arc::new(Mutex::new(QaSessionWorkerHandle {
                child,
                stdin,
                stdout: BufReader::new(stdout),
            })),
            next_request_id: Arc::new(AtomicU64::new(1)),
        };
        logger.info("QA session 兼容 worker 已启动。").await;
        Ok(worker)
    }

    pub async fn request(&self, action: &str, payload: Value) -> Result<Value> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let request = json!({
            "id": request_id,
            "action": action,
            "payload": payload
        });

        let mut handle = self.inner.lock().await;
        let line = serde_json::to_string(&request)?;
        handle.stdin.write_all(line.as_bytes()).await?;
        handle.stdin.write_all(b"\n").await?;
        handle.stdin.flush().await?;

        let mut response_line = String::new();
        let bytes = handle.stdout.read_line(&mut response_line).await?;
        if bytes == 0 {
            bail!("QA session worker 提前退出");
        }

        let response: Value = serde_json::from_str(response_line.trim())
            .context("解析 QA session worker 响应失败")?;
        if response.get("ok").and_then(Value::as_bool) != Some(true) {
            bail!(
                "QA session worker 请求失败: {}",
                response.get("error").and_then(Value::as_str).unwrap_or("unknown error")
            );
        }
        Ok(response.get("data").cloned().unwrap_or(Value::Null))
    }

    pub async fn stop(&self) -> Result<()> {
        let _ = self.request("shutdown", Value::Null).await;
        let mut handle = self.inner.lock().await;
        let _ = handle.child.wait().await;
        Ok(())
    }
}
