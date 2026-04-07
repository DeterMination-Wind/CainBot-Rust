use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use reqwest::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};

use crate::event_utils::EventContext;
use crate::logger::Logger;
use crate::napcat_client::NapCatClient;
use crate::runtime_config_store::RuntimeConfigStore;

const DEFAULT_GITHUB_API_BASE: &str = "https://api.github.com";
const DEFAULT_DOWNLOAD_TIMEOUT_MS: u64 = 90_000;
const PENDING_SELECTION_TTL_MS: u64 = 10 * 60 * 1000;
const BUILD_TIMEOUT_MS: u64 = 60 * 60 * 1000;
const SHORT_COMMAND_TIMEOUT_MS: u64 = 120_000;
const SUBMODULE_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const PATCH_TIMEOUT_MS: u64 = 20 * 60 * 1000;
const MAX_RELEASE_CHOICES: usize = 6;
const MAX_COMMIT_CHOICES: usize = 30;
const LOCAL_SCAN_MAX_DEPTH: usize = 4;
const MAX_RELEASE_PAGES: usize = 4;
const RELEASES_PER_PAGE: usize = 100;
const RELEASE_MATCH_SCAN_LIMIT: usize = MAX_RELEASE_PAGES * RELEASES_PER_PAGE;
const MAX_CONCURRENT_DOWNLOADS: usize = 5;
const PREFERRED_MIRROR_TTL_MS: u64 = 8 * 60 * 60 * 1000;
const DOWNLOAD_TIMEOUT_ABORT_LIMIT: usize = 3;
const MIRROR_PROBE_TIMEOUT_MS: u64 = 8_000;

const GITHUB_DOWNLOAD_MIRRORS: &[&str] = &[
    "https://github.chenc.dev",
    "https://ghproxy.cfd",
    "https://github.tbedu.top",
    "https://ghproxy.cc",
    "https://gh.monlor.com",
    "https://cdn.akaere.online",
    "https://gh.idayer.com",
    "https://gh.llkk.cc",
    "https://ghpxy.hwinzniej.top",
    "https://github-proxy.memory-echoes.cn",
    "https://git.yylx.win",
    "https://gitproxy.mrhjx.cn",
    "https://gh.fhjhy.top",
    "https://gp.zkitefly.eu.org",
    "https://gh-proxy.com",
    "https://ghfile.geekertao.top",
    "https://j.1lin.dpdns.org",
    "https://ghproxy.imciel.com",
    "https://github-proxy.teach-english.tech",
    "https://gitproxy.click",
    "https://gh.927223.xyz",
    "https://github.ednovas.xyz",
    "https://ghf.xn--eqrr82bzpe.top",
    "https://gh.dpik.top",
    "https://gh.jasonzeng.dev",
    "https://gh.xxooo.cf",
    "https://gh.bugdey.us.kg",
    "https://ghm.078465.xyz",
    "https://j.1win.ggff.net",
    "https://tvv.tw",
    "https://gitproxy.127731.xyz",
    "https://gh.inkchills.cn",
    "https://ghproxy.cxkpro.top",
    "https://gh.sixyin.com",
    "https://github.geekery.cn",
    "https://git.669966.xyz",
    "https://gh.5050net.cn",
    "https://gh.felicity.ac.cn",
    "https://github.dpik.top",
    "https://ghp.keleyaa.com",
    "https://gh.wsmdn.dpdns.org",
    "https://ghproxy.monkeyray.net",
    "https://fastgit.cc",
    "https://gh.catmak.name",
    "https://gh.noki.icu",
];

#[derive(Clone)]
pub struct GroupFileDownloadWorker {
    runtime_config_store: RuntimeConfigStore,
    napcat_client: NapCatClient,
    logger: Logger,
    client: Client,
    download_root: PathBuf,
    github_api_base: String,
    github_token: String,
    local_build_root: Option<PathBuf>,
    vanilla_repo_root: Option<PathBuf>,
    x_repo_root: Option<PathBuf>,
    pending_selections: Arc<Mutex<HashMap<String, PendingSelection>>>,
    file_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    preferred_mirror_base: Arc<Mutex<Option<PreferredMirrorBase>>>,
}

#[derive(Debug, Clone)]
struct RepoChoice {
    owner: String,
    repo: String,
}

