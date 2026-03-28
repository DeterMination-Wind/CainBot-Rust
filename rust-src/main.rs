mod app;
mod config;
mod logger;
mod napcat_client;
mod openai_chat_client;
mod openai_translator;
mod runtime_config_store;
mod state_store;
mod utils;
mod webui_sync_store;

use anyhow::Result;

use crate::app::{AppRuntime, resolve_config_path};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let project_root = std::env::current_dir()?;
    let runtime = AppRuntime::bootstrap(project_root.clone(), resolve_config_path(&project_root)).await?;
    runtime.run().await
}
