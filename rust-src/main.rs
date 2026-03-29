mod app;
mod config;
mod commands;
mod event_utils;
mod logger;
mod message_attachment_reader;
mod message_input;
mod napcat_client;
mod openai_chat_client;
mod openai_translator;
mod runtime_config_store;
mod state_store;
mod utils;
mod webui_sync_store;
mod worker_process;

use anyhow::Result;

use crate::app::{AppRuntime, resolve_config_path};
use crate::worker_process::{WorkerKind, run_worker_mode};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    if let Some(mode) = std::env::args().nth(1)
        && mode == "worker"
    {
        let kind = WorkerKind::parse(&std::env::args().nth(2).unwrap_or_default())?;
        return run_worker_mode(kind).await;
    }
    let project_root = std::env::current_dir()?;
    let runtime = AppRuntime::bootstrap(project_root.clone(), resolve_config_path(&project_root)).await?;
    runtime.run().await
}