impl RepoChoice {
    fn repo_key(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    fn is_x_repo(&self) -> bool {
        self.owner.eq_ignore_ascii_case("TinyLake") && self.repo.eq_ignore_ascii_case("MindustryX")
    }

    fn is_vanilla_repo(&self) -> bool {
        self.owner.eq_ignore_ascii_case("Anuken") && self.repo.eq_ignore_ascii_case("Mindustry")
    }

    fn display_name(&self) -> &'static str {
        if self.is_x_repo() {
            "MindustryX X端"
        } else if self.is_vanilla_repo() {
            "Mindustry 原版"
        } else {
            "GitHub 仓库"
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlatformHint {
    Pc,
    Android,
    Server,
    Unknown,
}

impl PlatformHint {
    fn normalized_for_build(self) -> Self {
        match self {
            Self::Unknown => Self::Pc,
            other => other,
        }
    }

    fn as_label(self) -> &'static str {
        match self {
            Self::Pc => "desktop jar",
            Self::Android => "apk",
            Self::Server => "server jar",
            Self::Unknown => "构建产物",
        }
    }

    fn as_key(self) -> &'static str {
        match self {
            Self::Pc => "pc",
            Self::Android => "android",
            Self::Server => "server",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DownloadMode {
    Release,
    CommitBuild,
    LocalBuild,
}

#[derive(Debug, Clone)]
struct DownloadRequest {
    repo: Option<RepoChoice>,
    mode: DownloadMode,
    tag_query: Option<String>,
    commit_hash: Option<String>,
    exact_commit_build: bool,
    platform_hint: PlatformHint,
    folder_name: String,
    request_text: String,
    local_release_choices: Vec<String>,
}

#[derive(Debug, Clone)]
enum PendingSelectionKind {
    Repo {
        request: DownloadRequest,
    },
    Commit {
        request: DownloadRequest,
        commits: Vec<GithubCommit>,
    },
    Release {
        request: DownloadRequest,
        releases: Vec<GithubRelease>,
    },
    CommitRelease {
        request: DownloadRequest,
        candidates: Vec<CommitReleaseCandidate>,
    },
    Asset {
        request: DownloadRequest,
        release: GithubRelease,
        assets: Vec<GithubAsset>,
    },
}

#[derive(Debug, Clone)]
struct PendingSelection {
    group_id: String,
    user_id: String,
    expires_at_ms: u64,
    kind: PendingSelectionKind,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    target_commitish: String,
    #[serde(default)]
    published_at: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubCommitPayload {
    #[serde(default)]
    sha: String,
    #[serde(default)]
    commit: GithubCommitMeta,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct GithubCommitMeta {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Clone)]
struct GithubCommit {
    sha: String,
    title: String,
}

#[derive(Debug, Clone)]
struct CommitReleaseCandidate {
    release: GithubRelease,
    ahead_by: u64,
}

#[derive(Debug)]
enum ReleaseResolution {
    Selected(GithubRelease),
    PendingPrompted,
}

#[derive(Debug)]
enum AssetResolution {
    Selected(GithubAsset),
    PendingPrompted,
}

#[derive(Debug, Clone)]
struct LocalReleaseSpec {
    choice: &'static str,
    display_name: &'static str,
    folder_name: &'static str,
}

#[derive(Debug, Clone)]
struct LocalReleaseCandidate {
    file_path: PathBuf,
    file_name: String,
    ext: String,
    mtime_ms: u64,
}

#[derive(Debug, Clone)]
struct LocalArtifact {
    file_path: PathBuf,
    file_name: String,
    folder_name: String,
}

#[derive(Debug, Clone)]
struct BuiltArtifact {
    file_path: PathBuf,
    file_name: String,
    cleanup: Option<BuildCleanup>,
}

#[derive(Debug, Clone)]
struct BuildCleanup {
    repo_root: PathBuf,
    worktree_dir: PathBuf,
}

#[derive(Debug)]
struct CommandCapture {
    status_code: i32,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Clone)]
struct PreferredMirrorBase {
    base: String,
    expires_at_ms: u64,
}

#[derive(Debug, Clone)]
struct DownloadCandidate {
    label: String,
    url: String,
    use_auth: bool,
    mirror_base: Option<String>,
}

#[derive(Debug)]
struct DownloadBatchOutcome {
    success: bool,
    timeout_count: usize,
    last_error: Option<String>,
    winner_label: String,
    winner_mirror_base: Option<String>,
}

#[derive(Debug)]
struct DownloadTempSuccess {
    index: usize,
    temp_path: PathBuf,
    candidate: DownloadCandidate,
}

#[derive(Debug)]
struct DownloadTempError {
    is_timeout: bool,
    message: String,
}

impl GroupFileDownloadWorker {
    pub async fn start(
        project_root: &Path,
        runtime_config_store: RuntimeConfigStore,
        napcat_client: NapCatClient,
        logger: Logger,
        local_build_root: Option<PathBuf>,
        vanilla_repo_root: Option<PathBuf>,
        x_repo_root: Option<PathBuf>,
    ) -> Result<Self> {
        let download_root = project_root.join("data").join("release-downloads");
        fs::create_dir_all(&download_root)
            .await
            .with_context(|| format!("创建下载目录失败: {}", download_root.display()))?;
        let github_api_base = std::env::var("CAINBOT_GITHUB_API_BASE_URL")
            .ok()
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .unwrap_or_else(|| DEFAULT_GITHUB_API_BASE.to_string());
        let github_token = std::env::var("CAINBOT_GITHUB_TOKEN")
            .ok()
            .or_else(|| std::env::var("GITHUB_TOKEN").ok())
            .unwrap_or_default()
            .trim()
            .to_string();
        let client = Client::builder()
            .timeout(Duration::from_millis(DEFAULT_DOWNLOAD_TIMEOUT_MS))
            .build()
            .context("创建群文件下载 HTTP 客户端失败")?;
        logger
            .info("群文件下载已切换到纯 Rust 实现（release + commit-build + local-build 状态机）。")
            .await;
        Ok(Self {
            runtime_config_store,
            napcat_client,
            logger,
            client,
            download_root,
            github_api_base,
            github_token,
            local_build_root,
            vanilla_repo_root,
            x_repo_root,
            pending_selections: Arc::new(Mutex::new(HashMap::new())),
            file_locks: Arc::new(Mutex::new(HashMap::new())),
            preferred_mirror_base: Arc::new(Mutex::new(None)),
        })
    }

    pub async fn handle_group_message(
        &self,
        context: &EventContext,
        event: &Value,
        text: &str,
    ) -> Result<bool> {
        if context.message_type != "group" {
            return Ok(false);
        }
        let group_id = context.group_id.trim();
        if group_id.is_empty() {
            return Ok(false);
        }

        let _ = self.runtime_config_store.load().await;
        if !self
            .runtime_config_store
            .is_qa_group_file_download_enabled(group_id)
            .await
        {
            return Ok(false);
        }
        self.cleanup_expired_pending_selections().await;

        let request_text = normalize_text(text);
        if self
            .maybe_resume_pending_selection(context, event, request_text.as_str())
            .await?
        {
            return Ok(true);
        }
        if request_text.is_empty() || !looks_like_download_intent(&request_text) {
            return Ok(false);
        }

        let Some(request) = infer_download_request(&request_text, None) else {
            return Ok(false);
        };
        let reply_message_id = event
            .get("message_id")
            .map(value_to_string)
            .unwrap_or_default();
        let result = self
            .execute_request_flow(context, reply_message_id.as_str(), request)
            .await?;
        self.reply_reason_if_failed(context, &reply_message_id, &result)
            .await;
        Ok(true)
    }

    pub async fn start_group_download_flow_from_tool(
        &self,
        context: &EventContext,
        message_id: &str,
        request_text: &str,
        request: &Value,
    ) -> Result<Value> {
        if context.message_type != "group" || context.group_id.trim().is_empty() {
            return Ok(json!({
                "started": false,
                "reason": "仅群聊可用",
                "handled_directly": true
            }));
        }
        let _ = self.runtime_config_store.load().await;
        if !self
            .runtime_config_store
            .is_qa_group_file_download_enabled(&context.group_id)
            .await
        {
            return Ok(json!({
                "started": false,
                "reason": "本群未启用文件下载",
                "handled_directly": true
            }));
        }
        self.cleanup_expired_pending_selections().await;
        let normalized_request_text = normalize_text(request_text);
        let parsed = infer_download_request(&normalized_request_text, Some(request))
            .or_else(|| infer_download_request("", Some(request)))
            .unwrap_or_else(|| DownloadRequest {
                repo: Some(RepoChoice {
                    owner: "TinyLake".to_string(),
                    repo: "MindustryX".to_string(),
                }),
                mode: DownloadMode::Release,
                tag_query: None,
                commit_hash: None,
                exact_commit_build: false,
                platform_hint: detect_platform_hint(&normalized_request_text),
                folder_name: String::new(),
                request_text: normalized_request_text.clone(),
                local_release_choices: detect_local_release_choices(&normalized_request_text),
            });
        self.execute_request_flow(context, message_id, parsed).await
    }

    pub async fn stop(&self) -> Result<()> {
        Ok(())
    }

    async fn maybe_resume_pending_selection(
        &self,
        context: &EventContext,
        event: &Value,
        text: &str,
    ) -> Result<bool> {
        let key = pending_key(context);
        let now = current_time_ms();
        let pending = {
            let guard = self.pending_selections.lock().await;
            guard.get(&key).cloned()
        };
        let Some(pending) = pending else {
            return Ok(false);
        };

        let normalized = normalize_text(text);
        let is_cancel = is_cancel_text(&normalized);
        let maybe_index = parse_selection_index(&normalized);

        if pending.expires_at_ms <= now {
            self.pending_selections.lock().await.remove(&key);
            if maybe_index.is_some() || is_cancel {
                let _ = self
                    .napcat_client
                    .reply_text(
                        "group",
                        &pending.group_id,
                        event.get("message_id").map(value_to_string).as_deref(),
                        "下载选择已过期，请重新发起下载请求。",
                    )
                    .await;
                return Ok(true);
            }
            return Ok(false);
        }

        if is_cancel {
            self.pending_selections.lock().await.remove(&key);
            let _ = self
                .napcat_client
                .reply_text(
                    "group",
                    &pending.group_id,
                    event.get("message_id").map(value_to_string).as_deref(),
                    "已取消本次下载流程。",
                )
                .await;
            return Ok(true);
        }

        match pending.kind.clone() {
            PendingSelectionKind::Repo { mut request } => {
                let repo = parse_repo_choice_from_text(&normalized);
                let Some(repo) = repo else {
                    if looks_like_download_intent(&normalized) {
                        self.pending_selections.lock().await.remove(&key);
                        return Ok(false);
                    }
                    let _ = self
                        .napcat_client
                        .reply_text(
                            "group",
                            &pending.group_id,
                            event.get("message_id").map(value_to_string).as_deref(),
                            "请回复“X端”或“原版”，也可以直接回复 owner/repo。",
                        )
                        .await;
                    return Ok(true);
                };
                request.repo = Some(repo);
                if request.mode != DownloadMode::CommitBuild
                    && request.local_release_choices.is_empty()
                {
                    request.local_release_choices = detect_local_release_choices(&normalized);
                }
                self.pending_selections.lock().await.remove(&key);
                let result = self
                    .execute_request_flow(
                        context,
                        event
                            .get("message_id")
                            .map(value_to_string)
                            .as_deref()
                            .unwrap_or_default(),
                        request,
                    )
                    .await?;
                self.reply_reason_if_failed(
                    context,
                    event
                        .get("message_id")
                        .map(value_to_string)
                        .as_deref()
                        .unwrap_or_default(),
                    &result,
                )
                .await;
                Ok(true)
            }
            PendingSelectionKind::Commit {
                mut request,
                commits,
            } => {
                let direct_hash = parse_commit_hash_from_text(&normalized);
                let selected_hash = if let Some(hash) = direct_hash {
                    Some(hash)
                } else if let Some(index) = maybe_index {
                    if index == 0 || index > commits.len() {
                        let _ = self
                            .napcat_client
                            .reply_text(
                                "group",
                                &pending.group_id,
                                event.get("message_id").map(value_to_string).as_deref(),
                                &format!(
                                    "序号无效，请回复 1 到 {}，或直接回复 commit hash。",
                                    commits.len()
                                ),
                            )
                            .await;
                        return Ok(true);
                    }
                    Some(commits[index - 1].sha.clone())
                } else {
                    if looks_like_download_intent(&normalized) {
                        self.pending_selections.lock().await.remove(&key);
                        return Ok(false);
                    }
                    return Ok(false);
                };

                if let Some(hash) = selected_hash {
                    request.commit_hash = Some(hash);
                }
                self.pending_selections.lock().await.remove(&key);
                let result = self
                    .execute_request_flow(
                        context,
                        event
                            .get("message_id")
                            .map(value_to_string)
                            .as_deref()
                            .unwrap_or_default(),
                        request,
                    )
                    .await?;
                self.reply_reason_if_failed(
                    context,
                    event
                        .get("message_id")
                        .map(value_to_string)
                        .as_deref()
                        .unwrap_or_default(),
                    &result,
                )
                .await;
                Ok(true)
            }
            PendingSelectionKind::Release { request, releases } => {
                let Some(index) = maybe_index else {
                    if looks_like_download_intent(&normalized) {
                        self.pending_selections.lock().await.remove(&key);
                    }
                    return Ok(false);
                };
                if index == 0 || index > releases.len() {
                    let _ = self
                        .napcat_client
                        .reply_text(
                            "group",
                            &pending.group_id,
                            event.get("message_id").map(value_to_string).as_deref(),
                            &format!("序号无效，请回复 1 到 {}。", releases.len()),
                        )
                        .await;
                    return Ok(true);
                }
                self.pending_selections.lock().await.remove(&key);
                let selected_release = releases[index - 1].clone();
                let result = self
                    .execute_with_release(
                        context,
                        event
                            .get("message_id")
                            .map(value_to_string)
                            .as_deref()
                            .unwrap_or_default(),
                        request,
                        selected_release,
                    )
                    .await?;
                self.reply_reason_if_failed(
                    context,
                    event
                        .get("message_id")
                        .map(value_to_string)
                        .as_deref()
                        .unwrap_or_default(),
                    &result,
                )
                .await;
                Ok(true)
            }
            PendingSelectionKind::CommitRelease {
                mut request,
                candidates,
            } => {
                if wants_exact_commit_build(&normalized) {
                    request.exact_commit_build = true;
                    self.pending_selections.lock().await.remove(&key);
                    let result = self
                        .execute_request_flow(
                            context,
                            event
                                .get("message_id")
                                .map(value_to_string)
                                .as_deref()
                                .unwrap_or_default(),
                            request,
                        )
                        .await?;
                    self.reply_reason_if_failed(
                        context,
                        event
                            .get("message_id")
                            .map(value_to_string)
                            .as_deref()
                            .unwrap_or_default(),
                        &result,
                    )
                    .await;
                    return Ok(true);
                }

                let Some(index) = maybe_index else {
                    if looks_like_download_intent(&normalized) {
                        self.pending_selections.lock().await.remove(&key);
                    }
                    return Ok(false);
                };
                if index == 0 || index > candidates.len() {
                    let _ = self
                        .napcat_client
                        .reply_text(
                            "group",
                            &pending.group_id,
                            event.get("message_id").map(value_to_string).as_deref(),
                            &format!(
                                "序号无效，请回复 1 到 {}，或回复“精确编译”。",
                                candidates.len()
                            ),
                        )
                        .await;
                    return Ok(true);
                }
                self.pending_selections.lock().await.remove(&key);
                let selected_release = candidates[index - 1].release.clone();
                let result = self
                    .execute_with_release(
                        context,
                        event
                            .get("message_id")
                            .map(value_to_string)
                            .as_deref()
                            .unwrap_or_default(),
                        request,
                        selected_release,
                    )
                    .await?;
                self.reply_reason_if_failed(
                    context,
                    event
                        .get("message_id")
                        .map(value_to_string)
                        .as_deref()
                        .unwrap_or_default(),
                    &result,
                )
                .await;
                Ok(true)
            }
            PendingSelectionKind::Asset {
                request,
                release,
                assets,
            } => {
                let Some(index) = maybe_index else {
                    if looks_like_download_intent(&normalized) {
                        self.pending_selections.lock().await.remove(&key);
                    }
                    return Ok(false);
                };
                if index == 0 || index > assets.len() {
                    let _ = self
                        .napcat_client
                        .reply_text(
                            "group",
                            &pending.group_id,
                            event.get("message_id").map(value_to_string).as_deref(),
                            &format!("序号无效，请回复 1 到 {}。", assets.len()),
                        )
                        .await;
                    return Ok(true);
                }
                self.pending_selections.lock().await.remove(&key);
                let selected_asset = assets[index - 1].clone();
                let result = self
                    .execute_with_release_asset(
                        context,
                        event
                            .get("message_id")
                            .map(value_to_string)
                            .as_deref()
                            .unwrap_or_default(),
                        request,
                        release,
                        selected_asset,
                    )
                    .await?;
                self.reply_reason_if_failed(
                    context,
                    event
                        .get("message_id")
                        .map(value_to_string)
                        .as_deref()
                        .unwrap_or_default(),
                    &result,
                )
                .await;
                Ok(true)
            }
        }
    }

    async fn execute_request_flow(
        &self,
        context: &EventContext,
        reply_message_id: &str,
        request: DownloadRequest,
    ) -> Result<Value> {
        if request.mode != DownloadMode::CommitBuild && !request.local_release_choices.is_empty() {
            return self
                .resolve_local_releases_and_send(context, reply_message_id, &request)
                .await;
        }

        if request.repo.is_none() {
            self.present_repo_choice(context, reply_message_id, &request)
                .await?;
            return Ok(json!({
                "started": true,
                "pending_selection": true,
                "state": "awaiting_repo_choice",
                "handled_directly": true
            }));
        }

        match request.mode {
            DownloadMode::Release => {
                self.execute_release_download_flow(context, reply_message_id, request)
                    .await
            }
            DownloadMode::CommitBuild => {
                self.resolve_commit_build_and_send(context, reply_message_id, request)
                    .await
            }
            DownloadMode::LocalBuild => {
                self.resolve_local_build_by_repo_and_send(context, reply_message_id, &request)
                    .await
            }
        }
    }

    async fn execute_release_download_flow(
        &self,
        context: &EventContext,
        reply_message_id: &str,
        request: DownloadRequest,
    ) -> Result<Value> {
        let repo = request
            .repo
            .clone()
            .expect("repo should exist in release flow");
        let repo_key = repo.repo_key();
        let tag_display = request
            .tag_query
            .clone()
            .or_else(|| request.commit_hash.clone())
            .unwrap_or_else(|| "latest".to_string());
        let start_tip = format!("开始查找 {repo_key} 的 {tag_display} 版本文件，请稍等。");
        let _ = self
            .napcat_client
            .reply_text(
                "group",
                &context.group_id,
                normalize_option_str(reply_message_id),
                &start_tip,
            )
            .await;

        let release = match self
            .resolve_release(context, reply_message_id, &request, &repo)
            .await?
        {
            ReleaseResolution::Selected(release) => release,
            ReleaseResolution::PendingPrompted => {
                return Ok(json!({
                    "started": true,
                    "pending_selection": true,
                    "handled_directly": true
                }));
            }
        };
        self.execute_with_release(context, reply_message_id, request, release)
            .await
    }

    async fn resolve_commit_build_and_send(
        &self,
        context: &EventContext,
        reply_message_id: &str,
        request: DownloadRequest,
    ) -> Result<Value> {
        let repo = request
            .repo
            .clone()
            .expect("repo should exist in commit flow");
        let platform = request.platform_hint.normalized_for_build();
        let commit_hash = request
            .commit_hash
            .clone()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();

        if commit_hash.is_empty() {
            return self
                .present_commit_choices(context, reply_message_id, request, &repo)
                .await;
        }

        if repo.is_x_repo() {
            if let Some((release, asset)) = self
                .resolve_exact_x_commit_release_asset(
                    &repo,
                    &commit_hash,
                    platform,
                    &request.request_text,
                )
                .await?
            {
                return self
                    .execute_with_release_asset(context, reply_message_id, request, release, asset)
                    .await;
            }

            if !request.exact_commit_build {
                let candidates = self
                    .find_x_commit_release_candidates(&repo, &commit_hash, MAX_RELEASE_CHOICES)
                    .await?;
                if !candidates.is_empty() {
                    let lines = candidates
                        .iter()
                        .enumerate()
                        .map(|(index, candidate)| {
                            let tag = release_display_name(&candidate.release);
                            format!("{}. {} | 后续 {} 提交", index + 1, tag, candidate.ahead_by)
                        })
                        .collect::<Vec<_>>();
                    let prompt = [
                        "这个 commit 没有精确预编译，我找到后续最近的几个 X端预编译：".to_string(),
                        lines.join("\n"),
                        "请在 10 分钟内回复序号选择，或回复“精确编译”走本地构建，回复“取消”退出。"
                            .to_string(),
                    ]
                    .join("\n");
                    self.napcat_client
                        .reply_text(
                            "group",
                            &context.group_id,
                            normalize_option_str(reply_message_id),
                            &prompt,
                        )
                        .await?;
                    self.store_pending_selection(
                        context,
                        PendingSelectionKind::CommitRelease {
                            request,
                            candidates,
                        },
                    )
                    .await;
                    return Ok(json!({
                        "started": true,
                        "pending_selection": true,
                        "state": "awaiting_commit_release_choice",
                        "handled_directly": true
                    }));
                }
            }

            if platform == PlatformHint::Android {
                return Ok(json!({
                    "started": false,
                    "reason": "X端精确 commit 构建暂不支持 Android，请改为 pc 或 server。",
                    "handled_directly": true
                }));
            }

            let _ = self
                .napcat_client
                .reply_text(
                    "group",
                    &context.group_id,
                    normalize_option_str(reply_message_id),
                    &format!(
                        "开始本地精确编译 {} 的 {}。",
                        commit_hash.chars().take(7).collect::<String>(),
                        platform.as_label()
                    ),
                )
                .await;
            let built = match self
                .build_exact_x_commit_artifact(&commit_hash, platform)
                .await
            {
                Ok(item) => item,
                Err(error) => {
                    self.logger
                        .warn(format!(
                            "X端 commit 构建失败：commit={} platform={} error={error:#}",
                            commit_hash,
                            platform.as_key()
                        ))
                        .await;
                    return Ok(json!({
                        "started": false,
                        "reason": format!("X端 commit 构建失败：{error}"),
                        "handled_directly": true
                    }));
                }
            };
            return self
                .upload_built_artifact(context, &request, built, &repo, &commit_hash)
                .await;
        }

        if repo.is_vanilla_repo() {
            if platform == PlatformHint::Android {
                return Ok(json!({
                    "started": false,
                    "reason": "原版 commit 构建目前只支持 pc 或 server。",
                    "handled_directly": true
                }));
            }
            let _ = self
                .napcat_client
                .reply_text(
                    "group",
                    &context.group_id,
                    normalize_option_str(reply_message_id),
                    &format!(
                        "开始本地编译 {} 的 {}。",
                        commit_hash.chars().take(7).collect::<String>(),
                        platform.as_label()
                    ),
                )
                .await;
            let built = match self
                .build_vanilla_commit_artifact(&commit_hash, platform)
                .await
            {
                Ok(item) => item,
                Err(error) => {
                    self.logger
                        .warn(format!(
                            "原版 commit 构建失败：commit={} platform={} error={error:#}",
                            commit_hash,
                            platform.as_key()
                        ))
                        .await;
                    return Ok(json!({
                        "started": false,
                        "reason": format!("原版 commit 构建失败：{error}"),
                        "handled_directly": true
                    }));
                }
            };
            return self
                .upload_built_artifact(context, &request, built, &repo, &commit_hash)
                .await;
        }

        Ok(json!({
            "started": false,
            "reason": format!("{} 暂不支持 commit-build，仅支持 MindustryX 与原版。", repo.repo_key()),
            "handled_directly": true
        }))
    }

    async fn resolve_local_build_by_repo_and_send(
        &self,
        context: &EventContext,
        _reply_message_id: &str,
        request: &DownloadRequest,
    ) -> Result<Value> {
        if !request.local_release_choices.is_empty() {
            return self
                .resolve_local_releases_and_send(context, "", request)
                .await;
        }

        let Some(repo) = request.repo.clone() else {
            return Ok(json!({
                "started": false,
                "reason": "local-build 缺少仓库信息，请指定 X端 或 原版。",
                "handled_directly": true
            }));
        };

        let platform = request.platform_hint.normalized_for_build();
        if platform == PlatformHint::Android {
            return Ok(json!({
                "started": false,
                "reason": "local-build 目前仅支持 pc 或 server。",
                "handled_directly": true
            }));
        }

        let candidate = self.find_local_repo_artifact(&repo, platform).await?;
        let Some((file_path, file_name)) = candidate else {
            return Ok(json!({
                "started": false,
                "reason": format!("本地构建产物不存在：{} {}", repo.display_name(), platform.as_key()),
                "handled_directly": true
            }));
        };

        let folder_name = self
            .resolve_target_folder_name(context, &request.folder_name)
            .await;
        let notify_text = format!("已上传本地构建产物：{}", file_name);
        let upload_result = self
            .napcat_client
            .send_local_file_to_group(
                &context.group_id,
                &file_path.to_string_lossy(),
                Some(file_name.as_str()),
                if folder_name.trim().is_empty() {
                    None
                } else {
                    Some(folder_name.trim())
                },
                Some(notify_text.as_str()),
            )
            .await
            .with_context(|| format!("上传本地构建产物失败: {}", file_path.display()))?;

        Ok(json!({
            "started": true,
            "handled_directly": true,
            "mode": "local-build",
            "repo": repo.repo_key(),
            "platform": platform.as_key(),
            "artifact": file_name,
            "downloadPath": file_path.to_string_lossy(),
            "upload": upload_result
        }))
    }

    async fn resolve_local_releases_and_send(
        &self,
        context: &EventContext,
        _reply_message_id: &str,
        request: &DownloadRequest,
    ) -> Result<Value> {
        let mut choices = request.local_release_choices.clone();
        if choices.is_empty() {
            choices = detect_local_release_choices(&request.request_text);
        }
        choices = unique_strings(choices);
        if choices.is_empty() {
            return Ok(json!({
                "started": false,
                "reason": "未识别到可发送的本地发布类型。",
                "handled_directly": true
            }));
        }

        let mut artifacts = Vec::<LocalArtifact>::new();
        let mut missing = Vec::<String>::new();
        for choice in &choices {
            match self
                .find_latest_local_release_artifact(choice, request)
                .await?
            {
                Some(artifact) => artifacts.push(artifact),
                None => missing.push(local_release_display_name(choice).to_string()),
            }
        }

        if artifacts.is_empty() {
            return Ok(json!({
                "started": false,
                "reason": "本地构建目录里还没找到对应的可发发布文件。",
                "handled_directly": true
            }));
        }

        let _ = self
            .napcat_client
            .reply_text(
                "group",
                &context.group_id,
                None,
                if artifacts.len() > 1 {
                    "开始发送本地最新发布。"
                } else {
                    "开始发送本地最新发布文件。"
                },
            )
            .await;

        let folder_override = self
            .resolve_target_folder_name(context, &request.folder_name)
            .await;
        for artifact in &artifacts {
            let folder_name = if folder_override.trim().is_empty() {
                artifact.folder_name.trim()
            } else {
                folder_override.trim()
            };
            self.napcat_client
                .send_local_file_to_group(
                    &context.group_id,
                    &artifact.file_path.to_string_lossy(),
                    Some(artifact.file_name.as_str()),
                    if folder_name.is_empty() {
                        None
                    } else {
                        Some(folder_name)
                    },
                    None,
                )
                .await
                .with_context(|| {
                    format!("上传本地发布文件失败: {}", artifact.file_path.display())
                })?;
        }

        if !missing.is_empty() {
            let _ = self
                .napcat_client
                .reply_text(
                    "group",
                    &context.group_id,
                    None,
                    &format!("这些资源本地没找到可发文件：{}", missing.join("、")),
                )
                .await;
        }

        Ok(json!({
            "started": true,
            "handled_directly": true,
            "mode": "local-build",
            "sent": artifacts.len(),
            "missing": missing
        }))
    }

    async fn present_repo_choice(
        &self,
        context: &EventContext,
        reply_message_id: &str,
        request: &DownloadRequest,
    ) -> Result<()> {
        self.napcat_client
            .reply_text(
                "group",
                &context.group_id,
                normalize_option_str(reply_message_id),
                "你要的是 MindustryX 的 X端(TinyLake) 还是原版(Anuken)？直接回“X端”或“原版”；回 Cancel 退出。",
            )
            .await?;
        self.store_pending_selection(
            context,
            PendingSelectionKind::Repo {
                request: request.clone(),
            },
        )
        .await;
        Ok(())
    }

    async fn present_commit_choices(
        &self,
        context: &EventContext,
        reply_message_id: &str,
        request: DownloadRequest,
        repo: &RepoChoice,
    ) -> Result<Value> {
        let commits = self.list_commits(repo, MAX_COMMIT_CHOICES).await?;
        if commits.is_empty() {
            return Ok(json!({
                "started": false,
                "reason": format!("{} 这边暂时没读到最近 commit。", repo.display_name()),
                "handled_directly": true
            }));
        }

        let lines = commits
            .iter()
            .enumerate()
            .map(|(index, item)| {
                format!(
                    "{}. {} {}",
                    index + 1,
                    item.sha.chars().take(7).collect::<String>(),
                    if item.title.trim().is_empty() {
                        "(无标题)"
                    } else {
                        item.title.trim()
                    }
                )
            })
            .collect::<Vec<_>>();
        let prompt = [
            format!(
                "这是 {} 最近 {} 个 commit：",
                repo.display_name(),
                commits.len()
            ),
            lines.join("\n"),
            "请在 10 分钟内回复序号，或直接回复 commit hash，回复“取消”退出。".to_string(),
        ]
        .join("\n");
        self.napcat_client
            .reply_text(
                "group",
                &context.group_id,
                normalize_option_str(reply_message_id),
                &prompt,
            )
            .await?;
        self.store_pending_selection(context, PendingSelectionKind::Commit { request, commits })
            .await;
        Ok(json!({
            "started": true,
            "pending_selection": true,
            "state": "awaiting_commit_choice",
            "handled_directly": true
        }))
    }

    async fn execute_with_release(
        &self,
        context: &EventContext,
        reply_message_id: &str,
        request: DownloadRequest,
        release: GithubRelease,
    ) -> Result<Value> {
        let asset = match self
            .resolve_asset(context, reply_message_id, &request, &release)
            .await?
        {
            AssetResolution::Selected(asset) => asset,
            AssetResolution::PendingPrompted => {
                return Ok(json!({
                    "started": true,
                    "pending_selection": true,
                    "handled_directly": true
                }));
            }
        };
        self.execute_with_release_asset(context, reply_message_id, request, release, asset)
            .await
    }

    async fn execute_with_release_asset(
        &self,
        context: &EventContext,
        _reply_message_id: &str,
        request: DownloadRequest,
        release: GithubRelease,
        asset: GithubAsset,
    ) -> Result<Value> {
        let repo = request
            .repo
            .clone()
            .expect("repo should exist when downloading release asset");
        let repo_key = repo.repo_key();
        let file_path = match self.download_asset(&repo, &release, &asset).await {
            Ok(path) => path,
            Err(error) => {
                self.logger
                    .warn(format!(
                        "下载 Release 资产失败：repo={repo_key}, asset={}, error={error:#}",
                        asset.name
                    ))
                    .await;
                return Ok(json!({
                    "started": false,
                    "reason": format!("下载文件失败：{error}"),
                    "handled_directly": true
                }));
            }
        };

        let folder_name = self
            .resolve_target_folder_name(context, &request.folder_name)
            .await;
        let notify_text = format!(
            "已上传：{}（{}）",
            asset.name,
            release_display_name(&release)
        );
        let upload_result = self
            .napcat_client
            .send_local_file_to_group(
                &context.group_id,
                &file_path.to_string_lossy(),
                Some(asset.name.trim()),
                if folder_name.trim().is_empty() {
                    None
                } else {
                    Some(folder_name.trim())
                },
                Some(notify_text.as_str()),
            )
            .await
            .with_context(|| format!("上传群文件失败: {}", file_path.display()))?;

        Ok(json!({
            "started": true,
            "handled_directly": true,
            "repo": repo_key,
            "tag": release.tag_name,
            "asset": asset.name,
            "downloadPath": file_path.to_string_lossy(),
            "releaseUrl": release.html_url,
            "upload": upload_result
        }))
    }

    async fn resolve_release(
        &self,
        context: &EventContext,
        reply_message_id: &str,
        request: &DownloadRequest,
        repo: &RepoChoice,
    ) -> Result<ReleaseResolution> {
        let mut query = request
            .tag_query
            .clone()
            .or_else(|| request.commit_hash.clone())
            .unwrap_or_default()
            .trim()
            .to_string();
        if query.eq_ignore_ascii_case("latest") || query == "最新版" {
            query.clear();
        }

        if query.is_empty() {
            let release = self.fetch_release_latest(repo).await?;
            return Ok(ReleaseResolution::Selected(release));
        }

        let releases = self.fetch_releases(repo, RELEASE_MATCH_SCAN_LIMIT).await?;
        let mut scored = releases
            .into_iter()
            .map(|release| (score_release(&release, &query), release))
            .filter(|(score, release)| *score > 0 && !release.assets.is_empty())
            .collect::<Vec<_>>();
        scored.sort_by(|left, right| right.0.cmp(&left.0));
        if scored.is_empty() {
            let exact = self.fetch_release_by_tag(repo, &query).await?;
            return Ok(ReleaseResolution::Selected(exact));
        }
        if scored.len() == 1
            || scored[0].0 >= scored.get(1).map(|item| item.0).unwrap_or(0) + 40
            || scored[0].0 >= 220
        {
            return Ok(ReleaseResolution::Selected(scored[0].1.clone()));
        }

        let candidates = scored
            .into_iter()
            .take(MAX_RELEASE_CHOICES)
            .map(|(_, release)| release)
            .collect::<Vec<_>>();
        if candidates.len() <= 1 {
            return Ok(ReleaseResolution::Selected(candidates[0].clone()));
        }
        let prompt_lines = candidates
            .iter()
            .enumerate()
            .map(|(index, release)| {
                format!(
                    "{}. {}{}",
                    index + 1,
                    release_display_name(release),
                    if release.prerelease { " (pre)" } else { "" }
                )
            })
            .collect::<Vec<_>>();
        let prompt = [
            format!("匹配到多个版本（{}）：", repo.repo_key()),
            prompt_lines.join("\n"),
            "请在 10 分钟内回复序号选择版本，或回复“取消”。".to_string(),
        ]
        .join("\n");
        self.napcat_client
            .reply_text(
                "group",
                &context.group_id,
                normalize_option_str(reply_message_id),
                &prompt,
            )
            .await?;
        self.store_pending_selection(
            context,
            PendingSelectionKind::Release {
                request: request.clone(),
                releases: candidates,
            },
        )
        .await;
        Ok(ReleaseResolution::PendingPrompted)
    }

    async fn resolve_asset(
        &self,
        context: &EventContext,
        reply_message_id: &str,
        request: &DownloadRequest,
        release: &GithubRelease,
    ) -> Result<AssetResolution> {
        let mut scored = release
            .assets
            .iter()
            .filter(|asset| {
                !asset.name.trim().is_empty() && !asset.browser_download_url.trim().is_empty()
            })
            .map(|asset| {
                (
                    score_asset(asset, request.platform_hint, &request.request_text),
                    asset.clone(),
                )
            })
            .filter(|(score, _)| *score > -40)
            .collect::<Vec<_>>();
        scored.sort_by(|left, right| right.0.cmp(&left.0));
        if scored.is_empty() {
            bail!("未找到可下载资产：{}", release_display_name(release));
        }
        if scored.len() == 1
            || scored[0].0 >= scored.get(1).map(|item| item.0).unwrap_or(0) + 25
            || scored[0].0 >= 120
        {
            return Ok(AssetResolution::Selected(scored[0].1.clone()));
        }

        let candidates = scored
            .into_iter()
            .take(MAX_RELEASE_CHOICES)
            .map(|(_, asset)| asset)
            .collect::<Vec<_>>();
        let lines = candidates
            .iter()
            .enumerate()
            .map(|(index, asset)| format!("{}. {}", index + 1, asset.name))
            .collect::<Vec<_>>();
        let prompt = [
            format!(
                "版本 {} 包含多个可下载文件：",
                release_display_name(release)
            ),
            lines.join("\n"),
            "请在 10 分钟内回复序号选择文件，或回复“取消”。".to_string(),
        ]
        .join("\n");
        self.napcat_client
            .reply_text(
                "group",
                &context.group_id,
                normalize_option_str(reply_message_id),
                &prompt,
            )
            .await?;
        self.store_pending_selection(
            context,
            PendingSelectionKind::Asset {
                request: request.clone(),
                release: release.clone(),
                assets: candidates,
            },
        )
        .await;
        Ok(AssetResolution::PendingPrompted)
    }

    async fn resolve_exact_x_commit_release_asset(
        &self,
        repo: &RepoChoice,
        commit_hash: &str,
        platform: PlatformHint,
        request_text: &str,
    ) -> Result<Option<(GithubRelease, GithubAsset)>> {
        let releases = self.fetch_releases(repo, 24).await?;
        let normalized_commit = commit_hash.trim().to_ascii_lowercase();
        let maybe_release = releases
            .into_iter()
            .filter(|release| {
                let target = normalize_text(&release.target_commitish).to_ascii_lowercase();
                !target.is_empty()
                    && (target.starts_with(&normalized_commit)
                        || normalized_commit.starts_with(&target))
            })
            .max_by(|left, right| release_sort_value(left).cmp(&release_sort_value(right)));
        let Some(release) = maybe_release else {
            return Ok(None);
        };

        let selected_asset = release
            .assets
            .iter()
            .filter(|asset| {
                !asset.name.trim().is_empty() && !asset.browser_download_url.trim().is_empty()
            })
            .map(|asset| (score_asset(asset, platform, request_text), asset.clone()))
            .max_by(|left, right| left.0.cmp(&right.0))
            .map(|(_, asset)| asset);
        if let Some(asset) = selected_asset {
            return Ok(Some((release, asset)));
        }
        Ok(None)
    }

    async fn find_x_commit_release_candidates(
        &self,
        repo: &RepoChoice,
        commit_hash: &str,
        max_candidates: usize,
    ) -> Result<Vec<CommitReleaseCandidate>> {
        let Some(repo_root) = self.x_repo_root.as_ref() else {
            bail!("xRepoRoot 未配置，无法比较 commit 与 release 关系");
        };
        if !path_exists(repo_root).await {
            bail!("X端源码仓库不存在：{}", repo_root.display());
        }

        let releases = self.fetch_releases(repo, 24).await?;
        let resolved_commit = self
            .resolve_git_commit(repo_root, commit_hash)
            .await
            .with_context(|| {
                format!(
                    "本地 X端仓库里找不到 commit {}",
                    commit_hash.chars().take(7).collect::<String>()
                )
            })?;

        let mut candidates = Vec::<CommitReleaseCandidate>::new();
        for release in releases
            .into_iter()
            .filter(|release| !release.target_commitish.trim().is_empty())
        {
            let target = release.target_commitish.trim();
            let resolved_target = match self.try_resolve_git_commit(repo_root, target).await? {
                Some(value) => value,
                None => continue,
            };
            if !self
                .is_git_ancestor(repo_root, &resolved_commit, &resolved_target)
                .await?
            {
                continue;
            }
            let ahead_by = self
                .count_git_revision_distance(repo_root, &resolved_commit, &resolved_target)
                .await
                .unwrap_or(0);
            candidates.push(CommitReleaseCandidate { release, ahead_by });
        }

        candidates.sort_by(|left, right| {
            left.ahead_by.cmp(&right.ahead_by).then_with(|| {
                release_sort_value(&right.release).cmp(&release_sort_value(&left.release))
            })
        });
        candidates.truncate(max_candidates.max(1));
        Ok(candidates)
    }

    async fn fetch_release_latest(&self, repo: &RepoChoice) -> Result<GithubRelease> {
        let endpoint = format!(
            "{}/repos/{}/{}/releases/latest",
            self.github_api_base.trim_end_matches('/'),
            repo.owner,
            repo.repo
        );
        self.get_json::<GithubRelease>(&endpoint).await
    }

    async fn fetch_release_by_tag(&self, repo: &RepoChoice, tag: &str) -> Result<GithubRelease> {
        let endpoint = format!(
            "{}/repos/{}/{}/releases/tags/{}",
            self.github_api_base.trim_end_matches('/'),
            repo.owner,
            repo.repo,
            tag.trim()
        );
        self.get_json::<GithubRelease>(&endpoint).await
    }

    async fn fetch_releases(&self, repo: &RepoChoice, count: usize) -> Result<Vec<GithubRelease>> {
        let desired = count.clamp(1, RELEASE_MATCH_SCAN_LIMIT);
        let per_page = RELEASES_PER_PAGE;

        let mut releases = Vec::<GithubRelease>::new();
        for page in 1..=MAX_RELEASE_PAGES {
            let endpoint = format!(
                "{}/repos/{}/{}/releases?per_page={}&page={}",
                self.github_api_base.trim_end_matches('/'),
                repo.owner,
                repo.repo,
                per_page,
                page
            );
            let current = self.get_json::<Vec<GithubRelease>>(&endpoint).await?;
            let current_len = current.len();
            releases.extend(
                current
                    .into_iter()
                    .filter(|release| !release.draft && !release.assets.is_empty()),
            );
            if releases.len() >= desired {
                break;
            }
            if current_len < per_page {
                break;
            }
        }
        releases.truncate(desired);
        Ok(releases)
    }

    async fn list_commits(
        &self,
        repo: &RepoChoice,
        max_commits: usize,
    ) -> Result<Vec<GithubCommit>> {
        let per_page = max_commits.clamp(1, 100);
        let endpoint = format!(
            "{}/repos/{}/{}/commits?per_page={}",
            self.github_api_base.trim_end_matches('/'),
            repo.owner,
            repo.repo,
            per_page
        );
        let payload = self.get_json::<Vec<GithubCommitPayload>>(&endpoint).await?;
        let commits = payload
            .into_iter()
            .map(|item| {
                let message = item.commit.message.replace("\r\n", "\n");
                let title = message
                    .lines()
                    .next()
                    .map(str::trim)
                    .unwrap_or_default()
                    .to_string();
                GithubCommit {
                    sha: item.sha.trim().to_string(),
                    title,
                }
            })
            .filter(|item| !item.sha.is_empty())
            .take(max_commits)
            .collect::<Vec<_>>();
        Ok(commits)
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, endpoint: &str) -> Result<T> {
        let mut last_error = String::new();
        for attempt in 0..3 {
            let response = self
                .client
                .get(endpoint)
                .headers(self.build_github_api_headers()?)
                .send()
                .await;
            let response = match response {
                Ok(value) => value,
                Err(error) => {
                    last_error = error.to_string();
                    if attempt < 2 {
                        sleep(Duration::from_millis(300 * (attempt as u64 + 1))).await;
                        continue;
                    }
                    break;
                }
            };
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                last_error = format!(
                    "HTTP {}: {}",
                    status,
                    body.chars().take(240).collect::<String>()
                );
                if attempt < 2 && (status.is_server_error() || status.as_u16() == 429) {
                    sleep(Duration::from_millis(350 * (attempt as u64 + 1))).await;
                    continue;
                }
                break;
            }
            return response
                .json::<T>()
                .await
                .context("解析 GitHub API 响应失败");
        }
        bail!("请求 GitHub API 失败: {endpoint} ({last_error})")
    }

    async fn download_asset(
        &self,
        repo: &RepoChoice,
        release: &GithubRelease,
        asset: &GithubAsset,
    ) -> Result<PathBuf> {
        let tag_dir = sanitize_path_component(if release.tag_name.trim().is_empty() {
            "latest"
        } else {
            release.tag_name.trim()
        });
        let repo_dir = sanitize_path_component(&repo.repo_key().replace('/', "_"));
        let file_name = sanitize_path_component(asset.name.trim());
        let target_dir = self.download_root.join(repo_dir).join(tag_dir);
        fs::create_dir_all(&target_dir)
            .await
            .with_context(|| format!("创建下载目录失败: {}", target_dir.display()))?;
        let target_path = target_dir.join(&file_name);
        let file_lock = self
            .get_file_lock(target_path.to_string_lossy().as_ref())
            .await;
        let _guard = file_lock.lock().await;

        if let Ok(meta) = fs::metadata(&target_path).await
            && meta.is_file()
            && meta.len() > 0
            && (asset.size == 0 || meta.len() == asset.size)
        {
            return Ok(target_path);
        }

        let source_url = normalize_text(asset.browser_download_url.trim());
        if source_url.is_empty() {
            bail!("资产 {} 缺少下载地址", asset.name);
        }

        let preferred_base = self.get_preferred_mirror_base().await;
        let mut timeout_count = 0usize;
        let mut last_error = String::new();

        if let Some(base) = preferred_base.as_ref() {
            let preferred_candidate = DownloadCandidate {
                label: format!("preferred:{base}"),
                url: format!("{}/{}", base.trim_end_matches('/'), source_url),
                use_auth: false,
                mirror_base: Some(base.clone()),
            };
            let outcome = self
                .download_candidate_batch(&[preferred_candidate], &target_path, asset.size)
                .await;
            if outcome.success {
                if !outcome.winner_label.trim().is_empty() {
                    self.logger
                        .info(format!("下载候选命中：{}", outcome.winner_label))
                        .await;
                }
                if let Some(winner_base) = outcome.winner_mirror_base.as_ref() {
                    self.remember_preferred_mirror_base(winner_base).await;
                }
                return Ok(target_path);
            }
            timeout_count = timeout_count.saturating_add(outcome.timeout_count);
            if let Some(error) = outcome.last_error {
                last_error = error;
            }
        }

        let candidates = self
            .build_download_candidates(&source_url, preferred_base.as_deref())
            .await;
        for chunk in candidates.chunks(MAX_CONCURRENT_DOWNLOADS.max(1)) {
            let outcome = self
                .download_candidate_batch(chunk, &target_path, asset.size)
                .await;
            if outcome.success {
                if !outcome.winner_label.trim().is_empty() {
                    self.logger
                        .info(format!("下载候选命中：{}", outcome.winner_label))
                        .await;
                }
                if let Some(winner_base) = outcome.winner_mirror_base.as_ref() {
                    self.remember_preferred_mirror_base(winner_base).await;
                }
                return Ok(target_path);
            }
            timeout_count = timeout_count.saturating_add(outcome.timeout_count);
            if let Some(error) = outcome.last_error {
                last_error = error;
            }
            if timeout_count >= DOWNLOAD_TIMEOUT_ABORT_LIMIT {
                bail!("下载超时次数过多（{} 次），已中止本次下载。", timeout_count);
            }
        }

        if last_error.is_empty() {
            bail!("下载失败：无可用下载候选");
        }
        bail!("下载失败：{}", last_error);
    }

    async fn build_download_candidates(
        &self,
        source_url: &str,
        excluded_mirror: Option<&str>,
    ) -> Vec<DownloadCandidate> {
        let mut candidates = Vec::<DownloadCandidate>::new();
        let mut seen = HashSet::<String>::new();

        let ranked = self.probe_mirror_latencies(source_url).await;
        for item in ranked.into_iter().take(MAX_CONCURRENT_DOWNLOADS) {
            if let Some(excluded) = excluded_mirror
                && item.0 == excluded
            {
                continue;
            }
            let url = format!("{}/{}", item.0.trim_end_matches('/'), source_url);
            if seen.insert(url.clone()) {
                candidates.push(DownloadCandidate {
                    label: format!("mirror:{}", item.0),
                    url,
                    use_auth: false,
                    mirror_base: Some(item.0),
                });
            }
        }

        if seen.insert(source_url.to_string()) {
            candidates.push(DownloadCandidate {
                label: "source:github".to_string(),
                url: source_url.to_string(),
                use_auth: true,
                mirror_base: None,
            });
        }
        candidates
    }

    async fn probe_mirror_latencies(&self, source_url: &str) -> Vec<(String, u64)> {
        let mut tasks = FuturesUnordered::new();
        for base in GITHUB_DOWNLOAD_MIRRORS {
            let mirror = base.trim().to_string();
            if mirror.is_empty() {
                continue;
            }
            tasks.push(self.probe_single_mirror(mirror.clone(), source_url.to_string()));
        }

        let mut successful = Vec::<(String, u64)>::new();
        while let Some(result) = tasks.next().await {
            if let Ok((base, latency)) = result {
                successful.push((base, latency));
            }
        }
        successful.sort_by(|left, right| left.1.cmp(&right.1));
        successful
    }

    async fn probe_single_mirror(&self, base: String, source_url: String) -> Result<(String, u64)> {
        let started = current_time_ms();
        let url = format!("{}/{}", base.trim_end_matches('/'), source_url.trim());
        let response = timeout(
            Duration::from_millis(MIRROR_PROBE_TIMEOUT_MS),
            self.client
                .get(url)
                .headers({
                    let mut headers = HeaderMap::new();
                    headers.insert(USER_AGENT, HeaderValue::from_static("NapCatCainBot-Rust"));
                    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
                    headers.insert("Range", HeaderValue::from_static("bytes=0-0"));
                    headers
                })
                .send(),
        )
        .await
        .context("mirror probe timeout")??;
        if !response.status().is_success() && response.status().as_u16() != 206 {
            bail!("HTTP {}", response.status());
        }
        Ok((base, current_time_ms().saturating_sub(started)))
    }

    async fn download_candidate_batch(
        &self,
        candidates: &[DownloadCandidate],
        target_path: &Path,
        expected_size: u64,
    ) -> DownloadBatchOutcome {
        if candidates.is_empty() {
            return DownloadBatchOutcome {
                success: false,
                timeout_count: 0,
                last_error: Some("下载候选为空".to_string()),
                winner_label: String::new(),
                winner_mirror_base: None,
            };
        }
        let mut tasks = FuturesUnordered::new();
        for (index, candidate) in candidates.iter().enumerate() {
            let temp_path = PathBuf::from(format!(
                "{}.part-{}.{}.{}",
                target_path.to_string_lossy(),
                index,
                std::process::id(),
                current_time_ms()
            ));
            tasks.push(self.download_url_to_temp_file(
                candidate.clone(),
                temp_path,
                index,
                expected_size,
            ));
        }

        let mut winner: Option<DownloadTempSuccess> = None;
        let mut timeout_count = 0usize;
        let mut last_error = None::<String>;
        let mut temp_files_to_cleanup = Vec::<PathBuf>::new();

        while let Some(result) = tasks.next().await {
            match result {
                Ok(success) => {
                    if winner.is_none() {
                        winner = Some(DownloadTempSuccess {
                            index: success.index,
                            temp_path: success.temp_path.clone(),
                            candidate: success.candidate.clone(),
                        });
                    } else {
                        temp_files_to_cleanup.push(success.temp_path);
                    }
                }
                Err(error) => {
                    if error.is_timeout {
                        timeout_count = timeout_count.saturating_add(1);
                    }
                    last_error = Some(error.message);
                }
            }
        }

        if let Some(winner) = winner {
            if fs::metadata(target_path).await.is_ok() {
                let _ = fs::remove_file(target_path).await;
            }
            if let Err(error) = fs::rename(&winner.temp_path, target_path).await {
                let _ = fs::remove_file(&winner.temp_path).await;
                return DownloadBatchOutcome {
                    success: false,
                    timeout_count,
                    last_error: Some(format!(
                        "写入下载文件失败: {} ({})",
                        target_path.display(),
                        error
                    )),
                    winner_label: String::new(),
                    winner_mirror_base: None,
                };
            }
            for file in temp_files_to_cleanup {
                let _ = fs::remove_file(file).await;
            }
            return DownloadBatchOutcome {
                success: true,
                timeout_count,
                last_error: None,
                winner_label: winner.candidate.label,
                winner_mirror_base: winner.candidate.mirror_base,
            };
        }

        DownloadBatchOutcome {
            success: false,
            timeout_count,
            last_error,
            winner_label: String::new(),
            winner_mirror_base: None,
        }
    }

    async fn download_url_to_temp_file(
        &self,
        candidate: DownloadCandidate,
        temp_path: PathBuf,
        index: usize,
        expected_size: u64,
    ) -> std::result::Result<DownloadTempSuccess, DownloadTempError> {
        let request = self.client.get(candidate.url.trim()).headers(
            match self.build_github_headers_with_auth(candidate.use_auth) {
                Ok(headers) => headers,
                Err(error) => {
                    return Err(DownloadTempError {
                        is_timeout: false,
                        message: error.to_string(),
                    });
                }
            },
        );

        let response = match request.send().await {
            Ok(resp) => resp,
            Err(error) => {
                return Err(DownloadTempError {
                    is_timeout: error.is_timeout(),
                    message: error.to_string(),
                });
            }
        };
        if !response.status().is_success() {
            return Err(DownloadTempError {
                is_timeout: false,
                message: format!("HTTP {}", response.status()),
            });
        }

        let mut file = match fs::File::create(&temp_path).await {
            Ok(created) => created,
            Err(error) => {
                return Err(DownloadTempError {
                    is_timeout: false,
                    message: error.to_string(),
                });
            }
        };
        let mut stream = response.bytes_stream();
        let mut total_written = 0u64;
        while let Some(chunk) = stream.next().await {
            let bytes = match chunk {
                Ok(value) => value,
                Err(error) => {
                    let _ = fs::remove_file(&temp_path).await;
                    return Err(DownloadTempError {
                        is_timeout: error.is_timeout(),
                        message: error.to_string(),
                    });
                }
            };
            total_written = total_written.saturating_add(bytes.len() as u64);
            if let Err(error) = file.write_all(&bytes).await {
                let _ = fs::remove_file(&temp_path).await;
                return Err(DownloadTempError {
                    is_timeout: false,
                    message: error.to_string(),
                });
            }
        }
        if let Err(error) = file.flush().await {
            let _ = fs::remove_file(&temp_path).await;
            return Err(DownloadTempError {
                is_timeout: false,
                message: error.to_string(),
            });
        }
        if expected_size > 0 && total_written != expected_size {
            let _ = fs::remove_file(&temp_path).await;
            return Err(DownloadTempError {
                is_timeout: false,
                message: format!(
                    "下载文件大小不匹配：expected={}, actual={}",
                    expected_size, total_written
                ),
            });
        }

        Ok(DownloadTempSuccess {
            index,
            temp_path,
            candidate,
        })
    }

    async fn get_preferred_mirror_base(&self) -> Option<String> {
        let now = current_time_ms();
        let mut guard = self.preferred_mirror_base.lock().await;
        let item = guard.clone()?;
        if item.expires_at_ms <= now {
            *guard = None;
            return None;
        }
        if item.base.trim().is_empty() {
            *guard = None;
            return None;
        }
        Some(item.base)
    }

    async fn remember_preferred_mirror_base(&self, base: &str) {
        let normalized = normalize_text(base);
        if normalized.is_empty() {
            return;
        }
        let mut guard = self.preferred_mirror_base.lock().await;
        *guard = Some(PreferredMirrorBase {
            base: normalized,
            expires_at_ms: current_time_ms().saturating_add(PREFERRED_MIRROR_TTL_MS),
        });
    }

    async fn build_vanilla_commit_artifact(
        &self,
        commit_hash: &str,
        platform: PlatformHint,
    ) -> Result<BuiltArtifact> {
        if platform == PlatformHint::Android {
            bail!("原版 commit 构建暂不支持 Android")
        }
        let repo_root = self
            .vanilla_repo_root
            .clone()
            .context("vanillaRepoRoot 未配置，无法执行原版 commit 构建")?;
        if !path_exists(&repo_root.join(".git")).await {
            bail!("原版源码仓库不存在：{}", repo_root.display());
        }

        let full_commit = self.resolve_git_commit(&repo_root, commit_hash).await?;
        let worktree_dir = self
            .create_git_worktree(&repo_root, &full_commit, "mindustry")
            .await?;
        let cleanup = BuildCleanup {
            repo_root: repo_root.clone(),
            worktree_dir: worktree_dir.clone(),
        };

        let result = async {
            let gradlew =
                find_gradle_wrapper(&worktree_dir).context("未找到 gradlew/gradlew.bat")?;
            let task_args = if platform == PlatformHint::Server {
                vec![":server:dist".to_string()]
            } else {
                vec![
                    ":desktop:dist".to_string(),
                    "-x".to_string(),
                    "test".to_string(),
                ]
            };
            self.run_command_checked(
                &gradlew.to_string_lossy(),
                &task_args,
                Some(&worktree_dir),
                BUILD_TIMEOUT_MS,
                &format!(
                    "vanilla-build:{}",
                    full_commit.chars().take(7).collect::<String>()
                ),
            )
            .await?;

            let artifact_path = if platform == PlatformHint::Server {
                worktree_dir
                    .join("server")
                    .join("build")
                    .join("libs")
                    .join("server-release.jar")
            } else {
                worktree_dir
                    .join("desktop")
                    .join("build")
                    .join("libs")
                    .join("Mindustry.jar")
            };
            if !path_exists(&artifact_path).await {
                bail!("构建成功但没找到产物：{}", artifact_path.display());
            }
            let artifact_name = if platform == PlatformHint::Server {
                format!(
                    "Mindustry-server-{}.jar",
                    full_commit.chars().take(7).collect::<String>()
                )
            } else {
                format!(
                    "Mindustry-desktop-{}.jar",
                    full_commit.chars().take(7).collect::<String>()
                )
            };
            Ok(BuiltArtifact {
                file_path: artifact_path,
                file_name: artifact_name,
                cleanup: Some(cleanup.clone()),
            })
        }
        .await;

        if result.is_err() {
            self.cleanup_build_worktree(&cleanup).await;
        }
        result
    }

    async fn build_exact_x_commit_artifact(
        &self,
        commit_hash: &str,
        platform: PlatformHint,
    ) -> Result<BuiltArtifact> {
        if platform == PlatformHint::Android {
            bail!("X端本地精确编译暂不支持 Android")
        }
        let repo_root = self
            .x_repo_root
            .clone()
            .context("xRepoRoot 未配置，无法执行 X端 commit 构建")?;
        if !path_exists(&repo_root.join("scripts").join("applyPatches.sh")).await {
            bail!("X端源码仓库不存在：{}", repo_root.display());
        }

        let full_commit = self.resolve_git_commit(&repo_root, commit_hash).await?;
        let worktree_dir = self
            .create_git_worktree(&repo_root, &full_commit, "mindustryx")
            .await?;
        let cleanup = BuildCleanup {
            repo_root: repo_root.clone(),
            worktree_dir: worktree_dir.clone(),
        };

        let result = async {
            self.run_command_checked(
                "git",
                &vec![
                    "-C".to_string(),
                    worktree_dir.to_string_lossy().to_string(),
                    "submodule".to_string(),
                    "sync".to_string(),
                    "--recursive".to_string(),
                ],
                None,
                SHORT_COMMAND_TIMEOUT_MS,
                "mindustryx-submodule-sync",
            )
            .await?;
            self.run_command_checked(
                "git",
                &vec![
                    "-C".to_string(),
                    worktree_dir.to_string_lossy().to_string(),
                    "submodule".to_string(),
                    "update".to_string(),
                    "--init".to_string(),
                    "--recursive".to_string(),
                    "--jobs".to_string(),
                    "4".to_string(),
                ],
                None,
                SUBMODULE_TIMEOUT_MS,
                "mindustryx-submodule-update",
            )
            .await?;

            let script = worktree_dir.join("scripts").join("applyPatches.sh");
            if !path_exists(&script).await {
                bail!("X端补丁脚本不存在：{}", script.display());
            }
            self.run_bash_script(&script, &worktree_dir, PATCH_TIMEOUT_MS)
                .await?;

            let work_root = worktree_dir.join("work");
            let gradlew =
                find_gradle_wrapper(&work_root).context("未找到 work/gradlew/gradlew.bat")?;
            let task_args = if platform == PlatformHint::Server {
                vec![":server:dist".to_string()]
            } else {
                vec![
                    ":desktop:dist".to_string(),
                    "-x".to_string(),
                    "test".to_string(),
                ]
            };
            self.run_command_checked(
                &gradlew.to_string_lossy(),
                &task_args,
                Some(&work_root),
                BUILD_TIMEOUT_MS,
                &format!(
                    "mindustryx-build:{}",
                    full_commit.chars().take(7).collect::<String>()
                ),
            )
            .await?;

            let artifact_path = if platform == PlatformHint::Server {
                work_root
                    .join("server")
                    .join("build")
                    .join("libs")
                    .join("server-release.jar")
            } else {
                work_root
                    .join("desktop")
                    .join("build")
                    .join("libs")
                    .join("Mindustry.jar")
            };
            if !path_exists(&artifact_path).await {
                bail!("X端精确编译成功但没找到产物：{}", artifact_path.display());
            }
            let artifact_name = if platform == PlatformHint::Server {
                format!(
                    "MindustryX-server-{}.jar",
                    full_commit.chars().take(7).collect::<String>()
                )
            } else {
                format!(
                    "MindustryX-desktop-{}.jar",
                    full_commit.chars().take(7).collect::<String>()
                )
            };

            Ok(BuiltArtifact {
                file_path: artifact_path,
                file_name: artifact_name,
                cleanup: Some(cleanup.clone()),
            })
        }
        .await;

        if result.is_err() {
            self.cleanup_build_worktree(&cleanup).await;
        }
        result
    }

    async fn upload_built_artifact(
        &self,
        context: &EventContext,
        request: &DownloadRequest,
        built: BuiltArtifact,
        repo: &RepoChoice,
        commit_hash: &str,
    ) -> Result<Value> {
        let folder_name = self
            .resolve_target_folder_name(context, &request.folder_name)
            .await;
        let notify_text = format!(
            "已上传本地构建：{} ({})",
            built.file_name,
            commit_hash.chars().take(7).collect::<String>()
        );
        let upload_result = self
            .napcat_client
            .send_local_file_to_group(
                &context.group_id,
                &built.file_path.to_string_lossy(),
                Some(built.file_name.as_str()),
                if folder_name.trim().is_empty() {
                    None
                } else {
                    Some(folder_name.trim())
                },
                Some(notify_text.as_str()),
            )
            .await;

        if let Some(cleanup) = built.cleanup.as_ref() {
            self.cleanup_build_worktree(cleanup).await;
        }

        let upload = upload_result
            .with_context(|| format!("上传构建产物失败: {}", built.file_path.display()))?;
        Ok(json!({
            "started": true,
            "handled_directly": true,
            "mode": "commit-build",
            "repo": repo.repo_key(),
            "commit": commit_hash,
            "artifact": built.file_name,
            "downloadPath": built.file_path.to_string_lossy(),
            "upload": upload
        }))
    }

    async fn find_local_repo_artifact(
        &self,
        repo: &RepoChoice,
        platform: PlatformHint,
    ) -> Result<Option<(PathBuf, String)>> {
        let (base_root, is_x) = if repo.is_x_repo() {
            (
                self.x_repo_root
                    .clone()
                    .context("xRepoRoot 未配置，无法读取 X端本地产物")?,
                true,
            )
        } else if repo.is_vanilla_repo() {
            (
                self.vanilla_repo_root
                    .clone()
                    .context("vanillaRepoRoot 未配置，无法读取原版本地产物")?,
                false,
            )
        } else {
            return Ok(None);
        };

        let artifact_path = if is_x {
            if platform == PlatformHint::Server {
                base_root
                    .join("work")
                    .join("server")
                    .join("build")
                    .join("libs")
                    .join("server-release.jar")
            } else {
                base_root
                    .join("work")
                    .join("desktop")
                    .join("build")
                    .join("libs")
                    .join("Mindustry.jar")
            }
        } else if platform == PlatformHint::Server {
            base_root
                .join("server")
                .join("build")
                .join("libs")
                .join("server-release.jar")
        } else {
            base_root
                .join("desktop")
                .join("build")
                .join("libs")
                .join("Mindustry.jar")
        };

        if !path_exists(&artifact_path).await {
            return Ok(None);
        }
        let file_name = if is_x {
            if platform == PlatformHint::Server {
                "MindustryX-server-local.jar".to_string()
            } else {
                "MindustryX-desktop-local.jar".to_string()
            }
        } else if platform == PlatformHint::Server {
            "Mindustry-server-local.jar".to_string()
        } else {
            "Mindustry-desktop-local.jar".to_string()
        };
        Ok(Some((artifact_path, file_name)))
    }

    async fn find_latest_local_release_artifact(
        &self,
        choice: &str,
        request: &DownloadRequest,
    ) -> Result<Option<LocalArtifact>> {
        let Some(spec) = local_release_spec(choice) else {
            return Ok(None);
        };

        let mut candidates = self.list_local_release_candidates(&spec).await?;
        if candidates.is_empty() {
            return Ok(None);
        }

        let version_query = request
            .tag_query
            .clone()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !version_query.is_empty() && version_query != "latest" {
            candidates.retain(|item| item.file_name.to_ascii_lowercase().contains(&version_query));
            if candidates.is_empty() {
                return Ok(None);
            }
        }

        let platform_hint = request.platform_hint;
        let mut scored = candidates
            .into_iter()
            .map(|item| {
                let score =
                    score_local_release_candidate(&item, &spec, &version_query, platform_hint);
                (score, item)
            })
            .collect::<Vec<_>>();
        scored.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| right.1.mtime_ms.cmp(&left.1.mtime_ms))
                .then_with(|| left.1.file_name.cmp(&right.1.file_name))
        });

        let Some((best_score, best)) = scored.into_iter().next() else {
            return Ok(None);
        };
        if best_score <= 0 {
            return Ok(None);
        }

        Ok(Some(LocalArtifact {
            file_path: best.file_path,
            file_name: best.file_name,
            folder_name: spec.folder_name.to_string(),
        }))
    }

    async fn list_local_release_candidates(
        &self,
        spec: &LocalReleaseSpec,
    ) -> Result<Vec<LocalReleaseCandidate>> {
        let roots = self.get_local_release_search_roots(spec.choice);
        let mut seen = HashSet::<String>::new();
        let mut results = Vec::<LocalReleaseCandidate>::new();

        for root in roots {
            let files = self
                .collect_files_recursively(&root, LOCAL_SCAN_MAX_DEPTH)
                .await?;
            for file_path in files {
                let file_name = file_path
                    .file_name()
                    .map(|item| item.to_string_lossy().to_string())
                    .unwrap_or_default();
                if !matches_local_release_file(spec.choice, &file_name) {
                    continue;
                }
                let normalized_path = file_path.to_string_lossy().to_ascii_lowercase();
                if !seen.insert(normalized_path) {
                    continue;
                }
                let meta = match fs::metadata(&file_path).await {
                    Ok(item) => item,
                    Err(_) => continue,
                };
                if !meta.is_file() {
                    continue;
                }
                let mtime_ms = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis() as u64)
                    .unwrap_or_default();
                let ext = file_path
                    .extension()
                    .map(|item| format!(".{}", item.to_string_lossy().to_ascii_lowercase()))
                    .unwrap_or_default();
                results.push(LocalReleaseCandidate {
                    file_path,
                    file_name,
                    ext,
                    mtime_ms,
                });
            }
        }

        Ok(results)
    }

    fn get_local_release_search_roots(&self, choice: &str) -> Vec<PathBuf> {
        let mut roots = Vec::<PathBuf>::new();
        let local_build_root = self.local_build_root.clone();
        match normalize_text(choice).to_ascii_lowercase().as_str() {
            "local:neon" => {
                if let Some(root) = local_build_root.as_ref() {
                    roots.push(root.join("Neon"));
                    roots.push(root.join("release-assets").join("Neon"));
                }
            }
            "local:determination" => {
                if let Some(root) = local_build_root.as_ref() {
                    roots.push(root.join("DeterMination"));
                    roots.push(root.join("release-assets").join("DeterMination"));
                    if let Some(parent) = root.parent() {
                        roots.push(parent.join("anonymous"));
                    }
                }
            }
            _ => {}
        }
        roots
    }

    async fn collect_files_recursively(
        &self,
        root_path: &Path,
        max_depth: usize,
    ) -> Result<Vec<PathBuf>> {
        if !path_exists(root_path).await {
            return Ok(Vec::new());
        }

        let mut queue = VecDeque::<(PathBuf, usize)>::new();
        let mut files = Vec::<PathBuf>::new();
        queue.push_back((root_path.to_path_buf(), 0));

        while let Some((current_path, depth)) = queue.pop_front() {
            let mut entries = match fs::read_dir(&current_path).await {
                Ok(item) => item,
                Err(_) => continue,
            };
            while let Some(entry) = entries.next_entry().await? {
                let entry_path = entry.path();
                let file_type = match entry.file_type().await {
                    Ok(item) => item,
                    Err(_) => continue,
                };
                if file_type.is_dir() {
                    if depth < max_depth {
                        queue.push_back((entry_path, depth + 1));
                    }
                    continue;
                }
                if file_type.is_file() {
                    files.push(entry_path);
                }
            }
        }

        Ok(files)
    }

    async fn resolve_target_folder_name(
        &self,
        context: &EventContext,
        request_folder_name: &str,
    ) -> String {
        if !request_folder_name.trim().is_empty() {
            return request_folder_name.trim().to_string();
        }
        self.runtime_config_store
            .get_qa_group_file_download_folder_name(&context.group_id)
            .await
    }

    async fn create_git_worktree(
        &self,
        repo_root: &Path,
        full_commit: &str,
        prefix: &str,
    ) -> Result<PathBuf> {
        let temp_parent = self.download_root.join("_tmp-builds");
        fs::create_dir_all(&temp_parent)
            .await
            .with_context(|| format!("创建临时构建目录失败: {}", temp_parent.display()))?;
        let dir_name = format!(
            "{}-{}-{}",
            sanitize_path_component(prefix),
            full_commit.chars().take(12).collect::<String>(),
            current_time_ms()
        );
        let worktree_dir = temp_parent.join(dir_name);

        self.run_command_checked(
            "git",
            &vec![
                "-C".to_string(),
                repo_root.to_string_lossy().to_string(),
                "worktree".to_string(),
                "add".to_string(),
                "--detach".to_string(),
                worktree_dir.to_string_lossy().to_string(),
                full_commit.to_string(),
            ],
            None,
            SHORT_COMMAND_TIMEOUT_MS,
            "git-worktree-add",
        )
        .await
        .with_context(|| format!("创建 git worktree 失败: {}", worktree_dir.display()))?;
        Ok(worktree_dir)
    }

    async fn cleanup_build_worktree(&self, cleanup: &BuildCleanup) {
        let _ = self
            .run_command_checked(
                "git",
                &vec![
                    "-C".to_string(),
                    cleanup.repo_root.to_string_lossy().to_string(),
                    "worktree".to_string(),
                    "remove".to_string(),
                    cleanup.worktree_dir.to_string_lossy().to_string(),
                    "--force".to_string(),
                ],
                None,
                SHORT_COMMAND_TIMEOUT_MS,
                "git-worktree-remove",
            )
            .await;
        let _ = fs::remove_dir_all(&cleanup.worktree_dir).await;
    }

    async fn resolve_git_commit(&self, repo_root: &Path, commit: &str) -> Result<String> {
        let capture = self
            .run_command_checked(
                "git",
                &vec![
                    "-C".to_string(),
                    repo_root.to_string_lossy().to_string(),
                    "rev-parse".to_string(),
                    "--verify".to_string(),
                    format!("{}^{{commit}}", commit.trim()),
                ],
                None,
                SHORT_COMMAND_TIMEOUT_MS,
                "git-rev-parse",
            )
            .await?;
        let full = capture
            .stdout
            .lines()
            .next()
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        if full.is_empty() {
            bail!("无法解析 commit：{}", commit);
        }
        Ok(full)
    }

    async fn try_resolve_git_commit(
        &self,
        repo_root: &Path,
        commit: &str,
    ) -> Result<Option<String>> {
        let raw = self
            .run_command_raw(
                "git",
                &vec![
                    "-C".to_string(),
                    repo_root.to_string_lossy().to_string(),
                    "rev-parse".to_string(),
                    "--verify".to_string(),
                    format!("{}^{{commit}}", commit.trim()),
                ],
                None,
                SHORT_COMMAND_TIMEOUT_MS,
                "git-rev-parse",
            )
            .await?;
        if raw.status_code != 0 {
            return Ok(None);
        }
        let full = raw
            .stdout
            .lines()
            .next()
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        if full.is_empty() {
            return Ok(None);
        }
        Ok(Some(full))
    }

    async fn is_git_ancestor(
        &self,
        repo_root: &Path,
        ancestor: &str,
        descendant: &str,
    ) -> Result<bool> {
        let raw = self
            .run_command_raw(
                "git",
                &vec![
                    "-C".to_string(),
                    repo_root.to_string_lossy().to_string(),
                    "merge-base".to_string(),
                    "--is-ancestor".to_string(),
                    ancestor.to_string(),
                    descendant.to_string(),
                ],
                None,
                SHORT_COMMAND_TIMEOUT_MS,
                "git-merge-base",
            )
            .await?;
        if raw.status_code == 0 {
            return Ok(true);
        }
        if raw.status_code == 1 {
            return Ok(false);
        }
        bail!(
            "检查 commit 祖先关系失败：ancestor={} descendant={} stderr={}",
            ancestor.chars().take(7).collect::<String>(),
            descendant.chars().take(7).collect::<String>(),
            truncate_for_error(&raw.stderr)
        );
    }

    async fn count_git_revision_distance(
        &self,
        repo_root: &Path,
        base: &str,
        head: &str,
    ) -> Result<u64> {
        let capture = self
            .run_command_checked(
                "git",
                &vec![
                    "-C".to_string(),
                    repo_root.to_string_lossy().to_string(),
                    "rev-list".to_string(),
                    "--count".to_string(),
                    format!("{}..{}", base, head),
                ],
                None,
                SHORT_COMMAND_TIMEOUT_MS,
                "git-rev-list-count",
            )
            .await?;
        let value = capture.stdout.trim().parse::<u64>().unwrap_or(0);
        Ok(value)
    }

    async fn run_bash_script(&self, script_path: &Path, cwd: &Path, timeout_ms: u64) -> Result<()> {
        let script_rel = if script_path
            .parent()
            .map(|parent| parent.ends_with("scripts"))
            .unwrap_or(false)
        {
            "./scripts/applyPatches.sh".to_string()
        } else {
            script_path.to_string_lossy().to_string()
        };

        if self
            .run_command_checked(
                "bash",
                &vec![script_rel.clone()],
                Some(cwd),
                timeout_ms,
                "run-bash-script",
            )
            .await
            .is_ok()
        {
            return Ok(());
        }

        self.run_command_checked(
            "sh",
            &vec![script_rel],
            Some(cwd),
            timeout_ms,
            "run-sh-script",
        )
        .await?;
        Ok(())
    }

    async fn run_command_checked(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
        timeout_ms: u64,
        label: &str,
    ) -> Result<CommandCapture> {
        let capture = self
            .run_command_raw(program, args, cwd, timeout_ms, label)
            .await
            .with_context(|| format!("命令执行失败：{} {}", program, args.join(" ")))?;
        if capture.status_code == 0 {
            return Ok(capture);
        }
        bail!(
            "命令退出码 {}：{} {}\nstdout: {}\nstderr: {}",
            capture.status_code,
            program,
            args.join(" "),
            truncate_for_error(&capture.stdout),
            truncate_for_error(&capture.stderr)
        );
    }

    async fn run_command_raw(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
        timeout_ms: u64,
        label: &str,
    ) -> Result<CommandCapture> {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(path) = cwd {
            command.current_dir(path);
        }
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        self.logger
            .info(format!("执行命令({label})：{} {}", program, args.join(" ")))
            .await;

        let output = timeout(Duration::from_millis(timeout_ms.max(1)), command.output())
            .await
            .with_context(|| format!("命令执行超时({label})：{} {}", program, args.join(" ")))?
            .with_context(|| format!("命令启动失败({label})：{} {}", program, args.join(" ")))?;

        Ok(CommandCapture {
            status_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    async fn reply_reason_if_failed(
        &self,
        context: &EventContext,
        reply_message_id: &str,
        result: &Value,
    ) {
        if result.get("started").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let Some(reason) = result
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|item| !item.is_empty())
        else {
            return;
        };
        let _ = self
            .napcat_client
            .reply_text(
                "group",
                &context.group_id,
                normalize_option_str(reply_message_id),
                reason,
            )
            .await;
    }

    async fn cleanup_expired_pending_selections(&self) {
        let now = current_time_ms();
        let mut guard = self.pending_selections.lock().await;
        guard.retain(|_, pending| pending.expires_at_ms > now);
    }

    async fn store_pending_selection(&self, context: &EventContext, kind: PendingSelectionKind) {
        self.cleanup_expired_pending_selections().await;
        let now = current_time_ms();
        let key = pending_key(context);
        self.pending_selections.lock().await.insert(
            key,
            PendingSelection {
                group_id: context.group_id.clone(),
                user_id: context.user_id.clone(),
                expires_at_ms: now + PENDING_SELECTION_TTL_MS,
                kind,
            },
        );
    }

    async fn get_file_lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut guard = self.file_locks.lock().await;
        guard
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn build_github_api_headers(&self) -> Result<HeaderMap> {
        let mut headers = self.build_github_headers_with_auth(true)?;
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        Ok(headers)
    }

    fn build_github_headers_with_auth(&self, use_auth: bool) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("NapCatCainBot-Rust"));
        headers.insert(ACCEPT, HeaderValue::from_static("application/octet-stream"));
        if use_auth && !self.github_token.trim().is_empty() {
            let auth = format!("Bearer {}", self.github_token.trim());
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&auth).context("GitHub token 含非法字符")?,
            );
        }
        Ok(headers)
    }
}

fn infer_download_request(text: &str, request: Option<&Value>) -> Option<DownloadRequest> {
    let normalized_text = normalize_text(text);

    let repo_input = request
        .and_then(|item| item.get("repo_choice").or_else(|| item.get("repo")))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();

    let mut local_release_choices = Vec::<String>::new();
    local_release_choices.extend(detect_local_release_choices(&repo_input));
    local_release_choices.extend(detect_local_release_choices(&normalized_text));
    local_release_choices = unique_strings(local_release_choices);

    let repo = parse_repo_choice(&repo_input)
        .or_else(|| parse_repo_choice_from_text(&normalized_text))
        .or_else(|| detect_default_repo(&normalized_text));

    let tag_query = request
        .and_then(|item| {
            item.get("version_query")
                .or_else(|| item.get("version"))
                .or_else(|| item.get("tag"))
        })
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
        .or_else(|| parse_version_query_from_text(&normalized_text));

    let commit_hash = request
        .and_then(|item| {
            item.get("commit_hash")
                .or_else(|| item.get("commit"))
                .or_else(|| item.get("sha"))
                .or_else(|| item.get("hash"))
        })
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| is_commit_hash_like(item))
        .map(|item| item.to_ascii_lowercase())
        .or_else(|| parse_commit_hash_from_text(&normalized_text));

    let requested_mode = request
        .and_then(|item| item.get("mode").or_else(|| item.get("download_mode")))
        .and_then(Value::as_str)
        .map(|item| item.trim().to_ascii_lowercase())
        .unwrap_or_default();

    let mode = if requested_mode == "commit-build" || requested_mode == "commit" {
        DownloadMode::CommitBuild
    } else if requested_mode == "local-build"
        || requested_mode == "local"
        || requested_mode == "local-release"
    {
        DownloadMode::LocalBuild
    } else if commit_hash.is_some() || looks_like_commit_build_request(&normalized_text) {
        DownloadMode::CommitBuild
    } else if !local_release_choices.is_empty() {
        DownloadMode::LocalBuild
    } else {
        DownloadMode::Release
    };

    let exact_commit_build = request
        .and_then(|item| {
            item.get("exact_commit_build")
                .or_else(|| item.get("exactCommitBuild"))
        })
        .and_then(value_to_bool)
        .unwrap_or_else(|| wants_exact_commit_build(&normalized_text));

    let folder_name = request
        .and_then(|item| item.get("folder_name").or_else(|| item.get("folderName")))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();

    let platform_hint = request
        .and_then(|item| item.get("platform_hint").or_else(|| item.get("platform")))
        .and_then(Value::as_str)
        .map(detect_platform_hint)
        .unwrap_or_else(|| detect_platform_hint(&normalized_text));

    let has_explicit_fields = repo.is_some()
        || tag_query.is_some()
        || commit_hash.is_some()
        || !local_release_choices.is_empty()
        || !requested_mode.is_empty();
    if !has_explicit_fields {
        if normalized_text.is_empty() {
            return None;
        }
        if request.is_none()
            && !looks_like_download_request_text(&normalized_text)
            && !looks_like_commit_build_request(&normalized_text)
        {
            return None;
        }
    }

    Some(DownloadRequest {
        repo,
        mode,
        tag_query,
        commit_hash,
        exact_commit_build,
        platform_hint,
        folder_name,
        request_text: normalized_text,
        local_release_choices,
    })
}

fn parse_repo_choice(input: &str) -> Option<RepoChoice> {
    let normalized = normalize_text(input);
    if normalized.is_empty() {
        return None;
    }
    let lower = normalized.to_ascii_lowercase();
    if lower == "x"
        || lower.contains("mindustryx")
        || lower.contains("tinylake/mindustryx")
        || lower.contains("x端")
    {
        return Some(RepoChoice {
            owner: "TinyLake".to_string(),
            repo: "MindustryX".to_string(),
        });
    }
    if lower == "vanilla"
        || lower.contains("anuken/mindustry")
        || lower.contains("原版")
        || lower.contains("官版")
    {
        return Some(RepoChoice {
            owner: "Anuken".to_string(),
            repo: "Mindustry".to_string(),
        });
    }

    if normalized.contains(char::is_whitespace) {
        return None;
    }

    let trimmed = normalized.trim_matches(|ch: char| !is_repo_char(ch) && ch != '/');
    let (owner, repo) = trimmed.split_once('/')?;
    let owner = owner.trim();
    let repo = repo.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    if !owner.chars().all(is_repo_char) || !repo.chars().all(is_repo_char) {
        return None;
    }
    Some(RepoChoice {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

fn parse_repo_choice_from_text(text: &str) -> Option<RepoChoice> {
    if let Some(repo) = parse_repo_choice(text) {
        return Some(repo);
    }
    text.split_whitespace().find_map(parse_repo_choice)
}

fn detect_default_repo(text: &str) -> Option<RepoChoice> {
    let lower = text.to_lowercase();
    if lower.is_empty() {
        return None;
    }
    if lower.contains("mindustryx") || lower.contains("牡丹亭") || lower.contains("x端") {
        return Some(RepoChoice {
            owner: "TinyLake".to_string(),
            repo: "MindustryX".to_string(),
        });
    }
    if lower.contains("mindustry") || lower.contains("原版") {
        return Some(RepoChoice {
            owner: "Anuken".to_string(),
            repo: "Mindustry".to_string(),
        });
    }
    None
}

fn parse_version_query_from_text(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("最新版") || lower.contains("最新版本") || lower.contains("latest") {
        return Some("latest".to_string());
    }
    for token in text.split_whitespace() {
        let cleaned =
            token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '.' && ch != '-');
        if cleaned.is_empty() {
            continue;
        }
        let probe = cleaned.strip_prefix('v').unwrap_or(cleaned);
        let dot_count = probe.chars().filter(|ch| *ch == '.').count();
        if dot_count >= 1
            && probe
                .chars()
                .all(|ch| ch.is_ascii_digit() || ch == '.' || ch == '-' || ch.is_ascii_alphabetic())
        {
            return Some(cleaned.to_string());
        }
    }
    None
}

fn parse_commit_hash_from_text(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let has_commit_hint = lower.contains("commit")
        || lower.contains("hash")
        || lower.contains("sha")
        || lower.contains("提交")
        || lower.contains("编译")
        || lower.contains("构建")
        || lower.contains("build");

    text.split_whitespace()
        .map(|token| token.trim_matches(|ch: char| !ch.is_ascii_hexdigit()))
        .find(|token| {
            if !is_commit_hash_like(token) {
                return false;
            }
            if token.chars().all(|ch| ch.is_ascii_digit()) && !has_commit_hint {
                return false;
            }
            true
        })
        .map(|item| item.to_ascii_lowercase())
}

fn is_commit_hash_like(text: &str) -> bool {
    (7..=40).contains(&text.len()) && text.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn detect_platform_hint(text: &str) -> PlatformHint {
    let lower = text.to_lowercase();
    if lower.contains("android")
        || lower.contains("安卓")
        || lower.contains(".apk")
        || lower.contains(" apk")
    {
        return PlatformHint::Android;
    }
    if lower.contains("server")
        || lower.contains("服务端")
        || lower.contains("服务器")
        || lower.contains("headless")
    {
        return PlatformHint::Server;
    }
    if lower.contains("pc")
        || lower.contains("电脑")
        || lower.contains("桌面")
        || lower.contains("windows")
        || lower.contains("desktop")
    {
        return PlatformHint::Pc;
    }
    PlatformHint::Unknown
}

fn detect_local_release_choices(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let mut choices = Vec::<String>::new();
    if lower.contains("neon") || text.contains('氖') {
        choices.push("local:neon".to_string());
    }
    if lower.contains("determination") || text.contains("决心") {
        choices.push("local:determination".to_string());
    }
    unique_strings(choices)
}

fn local_release_spec(choice: &str) -> Option<LocalReleaseSpec> {
    match normalize_text(choice).to_ascii_lowercase().as_str() {
        "local:neon" => Some(LocalReleaseSpec {
            choice: "local:neon",
            display_name: "Neon",
            folder_name: "Neon",
        }),
        "local:determination" => Some(LocalReleaseSpec {
            choice: "local:determination",
            display_name: "DeterMination 服务器插件",
            folder_name: "DeterMination",
        }),
        _ => None,
    }
}

fn local_release_display_name(choice: &str) -> &'static str {
    local_release_spec(choice)
        .map(|item| item.display_name)
        .unwrap_or("未知本地发布")
}

fn matches_local_release_file(choice: &str, file_name: &str) -> bool {
    let lower_name = file_name.to_ascii_lowercase();
    match normalize_text(choice).to_ascii_lowercase().as_str() {
        "local:neon" => {
            (lower_name.starts_with("neon-") || lower_name.starts_with("neon_"))
                && (lower_name.ends_with(".zip") || lower_name.ends_with(".jar"))
        }
        "local:determination" => {
            lower_name.starts_with("determination-modules") && lower_name.ends_with(".zip")
        }
        _ => false,
    }
}

fn score_local_release_candidate(
    candidate: &LocalReleaseCandidate,
    spec: &LocalReleaseSpec,
    version_query: &str,
    platform_hint: PlatformHint,
) -> i64 {
    let lower_name = candidate.file_name.to_ascii_lowercase();
    let mut score = 100i64;

    if !version_query.is_empty() && version_query != "latest" {
        if lower_name.contains(version_query) {
            score += 200;
        } else {
            score -= 120;
        }
    }

    match platform_hint {
        PlatformHint::Android => {
            if candidate.ext == ".jar" {
                score += 20;
            } else {
                score -= 10;
            }
        }
        PlatformHint::Pc => {
            if candidate.ext == ".zip" {
                score += 20;
            } else {
                score += 10;
            }
        }
        PlatformHint::Server => {
            if candidate.ext == ".zip" || candidate.ext == ".jar" {
                score += 20;
            }
        }
        PlatformHint::Unknown => {}
    }

    let preferred_exts = if spec.choice == "local:neon" {
        vec![".zip", ".jar"]
    } else {
        vec![".zip"]
    };
    if let Some(index) = preferred_exts
        .iter()
        .position(|item| *item == candidate.ext.to_ascii_lowercase())
    {
        score += ((preferred_exts.len() - index) as i64) * 10;
    }

    if spec.choice == "local:determination" && lower_name == "determination-modules.zip" {
        score += 25;
    }

    let fresh_bonus = ((candidate.mtime_ms / (60 * 1000)).min(30)) as i64;
    score += fresh_bonus;
    score
}

fn looks_like_commit_build_request(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let has_commit = parse_commit_hash_from_text(text).is_some();
    let build_like = lower.contains("commit")
        || lower.contains("hash")
        || lower.contains("提交")
        || lower.contains("编译")
        || lower.contains("构建")
        || lower.contains("build")
        || lower.contains("源码");
    if has_commit && build_like {
        return true;
    }
    let game_like = lower.contains("mindustryx")
        || lower.contains("mindustry")
        || lower.contains("mdt")
        || lower.contains("x端")
        || lower.contains("原版");
    let artifact_like = lower.contains("jar")
        || lower.contains("apk")
        || lower.contains("服务端")
        || lower.contains("server")
        || lower.contains("desktop")
        || lower.contains("pc")
        || lower.contains("安卓");
    game_like && build_like && artifact_like
}

fn wants_exact_commit_build(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("一定要")
        || lower.contains("精确编译")
        || lower.contains("本地编译")
        || lower.contains("exact")
        || lower.contains("这个commit")
        || lower.contains("该commit")
}

fn looks_like_download_intent(text: &str) -> bool {
    looks_like_download_request_text(text) || looks_like_commit_build_request(text)
}

fn looks_like_download_request_text(text: &str) -> bool {
    let normalized = normalize_text(text);
    if normalized.is_empty() {
        return false;
    }
    let lower = normalized.to_ascii_lowercase();
    let local_release_choices = detect_local_release_choices(&normalized);
    let has_repo = parse_repo_choice_from_text(&normalized).is_some();

    let release_keywords = [
        "release",
        "releases",
        "最新版",
        "最新版本",
        "latest",
        "安装包",
        "客户端下载",
        "下载包",
        "文件",
        "资产",
        "asset",
        "apk",
        "exe",
        "zip",
        "jar",
        "插件包",
        "服务端插件",
    ];
    if has_repo && contains_any(&lower, &release_keywords) {
        return true;
    }

    let install_like = [
        "安装包",
        "安装文件",
        "客户端",
        "下载包",
        "apk",
        "exe",
        "jar",
        "zip",
        "桌面版",
        "电脑版",
        "版本包",
        "插件",
        "服务器插件",
        "服务端插件",
    ];
    let plugin_like = [
        "sa插件",
        "sa plugin",
        "scriptagent",
        "script agent",
        "scriptagent4mindustryext",
        "neon",
        "determination",
        "服务器插件",
        "服务端插件",
    ];
    let request_like = [
        "有没有",
        "有吗",
        "求",
        "求发",
        "发一下",
        "发个",
        "给我",
        "来个",
        "想要",
        "下载",
        "整一个",
        "能发",
        "有无",
        "谁有",
        "发我",
        "来一份",
        "哪个包",
        "哪一个",
    ];
    let platform_like = ["电脑", "pc", "桌面", "安卓", "android", "server", "服务端"];
    let version_like = parse_version_query_from_text(&normalized).is_some();
    let game_like = looks_like_game_mention(&lower);

    if contains_any(&lower, &install_like) && (contains_any(&lower, &request_like) || version_like)
    {
        return true;
    }
    if (contains_any(&lower, &plugin_like) || !local_release_choices.is_empty())
        && (contains_any(&lower, &request_like) || version_like)
    {
        return true;
    }
    if contains_any(&lower, &request_like) && version_like && game_like {
        return true;
    }
    if contains_any(&lower, &request_like) && game_like && contains_any(&lower, &platform_like) {
        return true;
    }
    false
}

fn looks_like_game_mention(lower_text: &str) -> bool {
    ["mindustryx", "mindustry", "mdt", "牡丹亭", "x端", "原版"]
        .iter()
        .any(|needle| lower_text.contains(needle))
}

fn contains_any(text: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|item| text.contains(item))
}

fn score_release(release: &GithubRelease, query: &str) -> i64 {
    let normalized_query = query.trim().to_ascii_lowercase();
    if normalized_query.is_empty() {
        return 0;
    }
    let tag = release.tag_name.trim().to_ascii_lowercase();
    let name = release.name.trim().to_ascii_lowercase();
    let query_no_v = normalized_query.trim_start_matches('v');
    let tag_no_v = tag.trim_start_matches('v');

    let mut score = 0i64;
    if !tag.is_empty() && tag == normalized_query {
        score += 260;
    }
    if !tag_no_v.is_empty() && tag_no_v == query_no_v {
        score += 220;
    }
    if !tag.is_empty() && tag.contains(&normalized_query) {
        score += 120;
    }
    if !name.is_empty() && name.contains(&normalized_query) {
        score += 80;
    }
    if !tag.is_empty() && tag.starts_with(&normalized_query) {
        score += 40;
    }
    if !tag_no_v.is_empty() && tag_no_v.starts_with(query_no_v) {
        score += 30;
    }
    if release.prerelease && !normalized_query.contains("pre") && !normalized_query.contains("rc") {
        score -= 20;
    }
    score
}

fn score_asset(asset: &GithubAsset, platform_hint: PlatformHint, request_text: &str) -> i64 {
    let lower = asset.name.to_ascii_lowercase();
    let mut score = 0i64;
    if is_checksum_or_meta_file(&lower) {
        score -= 100;
    }
    if lower.ends_with(".apk") {
        score += 30;
    }
    if lower.ends_with(".jar") || lower.ends_with(".zip") || lower.ends_with(".exe") {
        score += 10;
    }
    match platform_hint {
        PlatformHint::Android => {
            if lower.contains("android") || lower.ends_with(".apk") {
                score += 90;
            }
        }
        PlatformHint::Server => {
            if lower.contains("server") || lower.contains("headless") {
                score += 90;
            }
        }
        PlatformHint::Pc => {
            if lower.contains("desktop") || lower.contains("windows") || lower.contains("pc") {
                score += 70;
            }
        }
        PlatformHint::Unknown => {}
    }
    for token in request_text.split_whitespace() {
        let normalized = token
            .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '.' && ch != '-')
            .to_ascii_lowercase();
        if normalized.len() >= 3 && lower.contains(&normalized) {
            score += 6;
        }
    }
    score
}

fn is_checksum_or_meta_file(lower_name: &str) -> bool {
    lower_name.ends_with(".sha256")
        || lower_name.ends_with(".sha1")
        || lower_name.ends_with(".md5")
        || lower_name.ends_with(".txt")
        || lower_name.ends_with(".json")
        || lower_name.ends_with(".yml")
        || lower_name.ends_with(".yaml")
        || lower_name.contains("checksum")
}

fn release_display_name(release: &GithubRelease) -> String {
    if !release.tag_name.trim().is_empty() {
        release.tag_name.trim().to_string()
    } else if !release.name.trim().is_empty() {
        release.name.trim().to_string()
    } else {
        "(无 tag)".to_string()
    }
}

fn release_sort_value(release: &GithubRelease) -> String {
    let published = normalize_text(&release.published_at);
    if !published.is_empty() {
        return published;
    }
    let created = normalize_text(&release.created_at);
    if !created.is_empty() {
        return created;
    }
    release_display_name(release)
}

fn parse_selection_index(text: &str) -> Option<usize> {
    let normalized = text.trim();
    if normalized.is_empty() {
        return None;
    }
    normalized.parse::<usize>().ok()
}

fn pending_key(context: &EventContext) -> String {
    format!("{}:{}", context.group_id.trim(), context.user_id.trim())
}

fn is_cancel_text(text: &str) -> bool {
    matches!(
        normalize_text(text).to_ascii_lowercase().as_str(),
        "取消" | "算了" | "cancel" | "c" | "stop" | "结束"
    )
}

fn normalize_option_str(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn value_to_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(item) => Some(*item),
        Value::Number(item) => Some(item.as_i64().unwrap_or_default() != 0),
        Value::String(item) => {
            let lower = item.trim().to_ascii_lowercase();
            if lower.is_empty() {
                None
            } else if matches!(lower.as_str(), "true" | "1" | "yes" | "y" | "是") {
                Some(true)
            } else if matches!(lower.as_str(), "false" | "0" | "no" | "n" | "否") {
                Some(false)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

fn sanitize_path_component(input: &str) -> String {
    let normalized = input.trim();
    let replaced = normalized
        .chars()
        .map(|ch| {
            if matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>();
    if replaced.is_empty() {
        "unknown".to_string()
    } else {
        replaced
    }
}

fn is_repo_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.'
}

fn unique_strings(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::<String>::new();
    let mut result = Vec::<String>::new();
    for item in values {
        let normalized = normalize_text(&item);
        if normalized.is_empty() {
            continue;
        }
        let key = normalized.to_ascii_lowercase();
        if seen.insert(key) {
            result.push(normalized);
        }
    }
    result
}

fn truncate_for_error(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "(empty)".to_string();
    }
    if trimmed.chars().count() <= 600 {
        return trimmed.to_string();
    }
    format!(
        "{}...(truncated)",
        trimmed.chars().take(600).collect::<String>()
    )
}

fn find_gradle_wrapper(root: &Path) -> Option<PathBuf> {
    let win = root.join("gradlew.bat");
    if win.exists() {
        return Some(win);
    }
    let unix = root.join("gradlew");
    if unix.exists() {
        return Some(unix);
    }
    None
}

async fn path_exists(path: &Path) -> bool {
    fs::metadata(path).await.is_ok()
}

fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}
