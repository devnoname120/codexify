//! Local-only owner view of the persisted agent chats. This is deliberately not
//! nested under `/mcp` and never shares the connector's tunnel listener.

mod analytics;
mod attachments;
mod creation;
mod pwa;
mod schema_refresh;
pub(crate) use schema_refresh::start as start_schema_refresh;
mod readiness;
mod settings;
mod wake;

use std::io::{Read, Write};
use std::path::{Path as FilePath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::agent_automation::{
    AgentAutomationStore, AgentAutomationSupervisor, AutomationTaskHandle, BrowserCompanionClient,
};
use crate::artifact_egress::ArtifactEgressStore;
use crate::conversation_continuations::ConversationContinuationStore;
use crate::markdown_chat::{MarkdownChatStore, OwnerChatSummary};
use crate::markdown_chat_ui::{CHAT_UI_HTML, CHAT_WIDGET_META};
use crate::project_bindings::ConversationIdentity;
use crate::types::AppConfig;
use crate::util::home_dir;

type ApiResult = Result<Json<Value>, (StatusCode, String)>;

fn require_connector_name(config: &AppConfig) -> Result<String, (StatusCode, String)> {
    crate::global_settings::connector_name(config.loaded_config_path.as_deref(),
        &config.markdown_chat.automation.connector_name).map_err(internal_error)?
        .ok_or_else(|| (StatusCode::PRECONDITION_REQUIRED,
            "Configure the exact ChatGPT connector name before using goals or creating conversations.".into()))
}
type BrowserOpener = Arc<dyn Fn(&str) -> std::io::Result<()> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnerMessage {
    Chat(String),
    Goal(crate::agent_automation::OwnerGoalCommand),
}

fn parse_owner_message(message: &str) -> Result<OwnerMessage, String> {
    if message == "//goal" || message.starts_with("//goal ") {
        return Ok(OwnerMessage::Chat(message[1..].to_string()));
    }
    if message != "/goal" && !message.starts_with("/goal ") {
        return Ok(OwnerMessage::Chat(message.to_string()));
    }
    if let Some(markdown) = message.strip_prefix("/goal set ") {
        if markdown.trim().is_empty() {
            return Err("Goal Markdown must not be empty".into());
        }
        if markdown.len() > crate::agent_automation::MAX_GOAL_BYTES {
            return Err("Goal Markdown must not exceed 10 KiB of UTF-8".into());
        }
        return Ok(OwnerMessage::Goal(
            crate::agent_automation::OwnerGoalCommand::Set {
                markdown: markdown.to_string(),
            },
        ));
    }
    let command = match message {
        "/goal run" => crate::agent_automation::OwnerGoalCommand::Run,
        "/goal stop" => crate::agent_automation::OwnerGoalCommand::Stop,
        "/goal clear" => crate::agent_automation::OwnerGoalCommand::Clear,
        "/goal status" => crate::agent_automation::OwnerGoalCommand::Status,
        "/goal set" => return Err("Goal Markdown must not be empty".into()),
        _ => return Err("Unknown /goal command".into()),
    };
    Ok(OwnerMessage::Goal(command))
}

#[derive(Clone)]
struct OwnerState {
    config: Arc<AppConfig>,
    chats: Arc<MarkdownChatStore>,
    project_bindings: Arc<crate::project_bindings::ProjectBindingStore>,
    creation_lock: Arc<tokio::sync::Mutex<()>>,
    artifacts: Arc<ArtifactEgressStore>,
    continuations: Arc<ConversationContinuationStore>,
    automation: Option<Arc<AgentAutomationSupervisor>>,
    open_browser: BrowserOpener,
    catalog_cache: Arc<tokio::sync::Mutex<OwnerCatalogRuntime>>,
    global: Arc<tokio::sync::Mutex<settings::GlobalRuntime>>,
    token: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerCatalogCache {
    key: String,
    catalog: crate::agent_automation::CapabilityCatalog,
}

const MODEL_CATALOG_REFRESH_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Default)]
struct OwnerCatalogRuntime {
    entry: Option<OwnerCatalogCache>,
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
    refresh_key: Option<String>,
    refresh_after_ms: u64,
}

impl OwnerCatalogRuntime {
    fn remember(&mut self, entry: OwnerCatalogCache, now_ms: u64) {
        self.refresh_key = Some(entry.key.clone());
        self.refresh_after_ms = now_ms.saturating_add(MODEL_CATALOG_REFRESH_MS);
        self.entry = Some(entry);
    }
}

fn catalog_context(health: &crate::agent_automation::CompanionHealth) -> String {
    catalog_key(&health.build, health.account_fingerprint.as_deref())
}

fn catalog_key(_build: &str, account: Option<&str>) -> String {
    // This cache contains model metadata, not executable browser requests.
    // Validate its schema separately from the companion's changing protocol hash.
    hashed_catalog_key("browser-model-catalog-v1", account)
}

fn hashed_catalog_key(context: &str, account: Option<&str>) -> String {
    let mut digest = Sha256::new();
    digest.update(context);
    digest.update([0]);
    digest.update(account.unwrap_or(""));
    format!("{:x}", digest.finalize())
}

fn read_catalog_cache(state: &OwnerState, key: &str) -> Option<OwnerCatalogCache> {
    let path =
        BrowserCompanionClient::catalog_cache_path(&state.config.markdown_chat.automation).ok()?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return None;
        }
    }
    let mut entry: OwnerCatalogCache = serde_json::from_reader(file.take(1024 * 1024 + 1)).ok()?;
    let account = Some(entry.catalog.account_fingerprint.as_str());
    let current_key = catalog_key(&entry.catalog.build, account);
    let protocol_key = hashed_catalog_key(
        entry
            .catalog
            .build
            .split_once("+protocol.")
            .map_or(entry.catalog.build.as_str(), |(_, schema)| schema),
        account,
    );
    let legacy_key = hashed_catalog_key(&entry.catalog.build, account);
    if current_key != key
        || (entry.key != current_key && entry.key != protocol_key && entry.key != legacy_key)
        || BrowserCompanionClient::validate_cached_catalog(&entry.catalog).is_err()
    {
        return None;
    }
    entry.key = current_key;
    Some(entry)
}

fn write_catalog_cache(state: &OwnerState, entry: &OwnerCatalogCache) -> anyhow::Result<()> {
    let path = BrowserCompanionClient::catalog_cache_path(&state.config.markdown_chat.automation)
        .map_err(anyhow::Error::msg)?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(path.parent().context("catalog cache has no parent")?)?;
    serde_json::to_writer(&mut temporary, entry)?;
    temporary.flush()?;
    temporary.persist(path)?;
    Ok(())
}

async fn cached_owner_catalog(
    state: &OwnerState,
    key: &str,
) -> Option<crate::agent_automation::CapabilityCatalog> {
    let mut cached = state.catalog_cache.lock().await;
    if cached.entry.as_ref().is_none_or(|entry| entry.key != key) {
        cached.entry = read_catalog_cache(state, key);
    }
    cached
        .entry
        .as_ref()
        .filter(|entry| entry.key == key)
        .map(|entry| entry.catalog.clone())
}

async fn owner_catalog(
    state: &OwnerState,
    force: bool,
) -> Result<
    (
        crate::agent_automation::CompanionHealth,
        crate::agent_automation::CapabilityCatalog,
    ),
    (StatusCode, String),
> {
    let client = companion_client(state)?;
    let cancellation = CancellationToken::new();
    if !force {
        let health = client.health(&cancellation).await.map_err(internal_error)?;
        if !health.authenticated {
            return Err(internal_error("ChatGPT authentication is required"));
        }
        if let Some(catalog) = cached_owner_catalog(state, &catalog_context(&health)).await {
            return Ok((health, catalog));
        }
    }
    let refresh_lock = state.catalog_cache.lock().await.refresh_lock.clone();
    let _refresh = refresh_lock.lock().await;
    // Another refresh may have filled the cache while this request waited.
    // Verify the current account again before reusing its result.
    let health = client.health(&cancellation).await.map_err(internal_error)?;
    if !health.authenticated {
        return Err(internal_error("ChatGPT authentication is required"));
    }
    let key = catalog_context(&health);
    if !force && let Some(catalog) = cached_owner_catalog(state, &key).await {
        return Ok((health, catalog));
    }
    if health.status != crate::agent_automation::CompanionSessionStatus::Idle
        || health.active_action_id.is_some()
    {
        return Err(internal_error(
            "Finish the current browser action before refreshing models",
        ));
    }
    let catalog = client
        .capabilities(&cancellation)
        .await
        .map_err(internal_error)?;
    if health.account_fingerprint.as_deref() != Some(catalog.account_fingerprint.as_str()) {
        return Err(internal_error(
            "Browser companion account changed during capability discovery",
        ));
    }
    let entry = OwnerCatalogCache {
        key,
        catalog: catalog.clone(),
    };
    if write_catalog_cache(state, &entry).is_err() {
        tracing::warn!("could not persist the browser model catalog cache");
    }
    state
        .catalog_cache
        .lock()
        .await
        .remember(entry, crate::markdown_chat::now_ms());
    Ok((health, catalog))
}

async fn refresh_owner_catalog_if_due(
    state: &OwnerState,
    health: &crate::agent_automation::CompanionHealth,
    now_ms: u64,
) -> bool {
    if !state.config.markdown_chat.automation.enabled
        || !health.authenticated
        || health.status != crate::agent_automation::CompanionSessionStatus::Idle
        || health.active_action_id.is_some()
    {
        return false;
    }
    let key = catalog_context(health);
    {
        let mut runtime = state.catalog_cache.lock().await;
        if runtime.refresh_key.as_ref() != Some(&key) {
            runtime.refresh_key = Some(key);
            runtime.refresh_after_ms = 0;
        }
        if now_ms < runtime.refresh_after_ms {
            return false;
        }
        // One attempt on startup and then daily. Failed discovery never erases
        // the last successful list or starts a refresh loop from chat polling.
        runtime.refresh_after_ms = now_ms.saturating_add(MODEL_CATALOG_REFRESH_MS);
    }
    if let Err((_, error)) = owner_catalog(state, true).await {
        if error == "Finish the current browser action before refreshing models"
            || error == "Browser companion returned HTTP status 423"
        {
            state.catalog_cache.lock().await.refresh_after_ms = 0;
            return false;
        }
        tracing::warn!(%error, "model catalog refresh failed; retaining the cached model list");
    }
    let mut runtime = state.catalog_cache.lock().await;
    runtime.refresh_after_ms = runtime
        .refresh_after_ms
        .max(now_ms.saturating_add(MODEL_CATALOG_REFRESH_MS));
    true
}

#[derive(Serialize, Deserialize)]
struct RuntimeInfo {
    port: u16,
    token: String,
}

pub struct OwnerServer {
    task: tokio::task::JoinHandle<()>,
    maintenance: tokio::task::JoinHandle<()>,
}

impl Drop for OwnerServer {
    fn drop(&mut self) {
        self.task.abort();
        self.maintenance.abort();
    }
}

fn runtime_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codexify")
        .join("owner-chat.json")
}

fn read_runtime(path: &FilePath) -> anyhow::Result<Option<RuntimeInfo>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect saved owner-chat credentials"),
    };
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Owner-chat credentials must be a regular non-symlink file: {}",
        path.display()
    );
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .context("open saved owner-chat credentials")?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Owner-chat credentials must be a regular non-symlink file: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "Owner-chat credentials must belong to the current user and have mode 0600: {}",
            path.display()
        );
    }
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 4096,
        "Saved owner-chat credentials are too large"
    );
    let info: RuntimeInfo = serde_json::from_slice(&bytes).map_err(|_| {
        anyhow::anyhow!("Invalid saved owner-chat credentials in {}", path.display())
    })?;
    anyhow::ensure!(
        info.port != 0
            && info
                .token
                .strip_prefix("codexify_")
                .and_then(|token| URL_SAFE_NO_PAD.decode(token).ok())
                .is_some_and(|bytes| bytes.len() == 32),
        "Invalid saved owner-chat credentials in {}",
        path.display()
    );
    Ok(Some(info))
}

fn prepare_runtime(
    parent: &FilePath,
    info: &RuntimeInfo,
) -> anyhow::Result<tempfile::NamedTempFile> {
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, info)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    Ok(temporary)
}

fn load_or_create_runtime(path: &FilePath, port: u16) -> anyhow::Result<RuntimeInfo> {
    let parent = path
        .parent()
        .context("owner-chat credentials have no parent directory")?;
    std::fs::create_dir_all(parent)?;
    let mut info = match read_runtime(path)? {
        Some(info) => info,
        None => {
            let candidate = RuntimeInfo {
                port,
                token: crate::auth::generate_internal_bearer_token()?,
            };
            // Concurrent first starts must adopt the same credential, not overwrite it.
            match prepare_runtime(parent, &candidate)?.persist_noclobber(path) {
                Ok(_) => candidate,
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    read_runtime(path)?
                        .context("owner-chat credentials disappeared during initialization")?
                }
                Err(error) => return Err(error.into()),
            }
        }
    };
    if info.port != port {
        info.port = port;
        prepare_runtime(parent, &info)?.persist(path)?;
    }
    Ok(info)
}

fn is_real_dir(path: &FilePath) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
}

fn is_real_file(path: &FilePath) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
}

fn stable_owner(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

struct LocatedChat {
    id: String,
    owner: String,
    path: PathBuf,
    state_root: PathBuf,
    work_dir: Option<PathBuf>,
    workspace: String,
    is_scratch: bool,
}

#[derive(Clone, Serialize)]
struct ProjectGroup {
    path: String,
    name: String,
}

fn project_group(
    config: &AppConfig,
    workspace: &FilePath,
    name: &str,
    is_scratch: bool,
) -> ProjectGroup {
    let source = if is_scratch {
        None
    } else {
        crate::worktrees::worktree_source_project(config, &workspace.to_string_lossy())
            .ok()
            .flatten()
    };
    let root = source
        .or_else(|| std::fs::canonicalize(workspace).ok())
        .unwrap_or_else(|| workspace.to_owned());
    ProjectGroup {
        path: root.to_string_lossy().into_owned(),
        name: if is_scratch {
            name.to_owned()
        } else {
            root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| name.to_owned())
        },
    }
}

fn state_roots(config: &AppConfig) -> Vec<PathBuf> {
    let base = config
        .memory
        .dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".codexify/projects")
        });
    if !is_real_dir(&base) {
        return Vec::new();
    }
    let mut roots: Vec<_> = std::fs::read_dir(&base)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_real_dir(path))
        .collect();
    if is_real_dir(&base.join("chats")) {
        roots.push(base);
    }
    roots
}

fn work_dir_for_root(config: &AppConfig, root: &FilePath) -> Option<PathBuf> {
    let path = root.join("memory.json");
    if !is_real_file(&path) {
        // Preserve visibility of legacy flat state directories without retaining
        // the old single-project runtime mode or moving user data.
        return config
            .memory
            .dir
            .as_ref()
            .filter(|dir| FilePath::new(dir) == root && is_real_dir(&root.join("chats")))
            .map(|_| config.work_dir.clone());
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > 1024 * 1024 {
        return None;
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get("workDir")?.as_str().map(PathBuf::from)
}

fn locate(
    config: &AppConfig,
    bindings: &crate::project_bindings::ProjectBindingStore,
) -> Vec<LocatedChat> {
    let mut found = Vec::new();
    let active_roots = bindings.owner_chat_work_dirs(config);
    for root in state_roots(config) {
        let chats_dir = root.join("chats");
        if !is_real_dir(&chats_dir) {
            continue;
        }
        let work_dir = work_dir_for_root(config, &root);
        for entry in std::fs::read_dir(chats_dir).into_iter().flatten().flatten() {
            let owner = entry.file_name().to_string_lossy().into_owned();
            let dir = entry.path();
            if !stable_owner(&owner) || !is_real_dir(&dir) {
                continue;
            }
            let path = dir.join("CHAT.md");
            if !is_real_file(&path) {
                continue;
            }
            let path = if path.is_absolute() {
                path
            } else if let Ok(cwd) = std::env::current_dir() {
                cwd.join(path)
            } else {
                continue;
            };
            let mut hasher = Sha256::new();
            hasher.update(path.to_string_lossy().as_bytes());
            let active_work_dir = active_roots
                .get(&owner)
                .cloned()
                .or_else(|| work_dir.clone());
            let is_scratch = active_work_dir
                .as_ref()
                .is_some_and(|path| bindings.is_scratch_root(path));
            let workspace = if is_scratch {
                "Scratch conversation".into()
            } else {
                active_work_dir
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .unwrap_or_else(|| root.file_name().unwrap_or_default())
                    .to_string_lossy()
                    .into_owned()
            };
            found.push(LocatedChat {
                id: format!("{:x}", hasher.finalize()),
                owner,
                path,
                state_root: root.clone(),
                work_dir: active_work_dir,
                workspace,
                is_scratch,
            });
        }
    }
    found
}

fn selected(state: &OwnerState, id: &str) -> Result<LocatedChat, (StatusCode, String)> {
    if !stable_owner(id) {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".into()));
    }
    locate(&state.config, &state.project_bindings)
        .into_iter()
        .find(|chat| chat.id == id)
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".into()))
}

fn internal_error(error: impl ToString) -> (StatusCode, String) {
    (StatusCode::CONFLICT, error.to_string())
}

fn wrap(value: Value) -> Json<Value> {
    Json(json!({"_meta": { CHAT_WIDGET_META: value }}))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatListing {
    id: String,
    title: String,
    custom_title: Option<String>,
    pinned_at_ms: Option<u64>,
    archived_at_ms: Option<u64>,
    owner_read_through: u64,
    workspace: String,
    project_id: String,
    project_path: String,
    project_group: ProjectGroup,
    is_scratch: bool,
    last_entry_end: u64,
    last_incoming_entry_end: u64,
    last_entry_at_ms: u64,
    last_agent_call_at_ms: Option<u64>,
    agent_waiting_until_ms: Option<u64>,
    agent_idle_until_ms: Option<u64>,
    agent_presence_until_ms: Option<u64>,
    goal_status: Option<crate::agent_automation::GoalStatus>,
    goal: Option<crate::agent_automation::GoalRecord>,
    goal_completion_dismissed: bool,
    goal_completed_at_ms: Option<u64>,
    goal_completed_chat_end: Option<u64>,
    automation_error: Option<String>,
    automation_retrying: bool,
    automation_waiting_for_browser: bool,
    automation_wake: Option<wake::WakeStatus>,
    total_tool_calls: u64,
}

#[derive(Default, Deserialize)]
struct ListQuery {
    q: Option<String>,
}

async fn transcript_matches(
    chats: &[LocatedChat],
    query: &str,
) -> Result<std::collections::HashSet<String>, String> {
    let mut matches = std::collections::HashSet::new();
    // Explicit paths keep the search confined to persisted chats. Literal matching
    // avoids treating user input as flags or a regular expression.
    for batch in chats.chunks(100) {
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new("rg")
                .args([
                    "--no-config",
                    "--text",
                    "--fixed-strings",
                    "--ignore-case",
                    "--files-with-matches",
                    "--null",
                    "--",
                    query,
                ])
                .args(batch.iter().map(|chat| &chat.path))
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| "Chat search timed out")?;
        match output {
            Ok(output) if matches!(output.status.code(), Some(0 | 1)) => {
                for path in output
                    .stdout
                    .split(|byte| *byte == 0)
                    .filter(|path| !path.is_empty())
                {
                    if let Some(chat) = batch
                        .iter()
                        .find(|chat| chat.path.to_string_lossy().as_bytes() == path)
                    {
                        matches.insert(chat.id.clone());
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // The standalone UI also works on installations without an rg binary.
                let paths: Vec<_> = batch
                    .iter()
                    .map(|chat| (chat.id.clone(), chat.path.clone()))
                    .collect();
                let pattern = regex::RegexBuilder::new(&regex::escape(query))
                    .case_insensitive(true)
                    .build()
                    .map_err(|_| "Invalid chat search")?;
                let found = tokio::task::spawn_blocking(move || {
                    use std::io::BufRead;
                    let mut found = Vec::new();
                    for (id, path) in paths {
                        let file =
                            std::fs::File::open(path).map_err(|_| "Chat transcript unavailable")?;
                        for line in std::io::BufReader::new(file).lines() {
                            if pattern.is_match(&line.map_err(|_| "Chat transcript unavailable")?) {
                                found.push(id);
                                break;
                            }
                        }
                    }
                    Ok::<_, String>(found)
                })
                .await
                .map_err(|_| "Chat search was interrupted")??;
                matches.extend(found);
            }
            _ => return Err("Chat transcripts could not be searched".into()),
        }
    }
    Ok(matches)
}

async fn list_chats(State(state): State<OwnerState>, Query(query): Query<ListQuery>) -> ApiResult {
    let query = query.q.unwrap_or_default().trim().to_string();
    if query.len() > 1024 || query.contains(['\n', '\r', '\0']) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Search must be a single line of at most 1 KiB".into(),
        ));
    }
    let located_chats = locate(&state.config, &state.project_bindings);
    let mut matched = if query.is_empty() {
        None
    } else {
        Some(
            transcript_matches(&located_chats, &query)
                .await
                .map_err(internal_error)?,
        )
    };
    let mut list = Vec::new();
    let mut project_groups = std::collections::HashMap::new();
    for located in located_chats {
        let chat = state
            .chats
            .chat_at_path(located.path.clone(), true)
            .map_err(internal_error)?;
        let summary = match chat.owner_summary().await {
            Ok(summary) => summary,
            Err(error) => {
                tracing::warn!("owner chat transcript unavailable: {error}");
                let metadata = std::fs::metadata(&located.path).map_err(internal_error)?;
                OwnerChatSummary {
                    title: "Conversation unavailable".into(),
                    custom_title: None,
                    pinned_at_ms: None,
                    archived_at_ms: None,
                    owner_read_through: 0,
                    last_entry_end: metadata.len(),
                    last_incoming_entry_end: 0,
                    last_entry_at_ms: metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| duration.as_millis() as u64)
                        .unwrap_or_default(),
                    last_agent_call_at_ms: None,
                    last_user_entry_end: 0,
                    last_user_entry_at_ms: None,
                    agent_stop_requested: false,
                    agent_waiting_until_ms: None,
                    agent_idle_until_ms: None,
                    agent_presence_until_ms: None,
                    total_tool_calls: 0,
                }
            }
        };
        let wake_activity = wake::Activity::from(&summary)
            .with_offline_timeout(state.config.markdown_chat.offline_after_ms());
        let OwnerChatSummary {
            title,
            custom_title,
            pinned_at_ms,
            archived_at_ms,
            owner_read_through,
            last_entry_end,
            last_incoming_entry_end,
            last_entry_at_ms,
            last_user_entry_end,
            last_user_entry_at_ms,
            last_agent_call_at_ms,
            agent_waiting_until_ms,
            agent_idle_until_ms,
            agent_presence_until_ms,
            total_tool_calls,
            ..
        } = summary;
        if title.to_lowercase().contains(&query.to_lowercase())
            && let Some(matched) = matched.as_mut()
        {
            matched.insert(located.id.clone());
        }
        let workspace_path = located.work_dir.as_ref().unwrap_or(&located.state_root);
        let project_group = project_groups
            .entry(workspace_path.clone())
            .or_insert_with(|| {
                project_group(
                    &state.config,
                    workspace_path,
                    &located.workspace,
                    located.is_scratch,
                )
            })
            .clone();
        let project_path = workspace_path.to_string_lossy().into_owned();
        let project_id = format!("{:x}", Sha256::digest(project_path.as_bytes()));
        // Read persisted goal/action state without registering browser work.
        let goal_task = automation_task(&located)?;
        let goal_config = state.config.clone();
        let goal_root = located.state_root.clone();
        let automation = tokio::task::spawn_blocking(move || {
            AgentAutomationStore::for_state_root(&goal_config, &goal_root)?.load(&goal_task)
        })
        .await;
        let (automation, state_error) = match automation {
            Ok(Ok(automation)) => (automation, None),
            Ok(Err(error)) => {
                tracing::warn!(chat_id=%located.id, %error, "owner chat automation state unavailable");
                (Default::default(), Some(error))
            }
            Err(error) => {
                tracing::warn!(chat_id=%located.id, %error, "owner chat automation state task failed");
                (
                    Default::default(),
                    Some("Automation state is unavailable; saved data was preserved".into()),
                )
            }
        };
        let automation_retrying = state_error.is_none() && automation.is_automatically_retrying();
        let automation_error = state_error.or_else(|| {
            automation
                .action_needs_attention_at(crate::markdown_chat::now_ms())
                .then(|| {
                    automation
                        .action
                        .last_error
                        .clone()
                        .unwrap_or_else(|| "Browser action needs attention".into())
                })
        });
        let automation_waiting_for_browser =
            automation_error.is_none() && automation.browser_cooldown_until_ms().is_some();
        let now = crate::markdown_chat::now_ms();
        let enabled = state.config.markdown_chat.automation.enabled;
        let pending_queue = if enabled && automation_error.is_none() && wake_activity.offline(now) {
            chat.has_wakeable_queued_messages().await.unwrap_or(false)
        } else {
            false
        };
        let automation_wake =
            wake::status(&automation, &wake_activity, pending_queue, enabled, now);
        let goal = automation.goal;
        let goal_status = goal.as_ref().map(|goal| goal.status);
        let goal_completion_dismissed = goal.as_ref().is_some_and(|goal| {
            goal.status == crate::agent_automation::GoalStatus::Completed
                && goal.completed_chat_end.map_or_else(
                    || {
                        last_user_entry_at_ms
                            .zip(goal.finished_at_ms)
                            .is_some_and(|(message, completed)| message > completed)
                    },
                    |completed_end| last_user_entry_end > completed_end,
                )
        });
        let completed_goal = goal
            .as_ref()
            .filter(|goal| goal.status == crate::agent_automation::GoalStatus::Completed);
        list.push(ChatListing {
            id: located.id,
            title,
            custom_title,
            pinned_at_ms,
            archived_at_ms,
            owner_read_through,
            workspace: located.workspace,
            project_id,
            project_path,
            project_group,
            is_scratch: located.is_scratch,
            last_entry_end,
            last_incoming_entry_end,
            last_entry_at_ms,
            last_agent_call_at_ms,
            agent_waiting_until_ms,
            agent_idle_until_ms,
            agent_presence_until_ms,
            goal_status,
            goal_completion_dismissed,
            goal_completed_at_ms: completed_goal.and_then(|goal| goal.finished_at_ms),
            goal_completed_chat_end: completed_goal.and_then(|goal| goal.completed_chat_end),
            goal,
            automation_error,
            automation_retrying,
            automation_waiting_for_browser,
            automation_wake,
            total_tool_calls,
        });
    }
    list.sort_by(|a, b| {
        b.last_entry_at_ms
            .cmp(&a.last_entry_at_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(Json(
        json!({"chats": list, "matchedChatIds": matched, "automationEnabled":state.config.markdown_chat.automation.enabled, "offlineAfterMs":state.config.markdown_chat.offline_after_ms(), "serverTimeMs": crate::markdown_chat::now_ms()}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TitleBody {
    title: String,
}

async fn rename_chat(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<TitleBody>,
) -> ApiResult {
    let located = selected(&state, &id)?;
    let chat = state
        .chats
        .chat_at_path(located.path, true)
        .map_err(internal_error)?;
    chat.set_owner_title(body.title)
        .await
        .map_err(internal_error)?;
    let summary = chat.owner_summary().await.map_err(internal_error)?;
    Ok(Json(
        json!({"title": summary.title, "customTitle": summary.custom_title}),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectPathBody {
    path: String,
    expected_path: String,
}

async fn change_project_path(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<ProjectPathBody>,
) -> ApiResult {
    let _guard = state.creation_lock.lock().await;
    let located = selected(&state, &id)?;
    let previous = located
        .work_dir
        .as_ref()
        .ok_or_else(|| internal_error("This conversation has no saved project path."))?;
    if body.expected_path != previous.to_string_lossy() {
        return Err(internal_error(
            "The project path changed. Refresh before saving.",
        ));
    }
    if body.path.len() > 4096 || body.path.chars().any(char::is_control) {
        return Err((StatusCode::BAD_REQUEST, "Invalid project path.".into()));
    }
    let task = automation_task(&located)?;
    let path = state
        .project_bindings
        .change_owner_workspace(
            &state.config,
            &task,
            previous,
            FilePath::new(&body.path),
            &located.state_root,
        )
        .await
        .map_err(internal_error)?;
    if let Some(supervisor) = &state.automation {
        supervisor.discard_queued_work(&task).await;
    }
    let project_path = path.to_string_lossy().into_owned();
    let project_group = project_group(
        &state.config,
        &path,
        &path.file_name().unwrap_or_default().to_string_lossy(),
        false,
    );
    Ok(Json(
        json!({"projectPath":project_path,"projectId":format!("{:x}",Sha256::digest(project_path.as_bytes())),"projectGroup":project_group,"workspace":path.file_name().unwrap_or_default().to_string_lossy(),"isScratch":false}),
    ))
}

#[derive(Deserialize)]
struct PageQuery {
    before: Option<u64>,
    revision: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadBody {
    through: u64,
}

async fn mark_chat_read(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<ReadBody>,
) -> ApiResult {
    let located = selected(&state, &id)?;
    let chat = state
        .chats
        .chat_at_path(located.path, true)
        .map_err(internal_error)?;
    let through = chat
        .mark_owner_read(body.through)
        .await
        .map_err(internal_error)?;
    Ok(Json(json!({"ownerReadThrough": through})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OrganizationBody {
    pinned: Option<bool>,
    archived: Option<bool>,
}

async fn organize_chat(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<OrganizationBody>,
) -> ApiResult {
    if body.pinned.is_some() == body.archived.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Change either pinning or archiving, one at a time.".into(),
        ));
    }
    let located = selected(&state, &id)?;
    let chat = state
        .chats
        .chat_at_path(located.path, true)
        .map_err(internal_error)?;
    let organization = chat
        .set_owner_organization(body.pinned, body.archived)
        .await
        .map_err(internal_error)?;
    Ok(Json(
        serde_json::to_value(organization).expect("chat organization"),
    ))
}

async fn chat_state(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Query(query): Query<PageQuery>,
) -> ApiResult {
    if query
        .revision
        .as_ref()
        .is_some_and(|revision| revision.len() > 128)
    {
        return Err((StatusCode::BAD_REQUEST, "Invalid chat revision".into()));
    }
    let located = selected(&state, &id)?;
    let chat = state
        .chats
        .chat_at_path(located.path, true)
        .map_err(internal_error)?;
    let task = ConversationIdentity::from_stable_keys(located.owner.clone(), located.owner)
        .ok_or_else(|| internal_error("Conversation has an invalid stable task identity"))?;
    let automation = AgentAutomationStore::for_state_root(&state.config, &located.state_root)
        .and_then(|store| store.load(&task))
        .map_err(internal_error)?;
    if let Some(goal) = automation.goal.as_ref() {
        chat.record_goal_finish(goal)
            .await
            .map_err(internal_error)?;
    }
    let page = chat
        .widget_page(query.before, query.revision)
        .await
        .map_err(internal_error)?;
    let mut page = serde_json::to_value(page).expect("chat page");
    let queued = chat.queued_messages().await.map_err(internal_error)?;
    let summary = chat.owner_summary().await.map_err(internal_error)?;
    page["wake_status"] = json!(wake::status(
        &automation,
        &wake::Activity::from(&summary)
            .with_offline_timeout(state.config.markdown_chat.offline_after_ms()),
        chat.has_wakeable_queued_messages()
            .await
            .map_err(internal_error)?,
        state.config.markdown_chat.automation.enabled,
        crate::markdown_chat::now_ms()
    ));
    page["queued_messages"] = serde_json::to_value(queued).expect("queued messages");
    page["input_required"] = json!(chat.queue_input_required().await.map_err(internal_error)?);
    page["default_message_mode"] =
        json!(crate::global_settings::default_message_mode(&state.config).map_err(internal_error)?);
    Ok(wrap(page))
}

async fn browser_snapshot(state: &OwnerState) -> (Value, Option<Value>) {
    let mut catalog = None;
    let mut companion = match companion_client(state) {
        Ok(client) => match tokio::time::timeout(
            Duration::from_secs(2),
            client.health(&CancellationToken::new()),
        )
        .await
        .unwrap_or_else(|_| Err("Browser companion health check timed out".into()))
        {
            Ok(health) => {
                let key = catalog_context(&health);
                if health.authenticated
                    && let Some(cached) = cached_owner_catalog(state, &key).await
                {
                    catalog = Some(
                        json!({"catalogRevision":cached.catalog_revision,"models":cached.models}),
                    );
                }
                let mut public = public_companion_health(&health);
                public["catalogContext"] = json!(key);
                public
            }
            Err(_) => {
                json!({"status":"unavailable","authenticated":false,"activeActionId":null,"detail":"Browser companion is unavailable"})
            }
        },
        Err(_) => {
            json!({"status":"unavailable","authenticated":false,"activeActionId":null,"detail":"Browser companion is unavailable"})
        }
    };
    if companion["status"] == "unavailable"
        && let Ok(client) = companion_client(state)
    {
        let responsive = matches!(
            tokio::time::timeout(
                Duration::from_secs(2),
                client.liveness(&CancellationToken::new())
            )
            .await,
            Ok(Ok(Some(true)))
        );
        companion["responsive"] = json!(responsive);
    }
    if let Ok(control) = crate::agent_automation::BrowserCompanionControl::new(
        &state.config.markdown_chat.automation,
    ) && let (Some(public), Some(lifecycle)) = (
        companion.as_object_mut(),
        control.public_status().as_object(),
    ) {
        public.extend(lifecycle.clone());
    }
    (companion, catalog)
}

async fn browser_state(State(state): State<OwnerState>) -> ApiResult {
    let (companion, _) = browser_snapshot(&state).await;
    Ok(Json(json!({"companion": companion})))
}

async fn automation_state(State(state): State<OwnerState>, Path(id): Path<String>) -> ApiResult {
    if !state.config.markdown_chat.automation.enabled {
        return Ok(Json(json!({
            "enabled": false,
            "serverTimeMs": crate::markdown_chat::now_ms()
        })));
    }
    let located = selected(&state, &id)?;
    let context = automation_context(&state, &located).await?;
    let automation = context.store.load(&context.task).map_err(internal_error)?;
    let summary = context.chat.owner_summary().await.map_err(internal_error)?;
    let live = match state.automation.as_ref() {
        Some(supervisor) => supervisor.live_state(&context.task).await,
        None => None,
    };
    let queue = match state.automation.as_ref() {
        Some(supervisor) => supervisor
            .queue_position(&context.task)
            .await
            .map(|(position, total)| json!({"position": position, "total": total})),
        None => None,
    };
    let mut public_action = serde_json::to_value(&automation.action).map_err(internal_error)?;
    let retry_mode = automation.retry_mode().or_else(|| {
        (automation.action.status == crate::agent_automation::AutomationActionStatus::Interference
            && automation.action.kind
                != Some(crate::agent_automation::AutomationActionKind::PairVerify)
            && automation.action.send_accepted != Some(true)
            && live.as_ref().is_some_and(|live| {
                live.action_id == automation.action.id
                    && live.status.as_deref()
                        == Some("The shared ChatGPT conversation is already generating")
            }))
        .then_some("retry")
    });
    public_action["retryMode"] = json!(retry_mode);
    public_action["canRunNow"] = json!(automation.can_run_queued_now());
    public_action["automaticRetriesEnabled"] = json!(automation.automatic_retries_enabled);
    public_action["canPauseRetries"] = json!(
        !automation.action.automatic_retry_paused
            && (automation.automatic_retry_at_ms().is_some()
                || automation.action.status
                    == crate::agent_automation::AutomationActionStatus::Queued
                    && automation.action.automatic_retry_in_progress)
    );
    public_action["automaticRetryAtMs"] = json!(automation.automatic_retry_at_ms());
    public_action["cooldownUntilMs"] = json!(automation.browser_cooldown_until_ms());
    public_action["automaticRetryAttempts"] = json!(automation.automatic_retry_attempts());
    public_action["automaticRetryLimit"] = json!(automation.automatic_retry_limit());
    if public_action["lastError"].is_null()
        && let Some(message) = live.as_ref().and_then(|live| live.status.as_ref())
        && matches!(
            automation.action.status,
            crate::agent_automation::AutomationActionStatus::Interference
                | crate::agent_automation::AutomationActionStatus::Failed
                | crate::agent_automation::AutomationActionStatus::ConversationUnavailable
                | crate::agent_automation::AutomationActionStatus::Ambiguous
                | crate::agent_automation::AutomationActionStatus::RateLimited
                | crate::agent_automation::AutomationActionStatus::ApprovalRequired
        )
    {
        public_action["lastError"] = json!(message);
    }
    let (companion, catalog) = browser_snapshot(&state).await;
    let wake = wake::status(
        &automation,
        &wake::Activity::from(&summary)
            .with_offline_timeout(state.config.markdown_chat.offline_after_ms()),
        context
            .chat
            .has_wakeable_queued_messages()
            .await
            .map_err(internal_error)?,
        true,
        crate::markdown_chat::now_ms(),
    );
    let available_at = if automation.action.status
        == crate::agent_automation::AutomationActionStatus::Queued
        && automation.action.kind != Some(crate::agent_automation::AutomationActionKind::PairVerify)
        && !automation.action.bypass_conversation_cooldown
    {
        if let (Ok(client), Some(url)) = (
            companion_client(&state),
            automation.pairing.canonical_url.as_deref(),
        ) {
            client
                .navigation_available_at(url, &CancellationToken::new())
                .await
                .ok()
                .flatten()
                .or(automation.browser_cooldown_until_ms())
        } else {
            automation.browser_cooldown_until_ms()
        }
    } else {
        None
    };
    Ok(Json(json!({
        "enabled": true,
        "ownerGeneration": context.owner_generation,
        "pairing": public_pairing(&automation.pairing, context.owner_generation),
        "selection": {
            "model": automation.selection.model,
            "effort": automation.selection.effort,
            "catalogRevision": automation.selection.catalog_revision
        },
        "goal": automation.goal,
        "action": public_action,
        "live": live,
        "queue": queue,
        "wake": wake,
        "waitUntilMs": available_at,
        "companion": companion,
        "catalog": catalog,
        "presence": {
            "lastAgentCallAtMs": summary.last_agent_call_at_ms,
            "agentWaitingUntilMs": summary.agent_waiting_until_ms,
            "agentIdleUntilMs": summary.agent_idle_until_ms,
            "agentPresenceUntilMs": summary.agent_presence_until_ms,
            "totalToolCalls": summary.total_tool_calls
        },
        "serverTimeMs": crate::markdown_chat::now_ms()
    })))
}

async fn automation_capabilities(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult {
    let located = selected(&state, &id)?;
    let _ = automation_context(&state, &located).await?;
    browser_capabilities(State(state), Json(body)).await
}

async fn browser_capabilities(
    State(state): State<OwnerState>,
    Json(body): Json<Value>,
) -> ApiResult {
    let (health, catalog) = owner_catalog(
        &state,
        body.get("refresh")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    )
    .await?;
    Ok(Json(
        json!({"companion":public_companion_health(&health),"catalogRevision":catalog.catalog_revision,"models":catalog.models}),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SelectionBody {
    request_id: String,
    model: Option<String>,
    effort: Option<String>,
}

async fn automation_selection(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<SelectionBody>,
) -> ApiResult {
    validate_owner_request_id(&body.request_id)?;
    let located = selected(&state, &id)?;
    let context = automation_context(&state, &located).await?;
    if body.model.is_none() && body.effort.is_none() {
        let selection = context
            .store
            .clear_selection(&context.task)
            .map_err(internal_error)?;
        if let Some(supervisor) = state.automation.as_ref() {
            supervisor.discard_queued_work(&context.task).await;
            supervisor.notify();
        }
        return Ok(Json(json!({"selection":selection})));
    }
    let (_, catalog) = owner_catalog(&state, false).await?;
    let model = catalog
        .models
        .iter()
        .find(|model| Some(&model.id) == body.model.as_ref())
        .ok_or_else(|| internal_error("The selected ChatGPT model is unavailable"))?;
    let effort = model
        .efforts
        .iter()
        .find(|effort| Some(&effort.id) == body.effort.as_ref())
        .ok_or_else(|| internal_error("The selected ChatGPT effort is unavailable"))?;
    if !effort.available {
        return Err(internal_error(
            effort
                .reason
                .as_deref()
                .unwrap_or("The selected ChatGPT effort is unavailable"),
        ));
    }
    let selection = context
        .store
        .set_selection(
            &context.task,
            &model.id,
            &effort.id,
            &catalog.catalog_revision,
            &catalog.account_fingerprint,
            crate::markdown_chat::now_ms(),
        )
        .map_err(internal_error)?;
    if let Some(supervisor) = state.automation.as_ref() {
        supervisor.discard_queued_work(&context.task).await;
        supervisor.notify();
    }
    Ok(Json(json!({
        "selection": {
            "model": selection.model,
            "effort": selection.effort,
            "catalogRevision": selection.catalog_revision
        }
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PairBody {
    request_id: String,
    url: String,
}

async fn automation_pair(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<PairBody>,
) -> ApiResult {
    validate_owner_request_id(&body.request_id)?;
    let located = selected(&state, &id)?;
    let context = automation_context(&state, &located).await?;
    let health = companion_client(&state)?
        .health(&CancellationToken::new())
        .await
        .map_err(internal_error)?;
    let fingerprint = health
        .account_fingerprint
        .filter(|_| health.authenticated)
        .ok_or_else(|| internal_error("Sign in to ChatGPT before pairing"))?;
    let supervisor = state
        .automation
        .as_ref()
        .ok_or_else(|| internal_error("Browser automation supervisor is unavailable"))?;
    supervisor.discard_queued_actions(&context.task).await;
    let now_ms = crate::markdown_chat::now_ms();
    let issue = context
        .store
        .issue_pairing(&context.task, &body.url, context.owner_generation, now_ms)
        .map_err(internal_error)?;
    context
        .store
        .bind_pairing_account(&context.task, issue.generation, &fingerprint)
        .map_err(internal_error)?;
    if let Err(error) = supervisor
        .queue_pair_verification(&context.task, &issue.reference, issue.generation, now_ms)
        .await
    {
        let _ = context
            .store
            .invalidate_pairing(&context.task, issue.generation, now_ms);
        return Err(internal_error(error));
    }
    supervisor.notify();
    let current = context.store.load(&context.task).map_err(internal_error)?;
    Ok(Json(json!({
        "pairing": public_pairing(&current.pairing, context.owner_generation),
        "action": current.action
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OwnerMutationBody {
    request_id: String,
}

async fn automation_unlink(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<OwnerMutationBody>,
) -> ApiResult {
    validate_owner_request_id(&body.request_id)?;
    let located = selected(&state, &id)?;
    let context = automation_context(&state, &located).await?;
    let pairing = context
        .store
        .unlink_pairing(&context.task, crate::markdown_chat::now_ms())
        .map_err(internal_error)?;
    if let Some(supervisor) = state.automation.as_ref() {
        supervisor.discard_queued_actions(&context.task).await;
        supervisor.notify();
    }
    Ok(Json(json!({
        "pairing": public_pairing(&pairing, context.owner_generation)
    })))
}

async fn browser_start(
    State(state): State<OwnerState>,
    Json(body): Json<OwnerMutationBody>,
) -> ApiResult {
    validate_owner_request_id(&body.request_id)?;
    let control = crate::agent_automation::BrowserCompanionControl::new(
        &state.config.markdown_chat.automation,
    )
    .map_err(internal_error)?;
    let client = control
        .start(&CancellationToken::new())
        .await
        .map_err(internal_error)?;
    let mut companion = match tokio::time::timeout(
        Duration::from_secs(2),
        client.health(&CancellationToken::new()),
    )
    .await
    {
        Ok(Ok(health)) => public_companion_health(&health),
        _ => {
            json!({"status":"unavailable","authenticated":false,"responsive":true,"activeActionId":null})
        }
    };
    if companion["status"] == "unavailable" {
        companion["responsive"] = json!(true);
    }
    state.global.lock().await.reset_plan();
    if let Some(supervisor) = &state.automation {
        supervisor.notify();
    }
    Ok(Json(
        json!({"companion":companion,"lifecycle":control.public_status()}),
    ))
}
async fn browser_stop(
    State(state): State<OwnerState>,
    Json(body): Json<OwnerMutationBody>,
) -> ApiResult {
    validate_owner_request_id(&body.request_id)?;
    let control = crate::agent_automation::BrowserCompanionControl::new(
        &state.config.markdown_chat.automation,
    )
    .map_err(internal_error)?;
    control
        .stop(&CancellationToken::new())
        .await
        .map_err(internal_error)?;
    Ok(Json(
        json!({"companion":{"status":"unavailable","authenticated":false,"activeActionId":null},"lifecycle":control.public_status()}),
    ))
}

async fn automation_login(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(_): Json<Value>,
) -> ApiResult {
    let located = selected(&state, &id)?;
    let _ = automation_context(&state, &located).await?;
    browser_login(State(state), Json(json!({}))).await
}

async fn browser_login(State(state): State<OwnerState>, Json(_): Json<Value>) -> ApiResult {
    let client = crate::agent_automation::BrowserCompanionControl::new(
        &state.config.markdown_chat.automation,
    )
    .map_err(internal_error)?
    .start(&CancellationToken::new())
    .await
    .map_err(internal_error)?;
    let health = client
        .login(&CancellationToken::new())
        .await
        .map_err(internal_error)?;
    state.global.lock().await.reset_plan();
    Ok(Json(json!({"companion": public_companion_health(&health)})))
}

async fn automation_reveal(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(_): Json<Value>,
) -> ApiResult {
    let located = selected(&state, &id)?;
    let store = AgentAutomationStore::for_state_root(&state.config, &located.state_root)
        .map_err(internal_error)?;
    let automation = store
        .load(&automation_task(&located)?)
        .map_err(internal_error)?;
    let url = automation
        .pairing
        .canonical_url
        .or(automation.pairing.candidate_url)
        .ok_or_else(|| internal_error("Pair a ChatGPT conversation before opening it"))?;
    let open_browser = state.open_browser.clone();
    tokio::task::spawn_blocking(move || open_browser(&url))
        .await
        .map_err(internal_error)?
        .map_err(|error| internal_error(format!("Could not open the default browser: {error}")))?;
    Ok(Json(json!({"opened": true})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActionMutationBody {
    request_id: String,
    action_id: String,
}

async fn automation_action_state(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
) -> ApiResult {
    let located = selected(&state, &id)?;
    let context = automation_context(&state, &located).await?;
    let action = context
        .store
        .load(&context.task)
        .map_err(internal_error)?
        .action;
    let (live, queue) = match state.automation.as_ref() {
        Some(supervisor) => {
            let live = supervisor.live_state(&context.task).await;
            let queue = supervisor
                .queue_position(&context.task)
                .await
                .map(|(position, total)| json!({"position": position, "total": total}));
            (live, queue)
        }
        None => (None, None),
    };
    Ok(Json(
        json!({"action": action, "live": live, "queue": queue}),
    ))
}

async fn automation_action_cancel(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<ActionMutationBody>,
) -> ApiResult {
    validate_owner_request_id(&body.request_id)?;
    let located = selected(&state, &id)?;
    let context = automation_context(&state, &located).await?;
    let supervisor = state
        .automation
        .as_ref()
        .ok_or_else(|| internal_error("Browser automation supervisor is unavailable"))?;
    let action = supervisor
        .cancel_action(
            &context.task,
            &body.action_id,
            crate::markdown_chat::now_ms(),
        )
        .await
        .map_err(internal_error)?;
    let queue = supervisor
        .queue_position(&context.task)
        .await
        .map(|(position, total)| json!({"position": position, "total": total}));
    Ok(Json(json!({"action": action, "queue": queue})))
}

async fn automation_action_now(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<ActionMutationBody>,
) -> ApiResult {
    require_connector_name(&state.config)?;
    validate_owner_request_id(&body.request_id)?;
    let located = selected(&state, &id)?;
    let context = automation_context(&state, &located).await?;
    let supervisor = state
        .automation
        .as_ref()
        .ok_or_else(|| internal_error("Browser automation supervisor is unavailable"))?;
    let action = supervisor
        .run_queued_action_now(
            &context.task,
            &body.action_id,
            crate::markdown_chat::now_ms(),
        )
        .await
        .map_err(internal_error)?;
    Ok(Json(json!({"action":action})))
}

async fn automation_action_pause_retries(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<ActionMutationBody>,
) -> ApiResult {
    validate_owner_request_id(&body.request_id)?;
    let located = selected(&state, &id)?;
    let context = automation_context(&state, &located).await?;
    let supervisor = state
        .automation
        .as_ref()
        .ok_or_else(|| internal_error("Browser automation supervisor is unavailable"))?;
    let action = supervisor
        .pause_action_retries(&context.task, &body.action_id)
        .await
        .map_err(internal_error)?;
    Ok(Json(json!({"action": action})))
}

async fn automation_action_retry(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<ActionMutationBody>,
) -> ApiResult {
    require_connector_name(&state.config)?;
    validate_owner_request_id(&body.request_id)?;
    let located = selected(&state, &id)?;
    let context = automation_context(&state, &located).await?;
    let supervisor = state
        .automation
        .as_ref()
        .ok_or_else(|| internal_error("Browser automation supervisor is unavailable"))?;
    let action = supervisor
        .retry_failed_action(
            &context.task,
            &body.action_id,
            crate::markdown_chat::now_ms(),
        )
        .await
        .map_err(internal_error)?;
    let queue = supervisor
        .queue_position(&context.task)
        .await
        .map(|(position, total)| json!({"position": position, "total": total}));
    Ok(Json(json!({"action": action, "queue": queue})))
}

async fn chat_markdown(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    let located = selected(&state, &id)?;
    let chat = state
        .chats
        .chat_at_path(located.path, true)
        .map_err(internal_error)?;
    let transcript = chat
        .clean_markdown_transcript()
        .await
        .map_err(internal_error)?;
    let mut response = transcript.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/markdown; charset=utf-8"),
    );
    Ok(response)
}

#[derive(Deserialize)]
struct SendBody {
    request_id: String,
    message: String,
    #[serde(default)]
    stop_agent: bool,
    #[serde(default)]
    attachment_ids: Vec<String>,
    #[serde(default)]
    message_mode: Option<crate::markdown_chat::MessageMode>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueActionBody {
    id: String,
    revision: u64,
    action: crate::markdown_chat::QueueSubmission,
    message: Option<String>,
}

async fn chat_queue_action(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<QueueActionBody>,
) -> ApiResult {
    let located = selected(&state, &id)?;
    let automation = if body.action == crate::markdown_chat::QueueSubmission::Steer
        && state.config.markdown_chat.automation.enabled
    {
        Some(automation_context(&state, &located).await?)
    } else {
        None
    };
    if let (Some(supervisor), Some(context)) = (&state.automation, &automation) {
        supervisor
            .prepare_schema_rollover(&context.task)
            .await
            .map_err(internal_error)?;
    }
    let chat = state
        .chats
        .chat_at_path(located.path, true)
        .map_err(internal_error)?;
    let receipt = chat
        .update_queued_message(body.id, body.revision, body.action, body.message)
        .await
        .map_err(internal_error)?;
    if let (Some(context), Some(sent)) = (automation, receipt.sent.as_ref()) {
        if let Some(at) = sent.created_at_ms {
            context
                .store
                .resume_cancelled_action_for_new_message(&context.task, at)
                .map_err(internal_error)?;
        }
        chat.set_owner_organization(None, Some(false))
            .await
            .map_err(internal_error)?;
        if let Some(supervisor) = state.automation.as_ref() {
            supervisor.notify();
        }
    }
    Ok(wrap(
        json!({"queue_receipt":receipt,"queued_messages":chat.queued_messages().await.map_err(internal_error)?}),
    ))
}

fn automation_task(
    located: &LocatedChat,
) -> Result<crate::project_bindings::ConversationIdentity, (StatusCode, String)> {
    crate::project_bindings::ConversationIdentity::from_stable_keys(
        located.owner.clone(),
        located.owner.clone(),
    )
    .ok_or_else(|| internal_error("Conversation has an invalid stable task identity"))
}

struct OwnerAutomationContext {
    task: ConversationIdentity,
    owner_generation: u64,
    store: Arc<AgentAutomationStore>,
    chat: Arc<crate::markdown_chat::ChatFile>,
}

async fn automation_context(
    state: &OwnerState,
    located: &LocatedChat,
) -> Result<OwnerAutomationContext, (StatusCode, String)> {
    if !state.config.markdown_chat.automation.enabled {
        return Err(internal_error("Agent-chat browser automation is disabled"));
    }
    let task = automation_task(located)?;
    let store = Arc::new(
        AgentAutomationStore::for_state_root(&state.config, &located.state_root)
            .map_err(internal_error)?,
    );
    let mut owner_generation = state
        .continuations
        .owner_generation_for_task_key(task.stable_key())
        .map_err(internal_error)?;
    let pairing = store.load(&task).map_err(internal_error)?.pairing;
    if pairing.creating_chat
        && let Some(pending) = pairing.pending
        && pending.owner_generation == owner_generation.saturating_add(1)
    {
        owner_generation = pending.owner_generation;
    }
    let chat = state
        .chats
        .chat_at_path(located.path.clone(), true)
        .map_err(internal_error)?;
    if let Some(supervisor) = state.automation.as_ref() {
        supervisor
            .register_task(AutomationTaskHandle::new(
                task.clone(),
                owner_generation,
                store.clone(),
                chat.clone(),
            ))
            .await
            .map_err(internal_error)?;
    }
    Ok(OwnerAutomationContext {
        task,
        owner_generation,
        store,
        chat,
    })
}

fn validate_owner_request_id(request_id: &str) -> Result<(), (StatusCode, String)> {
    if request_id.is_empty()
        || request_id.len() > 512
        || request_id.contains('\0')
        || request_id.chars().any(char::is_control)
    {
        return Err((StatusCode::BAD_REQUEST, "Invalid request ID".into()));
    }
    Ok(())
}

fn companion_client(state: &OwnerState) -> Result<BrowserCompanionClient, (StatusCode, String)> {
    BrowserCompanionClient::from_config(&state.config.markdown_chat.automation)
        .map_err(internal_error)
}

fn public_companion_health(health: &crate::agent_automation::CompanionHealth) -> Value {
    json!({
        "status": health.status,
        "authenticated": health.authenticated,
        "activeActionId": health.active_action_id
    })
}

fn pairing_status(
    pairing: &crate::agent_automation::PairingState,
    owner_generation: u64,
) -> &'static str {
    if pairing.canonical_url.is_some()
        && pairing.verified_owner_generation == Some(owner_generation)
    {
        "verified"
    } else if pairing.candidate_url.is_some() || pairing.canonical_url.is_some() {
        "unverified"
    } else {
        "unpaired"
    }
}

fn public_pairing(pairing: &crate::agent_automation::PairingState, owner_generation: u64) -> Value {
    let last_tried_url = [
        pairing.last_attempted_url.as_deref(),
        pairing.candidate_url.as_deref(),
        pairing.canonical_url.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(crate::agent_automation::conversation_url_for_retry);
    json!({"creatingChat":pairing.creating_chat,
        "status": pairing_status(pairing, owner_generation),
        "candidateUrl": pairing.candidate_url,
        "canonicalUrl": pairing.canonical_url,
        "lastTriedUrl": last_tried_url,
        "generation": pairing.generation,
        "verifiedAtMs": pairing.verified_at_ms,
        "expiresAtMs": pairing.pending.as_ref().map(|pending| pending.expires_at_ms),
        "verificationWaitUntilMs": pairing.verification_sent_at_ms.map(|sent| sent.saturating_add(crate::agent_automation::PAIRING_VERIFICATION_GRACE_MS))
    })
}

async fn wake_status_for_chat(
    state: &OwnerState,
    located: &LocatedChat,
    chat: &Arc<crate::markdown_chat::ChatFile>,
) -> Option<wake::WakeStatus> {
    if !state.config.markdown_chat.automation.enabled {
        return None;
    }
    let task = automation_task(located).ok()?;
    let automation = AgentAutomationStore::for_state_root(&state.config, &located.state_root)
        .and_then(|store| store.load(&task))
        .ok()?;
    let summary = chat.owner_summary().await.ok()?;
    let queued = chat.has_wakeable_queued_messages().await.ok()?;
    wake::status(
        &automation,
        &wake::Activity::from(&summary)
            .with_offline_timeout(state.config.markdown_chat.offline_after_ms()),
        queued,
        true,
        crate::markdown_chat::now_ms(),
    )
}

async fn chat_send(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<SendBody>,
) -> ApiResult {
    let located = selected(&state, &id)?;
    if body.stop_agent {
        if body.request_id.is_empty()
            || body.request_id.len() > 80
            || !body
                .request_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err((
                StatusCode::BAD_REQUEST,
                "Invalid agent stop request ID".into(),
            ));
        }
        let chat = state
            .chats
            .chat_at_path(located.path.clone(), true)
            .map_err(internal_error)?;
        let goal = if state.config.markdown_chat.automation.enabled {
            let context = automation_context(&state, &located).await?;
            let receipt = context
                .store
                .pause_goal_for_agent_stop(
                    &context.task,
                    &body.request_id,
                    crate::markdown_chat::now_ms(),
                )
                .map_err(internal_error)?;
            if let Some(supervisor) = state.automation.as_ref() {
                supervisor.discard_queued_work(&context.task).await;
            }
            receipt.goal
        } else {
            None
        };
        chat.request_agent_stop(body.request_id)
            .await
            .map_err(internal_error)?;
        return Ok(wrap(json!({"stopped":true,"goal":goal})));
    }
    let message = if body.attachment_ids.is_empty() {
        parse_owner_message(&body.message).map_err(|error| (StatusCode::BAD_REQUEST, error))?
    } else {
        OwnerMessage::Chat(body.message.clone())
    };
    match message {
        OwnerMessage::Chat(message) => {
            let automation = if state.config.markdown_chat.automation.enabled {
                Some(automation_context(&state, &located).await?)
            } else {
                None
            };
            if let (Some(supervisor), Some(context)) = (&state.automation, &automation) {
                supervisor
                    .prepare_schema_rollover(&context.task)
                    .await
                    .map_err(internal_error)?;
            }
            let mode = match body.message_mode {
                Some(mode) => mode,
                None => {
                    let chat = state
                        .chats
                        .chat_at_path(located.path.clone(), true)
                        .map_err(internal_error)?;
                    if chat.queue_input_required().await.map_err(internal_error)? {
                        crate::markdown_chat::MessageMode::Steer
                    } else {
                        crate::global_settings::default_message_mode(&state.config)
                            .map_err(internal_error)?
                    }
                }
            };
            if mode == crate::markdown_chat::MessageMode::Queue {
                let chat = state
                    .chats
                    .chat_at_path(located.path.clone(), true)
                    .map_err(internal_error)?;
                let receipt = chat
                    .queue_user_message(body.request_id, message, body.attachment_ids)
                    .await
                    .map_err(internal_error)?;
                let queued = chat.queued_messages().await.map_err(internal_error)?;
                let wake_status = wake_status_for_chat(&state, &located, &chat).await;
                return Ok(wrap(
                    json!({"queue_receipt":receipt,"queued_messages":queued,"wake_status":wake_status}),
                ));
            }
            let chat = state
                .chats
                .chat_at_path(located.path.clone(), true)
                .map_err(internal_error)?;
            let sent = chat
                .append_user_with_attachments(body.request_id, message, body.attachment_ids)
                .await
                .map_err(internal_error)?;
            if let (Some(context), Some(created_at_ms)) = (automation, sent.created_at_ms) {
                context
                    .store
                    .resume_cancelled_action_for_new_message(&context.task, created_at_ms)
                    .map_err(internal_error)?;
            }
            let organization = chat
                .set_owner_organization(None, Some(false))
                .await
                .map_err(internal_error)?;
            if let Some(supervisor) = state.automation.as_ref() {
                supervisor.notify();
            }
            let wake_status = wake_status_for_chat(&state, &located, &chat).await;
            Ok(wrap(
                json!({"sent": sent, "organization": organization, "wake_status":wake_status}),
            ))
        }
        OwnerMessage::Goal(command) => {
            if matches!(
                &command,
                crate::agent_automation::OwnerGoalCommand::Set { .. }
                    | crate::agent_automation::OwnerGoalCommand::Run
            ) {
                require_connector_name(&state.config)?;
            }
            let context = automation_context(&state, &located).await?;
            if matches!(
                &command,
                crate::agent_automation::OwnerGoalCommand::Set { .. }
                    | crate::agent_automation::OwnerGoalCommand::Run
            ) && let Some(supervisor) = &state.automation
            {
                supervisor
                    .prepare_schema_rollover(&context.task)
                    .await
                    .map_err(internal_error)?;
            }
            let mut receipt = context
                .store
                .apply_owner_goal_command(
                    &context.task,
                    &body.request_id,
                    command,
                    crate::markdown_chat::now_ms(),
                )
                .map_err(internal_error)?;
            if receipt.updated_active_goal {
                let goal = receipt.goal.as_ref().expect("updated active goal receipt");
                let notification_id = format!(
                    "goal-update-{:x}",
                    Sha256::digest(body.request_id.as_bytes())
                );
                context.chat.append_user(notification_id, format!(
                    "The goal has changed (revision {}). The new goal is:\n\n{}\n\nCall `goal_read` for the current revision and continue toward the updated goal.",
                    goal.revision, goal.markdown,
                )).await.map_err(internal_error)?;
            }
            if !receipt.replayed
                && matches!(
                    receipt.kind,
                    crate::agent_automation::OwnerGoalCommandKind::Set
                        | crate::agent_automation::OwnerGoalCommandKind::Run
                )
            {
                context.chat.resume_agent().await.map_err(internal_error)?;
                let activity = context
                    .chat
                    .offline_activity()
                    .await
                    .map_err(internal_error)?;
                context
                    .store
                    .sync_goal_presence(
                        &context.task,
                        activity.last_call_for_presence(crate::markdown_chat::now_ms()),
                        activity.agent_waiting_until_ms,
                        activity.agent_stop_requested,
                        crate::markdown_chat::now_ms(),
                    )
                    .map_err(internal_error)?;
                receipt.goal = context
                    .store
                    .load(&context.task)
                    .map_err(internal_error)?
                    .goal;
            }
            if let Some(supervisor) = state.automation.as_ref() {
                supervisor.notify();
            }
            Ok(wrap(json!({
                "localCommand": receipt.clone(),
                "goal": receipt.goal,
                "goalCommand": receipt.kind,
                "replayed": receipt.replayed
            })))
        }
    }
}

#[derive(Deserialize)]
struct FileQuery {
    href: String,
}

async fn chat_file(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
) -> ApiResult {
    if query.href.is_empty() || query.href.len() > 4096 {
        return Err((StatusCode::BAD_REQUEST, "Invalid file reference".into()));
    }
    let located = selected(&state, &id)?;
    let work_dir = state
        .project_bindings
        .owner_chat_work_dirs(&state.config)
        .remove(&located.owner)
        .or(located.work_dir)
        .ok_or_else(|| internal_error("Workspace path is unavailable for this conversation"))?;
    let resource = state
        .artifacts
        .chat_file_link(&work_dir, &query.href, &CancellationToken::new())
        .await
        .map_err(internal_error)?;
    let mut file = serde_json::to_value(resource).expect("file resource");
    file["type"] = json!("resource_link");
    Ok(wrap(json!({"file": file})))
}

#[derive(Deserialize)]
struct DownloadQuery {
    uri: String,
}

async fn download(
    State(state): State<OwnerState>,
    Query(query): Query<DownloadQuery>,
) -> Result<Response, (StatusCode, String)> {
    let Some(contents) = state
        .artifacts
        .read_resource(&query.uri, &CancellationToken::new())
        .await
        .map_err(internal_error)?
    else {
        return Err((StatusCode::NOT_FOUND, "File not found".into()));
    };
    let rmcp::model::ResourceContents::BlobResourceContents {
        blob, mime_type, ..
    } = contents
    else {
        return Err(internal_error("File content is unavailable"));
    };
    let bytes = STANDARD.decode(blob).map_err(internal_error)?;
    let mut response = bytes.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        mime_type
            .as_deref()
            .unwrap_or("application/octet-stream")
            .parse()
            .unwrap_or_else(|_| header::HeaderValue::from_static("application/octet-stream")),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_static("attachment"),
    );
    Ok(response)
}

async fn page() -> Html<String> {
    Html(OWNER_HTML.clone())
}

async fn authorize(
    State(state): State<OwnerState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let content_request = attachments::is_content_path(request.uri().path());
    let host_ok = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| host.starts_with("127.0.0.1:"));
    if !host_ok {
        return StatusCode::FORBIDDEN.into_response();
    }
    let bearer_authenticated = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        == Some(format!("Bearer {}", state.token).as_str());
    let secure_cookie = request
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        == Some("https");
    if request.uri().path() != "/" && !pwa::is_asset(request.uri().path()) {
        let expected = format!("Bearer {}", state.token);
        if request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            != Some(&expected)
            && !attachments::authorized_cookie(&request, &state.token)
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    let mut response = next.run(request).await;
    if bearer_authenticated {
        let secure = if secure_cookie { "; Secure" } else { "" };
        if let Ok(value) = format!(
            "codexify-attachment={}; Path=/api/chats/; HttpOnly; SameSite=Strict{secure}",
            state.token
        )
        .parse()
        {
            response.headers_mut().insert(header::SET_COOKIE, value);
        }
    }
    if !content_request || !response.headers().contains_key(header::CACHE_CONTROL) {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
    }
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(if content_request {
            "sandbox; default-src 'none'; img-src 'self' blob: data:; media-src 'self' blob:; frame-ancestors 'self'; base-uri 'none'; form-action 'none'"
        }else{"default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src 'self' blob: data:; media-src 'self' blob:; manifest-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"}),
    );
    response
}

static OWNER_HTML: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let style = CHAT_UI_HTML
        .split_once("<style>")
        .unwrap()
        .1
        .split_once("</style>")
        .unwrap()
        .0
        .replace(
            ":root:not([data-theme=\"light\"])",
            ":host(:not([data-theme=\"light\"]))",
        )
        .replace(":root[data-theme=\"dark\"]", ":host([data-theme=\"dark\"])")
        .replace(":root", ":host")
        .replace("body {", ":host { display:block;");
    let body = CHAT_UI_HTML
        .split_once("<body>")
        .unwrap()
        .1
        .split_once("<script>")
        .unwrap()
        .0;
    let script = CHAT_UI_HTML
        .split_once("<script>")
        .unwrap()
        .1
        .split_once("</script>")
        .unwrap()
        .0;
    include_str!("owner_chat.html")
        .replace("<!-- CHAT_TEMPLATE -->", &format!("<template id=\"chat-template\"><style>{style}\n#chat {{ max-width:none; height:100%; border:0; border-radius:0; display:flex; flex-direction:column; }} #panel {{ display:flex; flex:1; min-height:0; flex-direction:column; }} #panel[hidden] {{ display:none; }} #messages {{ flex:1; max-height:none; min-height:0; }}\n</style>{body}</template>"))
        .replace("/* CHAT_SCRIPT */", script)
        .replace("/* GLOBAL_SETTINGS_SCRIPT */", include_str!("global_settings_ui.js"))
        .replace("/* OWNER_PWA_SCRIPT */", include_str!("owner_pwa_ui.js"))
        .replace("/* USAGE_ANALYTICS_SCRIPT */", include_str!("analytics_ui.js"))
        .replace("/* STATISTICS_EXTRAS_SCRIPT */", include_str!("statistics_extras_ui.js"))
});

pub async fn start(
    config: Arc<AppConfig>,
    chats: Arc<MarkdownChatStore>,
    artifacts: Arc<ArtifactEgressStore>,
) -> anyhow::Result<OwnerServer> {
    let continuations = Arc::new(
        ConversationContinuationStore::for_current_user(&config).map_err(anyhow::Error::msg)?,
    );
    start_with_automation(config, chats, artifacts, continuations, None).await
}

async fn restore_missing_automation(state: &OwnerState, chats: &[LocatedChat]) {
    let Some(supervisor) = state
        .automation
        .as_ref()
        .filter(|_| state.config.markdown_chat.automation.enabled)
    else {
        return;
    };
    for located in chats {
        let task = match automation_task(located) {
            Ok(task) => task,
            Err(error) => {
                tracing::warn!(?error, "could not identify persisted chat automation");
                continue;
            }
        };
        if supervisor.is_registered(&task).await {
            continue;
        }
        let saved = match AgentAutomationStore::for_state_root(&state.config, &located.state_root)
            .and_then(|store| store.load(&task))
        {
            Ok(saved) => saved,
            Err(error) => {
                tracing::warn!(task=task.stable_key(), %error, "could not read persisted chat automation; discovery will retry");
                continue;
            }
        };
        let active_goal = saved
            .goal
            .as_ref()
            .is_some_and(|goal| goal.status == crate::agent_automation::GoalStatus::Active);
        if !active_goal
            && saved.turn_renewal.handoff.is_none()
            && !located.path.with_file_name("message-queue.json").exists()
        {
            continue;
        }
        match automation_context(state, located).await {
            Ok(_) => tracing::info!(
                task = task.stable_key(),
                active_goal,
                "restored persisted chat automation"
            ),
            Err(error) => tracing::warn!(
                task = task.stable_key(),
                ?error,
                "could not restore persisted chat automation; discovery will retry"
            ),
        }
    }
}

async fn maintain_chat_automation(state: OwnerState, interval: std::time::Duration) {
    restore_chat_state(state.clone()).await;
    if state.automation.is_none() || !state.config.markdown_chat.automation.enabled {
        return;
    }
    loop {
        tokio::time::sleep(interval).await;
        let scan = state.clone();
        match tokio::task::spawn_blocking(move || locate(&scan.config, &scan.project_bindings))
            .await
        {
            Ok(chats) => restore_missing_automation(&state, &chats).await,
            Err(error) => {
                tracing::warn!(%error, "persisted chat discovery failed; discovery will retry")
            }
        }
    }
}

async fn restore_chat_state(state: OwnerState) {
    let scan = state.clone();
    let Ok(chats) =
        tokio::task::spawn_blocking(move || locate(&scan.config, &scan.project_bindings)).await
    else {
        return;
    };
    restore_missing_automation(&state, &chats).await;
    for located in chats {
        // Opening the durable queue resumes its completion/offline monitor without a widget.
        match state.chats.chat_at_path(located.path, true) {
            Ok(chat) => {
                if let Err(error) = chat.ensure_rollout().await {
                    tracing::warn!(%error, "could not initialize chat progress history");
                }
            }
            Err(error) => tracing::warn!(%error, "could not restore chat state"),
        }
    }
}

pub(crate) async fn start_with_automation(
    config: Arc<AppConfig>,
    chats: Arc<MarkdownChatStore>,
    artifacts: Arc<ArtifactEgressStore>,
    continuations: Arc<ConversationContinuationStore>,
    automation: Option<Arc<AgentAutomationSupervisor>>,
) -> anyhow::Result<OwnerServer> {
    crate::usage_analytics::initialize();
    let listener = bind_owner_listener(&config).await?;
    let port = listener.local_addr()?.port();
    let info = load_or_create_runtime(&runtime_path(), port)?;
    let state = OwnerState {
        project_bindings: Arc::new(
            crate::project_bindings::ProjectBindingStore::for_current_user(),
        ),
        creation_lock: Arc::default(),
        config,
        chats,
        artifacts,
        continuations,
        automation,
        open_browser: Arc::new(|url| open::that(url)),
        catalog_cache: Arc::default(),
        global: Arc::default(),
        token: info.token,
    };
    let maintenance = tokio::spawn(maintain_chat_automation(
        state.clone(),
        std::time::Duration::from_secs(30),
    ));
    settings::start(state.clone());
    let app = router(state);
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::error!("owner chat listener stopped: {error}");
        }
    });
    Ok(OwnerServer { task, maintenance })
}

async fn bind_owner_listener(config: &AppConfig) -> anyhow::Result<tokio::net::TcpListener> {
    let port = config.markdown_chat.port.unwrap_or(0);
    tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| match config.markdown_chat.port {
            Some(port) => format!("bind standalone agent chat to 127.0.0.1:{port}"),
            None => "bind standalone agent chat to an ephemeral loopback port".into(),
        })
}

fn router(state: OwnerState) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/manifest.webmanifest", get(pwa::serve))
        .route("/icons/{name}", get(pwa::serve))
        .route("/api/health", get(settings::health))
        .route("/api/global", get(settings::status))
        .route("/api/settings", get(settings::read).post(settings::save))
        .route(
            "/api/settings/connector",
            get(settings::connector).post(settings::save_connector),
        )
        .route("/api/analytics/turn-cutoffs", get(analytics::read))
        .route("/api/analytics/usage", get(analytics::usage))
        .route("/api/analytics/extras", get(analytics::extras))
        .route("/api/analytics/extras/claim", post(analytics::claim_extra))
        .route("/api/settings/restart", post(settings::restart))
        .route("/api/settings/update", post(settings::update))
        .route(
            "/api/settings/refresh-connector",
            post(settings::refresh_connector),
        )
        .route(
            "/api/chats",
            get(list_chats).post(creation::create_chat_response),
        )
        .route("/api/new-chat", post(creation::prepare_chat))
        .route("/api/new-chat/login", post(creation::login))
        .route(
            "/api/projects",
            get(creation::projects).post(creation::create_project),
        )
        .route("/api/projects/worktrees", get(creation::worktrees))
        .route(
            "/api/chats/{id}/automation/create/retry",
            post(creation::retry_creation),
        )
        .route("/api/chats/{id}", get(chat_state))
        .route("/api/chats/{id}/title", post(rename_chat))
        .route("/api/chats/{id}/organization", post(organize_chat))
        .route("/api/chats/{id}/read", post(mark_chat_read))
        .route("/api/browser", get(browser_state))
        .route("/api/browser/login", post(browser_login))
        .route("/api/browser/capabilities", post(browser_capabilities))
        .route("/api/browser/start", post(browser_start))
        .route("/api/browser/stop", post(browser_stop))
        .route("/api/chats/{id}/automation", get(automation_state))
        .route(
            "/api/chats/{id}/automation/capabilities",
            post(automation_capabilities),
        )
        .route(
            "/api/chats/{id}/automation/selection",
            post(automation_selection),
        )
        .route("/api/chats/{id}/queue", post(chat_queue_action))
        .route("/api/chats/{id}/automation/pair", post(automation_pair))
        .route("/api/chats/{id}/automation/unlink", post(automation_unlink))
        .route("/api/chats/{id}/automation/login", post(automation_login))
        .route("/api/chats/{id}/automation/reveal", post(automation_reveal))
        .route(
            "/api/chats/{id}/automation/action",
            get(automation_action_state),
        )
        .route(
            "/api/chats/{id}/automation/action/cancel",
            post(automation_action_cancel),
        )
        .route(
            "/api/chats/{id}/automation/action/pause-retries",
            post(automation_action_pause_retries),
        )
        .route(
            "/api/chats/{id}/automation/action/retry",
            post(automation_action_retry),
        )
        .route(
            "/api/chats/{id}/automation/action/now",
            post(automation_action_now),
        )
        .route("/api/chats/{id}/markdown", get(chat_markdown))
        .route("/api/chats/{id}/send", post(chat_send))
        .route("/api/chats/{id}/project", post(change_project_path))
        .route("/api/chats/{id}/attachments", post(attachments::upload))
        .route(
            "/api/chats/{id}/attachments/{attachment}",
            get(attachments::metadata),
        )
        .route(
            "/api/chats/{id}/attachments/{attachment}/content",
            get(attachments::content),
        )
        .route("/api/chats/{id}/file", get(chat_file))
        .route("/api/files", get(download))
        // A 16 MiB chat entry may expand sixfold when JSON-escaped.
        .layer(axum::extract::DefaultBodyLimit::max(
            crate::markdown_chat::MAX_UNREAD_BYTES * 6 + 1024,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authorize,
        ))
        .with_state(state)
}

/// Return a verified local URL. The secret is only printed by this explicit CLI command.
pub async fn dashboard_url() -> anyhow::Result<String> {
    crate::tls::ensure_crypto_provider();
    let info = read_runtime(&runtime_path())?.ok_or_else(|| {
        anyhow::anyhow!(
            "Owner chat is unavailable; start the Codexify service with agentChat enabled"
        )
    })?;
    let url = format!("http://127.0.0.1:{}/", info.port);
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?
        .get(format!("{url}api/chats"))
        .bearer_auth(&info.token)
        .send()
        .await?;
    if !response.status().is_success()
        || !response
            .json::<Value>()
            .await?
            .get("chats")
            .is_some_and(Value::is_array)
    {
        anyhow::bail!(
            "Owner chat is unavailable; start the Codexify service with agentChat enabled"
        );
    }
    Ok(format!("{url}#{}", info.token))
}

#[cfg(test)]
mod tests {
    include!("owner_chat_catalog_tests.rs");
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crate::agent_automation::{
        AutomationExecution, AutomationExecutionError, AutomationExecutor, CompanionActionSnapshot,
        PROTOCOL_VERSION,
    };
    use crate::exec_sessions::SessionState;
    use crate::project_bindings::ConversationIdentity;

    struct OwnerNoopExecutor;

    const OWNER_COMPANION_TOKEN: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc";
    const OWNER_COMPANION_BUILD: &str = "owner-companion-test-v1+protocol.2f7852fd999d639b18e829bc273b2e38021c40c839ec0feb0ff2c793cfeb9e03";

    #[test]
    fn project_groups_resolve_registered_worktrees_without_merging_same_named_projects() {
        let temp = tempfile::tempdir().unwrap();
        let access = temp.path().join("projects");
        let source = access.join("first/repo");
        let other = access.join("second/repo");
        let hooks = temp.path().join("empty-hooks");
        for path in [&source, &other, &hooks] {
            std::fs::create_dir_all(path).unwrap();
        }
        let git = |path: &FilePath, args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-c")
                .arg(format!("core.hooksPath={}", hooks.display()))
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "user.name=Test User",
                    "-c",
                    "user.email=test@example.test",
                ])
                .args(args)
                .current_dir(path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&source, &["init", "-q"]);
        std::fs::create_dir_all(source.join("packages/ui")).unwrap();
        std::fs::write(source.join("packages/ui/fixture.txt"), "fixture\n").unwrap();
        git(&source, &["add", "-f", "packages/ui/fixture.txt"]);
        git(&source, &["commit", "-qm", "Initial project"]);
        git(&other, &["init", "-q"]);
        let first = temp.path().join("worktrees/feature-a");
        let second = temp.path().join("worktrees/feature-b");
        for path in [&first, &second] {
            git(
                &source,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    "-q",
                    path.to_str().unwrap(),
                    "HEAD",
                ],
            );
        }
        let config = crate::config::default_config(access);
        let group = project_group(&config, &source, "repo", false);
        assert_eq!(group.name, "repo");
        for worktree in [&first, &second] {
            let grouped = project_group(&config, worktree, "feature", false);
            assert_eq!(grouped.path, group.path);
            assert_eq!(grouped.name, group.name);
            let package = project_group(&config, &worktree.join("packages/ui"), "ui", false);
            assert_eq!(
                package.path,
                source
                    .join("packages/ui")
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
            );
            assert_eq!(package.name, "ui");
            assert_ne!(package.path, group.path);
        }
        let separate = project_group(&config, &other, "repo", false);
        assert_eq!(separate.name, group.name);
        assert_ne!(separate.path, group.path);
        std::fs::write(
            first.join(".codexify-worktree.json"),
            serde_json::json!({"sourceProjectRoot":other}).to_string(),
        )
        .unwrap();
        assert_eq!(
            project_group(&config, &first, "feature", false).path,
            group.path,
            "display grouping uses Git registration, not an unverified metadata claim"
        );
        let unregistered = temp.path().join("unregistered/repo");
        std::fs::create_dir_all(&unregistered).unwrap();
        std::fs::copy(first.join(".git"), unregistered.join(".git")).unwrap();
        assert_ne!(
            project_group(&config, &unregistered, "repo", false).path,
            group.path
        );
        let scratch = project_group(&config, &first, "Scratch conversation", true);
        assert_ne!(scratch.path, group.path);
        assert_eq!(scratch.name, "Scratch conversation");
        let missing = temp.path().join("missing/repo");
        assert_eq!(
            project_group(&config, &missing, "repo", false).path,
            missing.to_string_lossy()
        );
    }

    #[test]
    fn model_catalog_cache_is_account_scoped_across_protocol_and_local_updates() {
        let first = format!("2.3.0{}", crate::agent_automation::PROTOCOL_BUILD_SUFFIX);
        let updated = format!("2.3.3{}", crate::agent_automation::PROTOCOL_BUILD_SUFFIX);
        assert_eq!(
            catalog_key(&first, Some("same-account")),
            catalog_key(&updated, Some("same-account"))
        );
        assert_ne!(
            catalog_key(&first, Some("same-account")),
            catalog_key(&updated, Some("different-account"))
        );
        assert_eq!(
            catalog_key(&first, Some("same-account")),
            catalog_key("2.3.3+protocol.other-schema", Some("same-account"))
        );
    }

    #[derive(Clone, Default)]
    struct OwnerCompanionState {
        login_calls: Arc<AtomicUsize>,
        capabilities_calls: Arc<AtomicUsize>,
        capabilities_fail: Arc<AtomicBool>,
        reveal_calls: Arc<AtomicUsize>,
        revealed_url: Arc<std::sync::Mutex<Option<String>>>,
    }

    fn owner_companion_authorized(headers: &axum::http::HeaderMap) -> bool {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            == Some(&format!("Bearer {OWNER_COMPANION_TOKEN}"))
    }

    fn owner_companion_health_value() -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "kind": crate::agent_automation::COMPANION_KIND,
            "build": OWNER_COMPANION_BUILD,
            "status": "idle",
            "authenticated": true,
            "accountFingerprint": "owner_account_1",
            "activeActionId": null
        })
    }

    async fn owner_companion_health(headers: axum::http::HeaderMap) -> Response {
        if !owner_companion_authorized(&headers) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Json(owner_companion_health_value()).into_response()
    }

    async fn owner_companion_login(
        State(state): State<OwnerCompanionState>,
        headers: axum::http::HeaderMap,
    ) -> Response {
        if !owner_companion_authorized(&headers) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        state.login_calls.fetch_add(1, Ordering::SeqCst);
        Json(owner_companion_health_value()).into_response()
    }

    async fn owner_companion_capabilities(
        State(state): State<OwnerCompanionState>,
        headers: axum::http::HeaderMap,
    ) -> Response {
        if !owner_companion_authorized(&headers) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        state.capabilities_calls.fetch_add(1, Ordering::SeqCst);
        if state.capabilities_fail.load(Ordering::SeqCst) {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Json(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "build": OWNER_COMPANION_BUILD,
            "accountFingerprint": "owner_account_1",
            "catalogRevision": "owner_catalog_1",
            "models": [{
                "id": "gpt-6-sol",
                "label": "GPT-6 Sol",
                "efforts": [
                    {"id":"high","label":"High","available":true,"reason":null},
                    {"id":"max","label":"Max","available":false,"reason":"not_available_for_account"}
                ]
            }]
        }))
        .into_response()
    }

    async fn owner_companion_reveal(
        State(state): State<OwnerCompanionState>,
        headers: axum::http::HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        if !owner_companion_authorized(&headers) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        state.reveal_calls.fetch_add(1, Ordering::SeqCst);
        *state.revealed_url.lock().unwrap() = body["url"].as_str().map(str::to_string);
        Json(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "build": OWNER_COMPANION_BUILD,
            "revealed": true,
            "activeActionId": null
        }))
        .into_response()
    }

    async fn spawn_owner_companion(
        root: &FilePath,
    ) -> (PathBuf, OwnerCompanionState, tokio::task::JoinHandle<()>) {
        let state = OwnerCompanionState::default();
        let app = Router::new()
            .route("/v1/health", get(owner_companion_health))
            .route("/v1/session/login", post(owner_companion_login))
            .route(
                "/v1/session/capabilities",
                post(owner_companion_capabilities),
            )
            .route("/v1/window/reveal", post(owner_companion_reveal))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = root.join("browser");
        std::fs::create_dir_all(&directory).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let descriptor = directory.join("descriptor.json");
        std::fs::write(
            &descriptor,
            serde_json::to_vec(&json!({
                "version": PROTOCOL_VERSION,
                "kind": crate::agent_automation::COMPANION_KIND,
                "pid": std::process::id(),
                "endpoint": endpoint,
                "token": OWNER_COMPANION_TOKEN,
                "build": OWNER_COMPANION_BUILD,
                "createdAt": "2026-10-09T12:00:00Z"
            }))
            .unwrap(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&descriptor, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        (descriptor, state, task)
    }

    #[async_trait::async_trait]
    impl AutomationExecutor for OwnerNoopExecutor {
        async fn execute(
            &self,
            _execution: AutomationExecution,
            _events: tokio::sync::mpsc::UnboundedSender<crate::agent_automation::CompanionEvent>,
            _cancellation: CancellationToken,
        ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
            panic!("owner API goal commands must not execute the browser while the agent is online")
        }
    }

    #[test]
    fn owner_credentials_reuse_existing_runtime_format_and_only_update_the_port() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("owner-chat.json");
        let token = crate::auth::generate_internal_bearer_token().unwrap();
        let mut legacy = tempfile::NamedTempFile::new_in(temp.path()).unwrap();
        serde_json::to_writer(&mut legacy, &json!({"port":3120,"token":token})).unwrap();
        legacy.persist(&path).unwrap();

        let restored = load_or_create_runtime(&path, 43123).unwrap();
        assert!(restored.token == token);
        assert_eq!(restored.port, 43123);
        let persisted = read_runtime(&path).unwrap().unwrap();
        assert!(persisted.token == token);
        assert_eq!(persisted.port, 43123);
    }

    #[test]
    fn owner_credentials_concurrent_initialization_chooses_one_token() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("owner-chat.json");
        let barrier = std::sync::Barrier::new(8);
        let tokens = std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..8)
                .map(|offset| {
                    let path = &path;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        load_or_create_runtime(path, 3120 + offset).unwrap().token
                    })
                })
                .collect();
            tasks
                .into_iter()
                .map(|task| task.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(tokens.iter().all(|token| token == &tokens[0]));
        assert!(read_runtime(&path).unwrap().unwrap().token == tokens[0]);
    }

    #[test]
    fn owner_credentials_reject_corruption_without_rotating_or_disclosing_secrets() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("owner-chat.json");
        load_or_create_runtime(&path, 3120).unwrap();
        for contents in [
            Vec::new(),
            b"not json".to_vec(),
            br#"{"port":3120,"token":""}"#.to_vec(),
            br#"{"port":3120,"token":"codexify_short"}"#.to_vec(),
            br#"{"port":"private-marker","token":"private-marker"}"#.to_vec(),
            vec![b' '; 4097],
        ] {
            std::fs::write(&path, &contents).unwrap();
            let error = load_or_create_runtime(&path, 43123).err().unwrap();
            assert!(!format!("{error:#}").contains("private-marker"));
            assert!(std::fs::read(&path).unwrap() == contents);
        }
    }

    #[cfg(unix)]
    #[test]
    fn owner_credentials_are_private_and_reject_unsafe_paths() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("owner-chat.json");
        load_or_create_runtime(&path, 3120).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let contents = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_or_create_runtime(&path, 43123).is_err());
        assert!(std::fs::read(&path).unwrap() == contents);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        load_or_create_runtime(&path, 43123).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let link = temp.path().join("linked.json");
        symlink(&path, &link).unwrap();
        assert!(load_or_create_runtime(&link, 3120).is_err());
        let dangling = temp.path().join("dangling.json");
        symlink(temp.path().join("missing.json"), &dangling).unwrap();
        assert!(load_or_create_runtime(&dangling, 3120).is_err());
        assert!(!temp.path().join("missing.json").exists());
        let directory = temp.path().join("directory.json");
        std::fs::create_dir(&directory).unwrap();
        assert!(load_or_create_runtime(&directory, 3120).is_err());
    }

    #[tokio::test]
    async fn owner_listener_uses_the_configured_port_or_an_ephemeral_default() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(temp.path().into());
        assert_eq!(
            config.markdown_chat.port,
            Some(crate::markdown_chat::DEFAULT_OWNER_CHAT_PORT)
        );

        config.markdown_chat.port = None;
        let ephemeral = bind_owner_listener(&config).await.unwrap();
        assert_ne!(ephemeral.local_addr().unwrap().port(), 0);
        drop(ephemeral);

        let reservation = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let fixed_port = reservation.local_addr().unwrap().port();
        drop(reservation);
        config.markdown_chat.port = Some(fixed_port);

        let fixed = bind_owner_listener(&config).await.unwrap();
        assert_eq!(fixed.local_addr().unwrap().port(), fixed_port);
        let error = bind_owner_listener(&config).await.err().unwrap();
        assert!(
            error
                .to_string()
                .contains(&format!("127.0.0.1:{fixed_port}"))
        );
    }

    #[tokio::test]
    async fn owner_project_edit_preserves_chat_identity_history_and_goal_and_labels_scratch() {
        crate::tls::ensure_crypto_provider();
        let temp = tempfile::tempdir().unwrap();
        let access = temp.path().join("projects");
        let first = access.join("first");
        let second = access.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let mut config = crate::config::default_config(access);

        config.worktrees.mode = crate::types::WorktreeMode::Never;
        config.markdown_chat.enabled = true;
        config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Steer;
        config.memory.dir = Some(temp.path().join("state").to_string_lossy().into_owned());
        let bindings = Arc::new(crate::project_bindings::ProjectBindingStore::new(
            temp.path().join("bindings"),
        ));
        let identity = ConversationIdentity::from_openai_session("owner-project-route").unwrap();
        bindings
            .select_project_root(&config, &identity, first.to_str().unwrap())
            .await
            .unwrap();
        let effective = bindings.effective_config(&config, &identity).unwrap();
        let chats = Arc::new(MarkdownChatStore::default());
        let chat = chats
            .chat(&effective, Some(&identity), &SessionState::new())
            .unwrap();
        chat.append("Keep this original history".into())
            .await
            .unwrap();
        let bytes = std::fs::read(chat.path()).unwrap();
        let id = format!(
            "{:x}",
            Sha256::digest(chat.path().to_string_lossy().as_bytes())
        );
        let store = AgentAutomationStore::for_config(&effective).unwrap();
        let now = crate::markdown_chat::now_ms();
        store
            .set_goal(&identity, "Keep this goal", now - 5000)
            .unwrap();
        store.run_goal(&identity, 1, now - 4000).unwrap();
        store
            .finish_goal(
                &identity,
                1,
                crate::agent_automation::GoalFinish::Completed { summary: None },
                now - 2000,
            )
            .unwrap();
        let scratch = ConversationIdentity::from_openai_session("owner-scratch-route").unwrap();
        bindings
            .select_without_project(&config, &scratch)
            .await
            .unwrap();
        chats
            .chat(
                &bindings.effective_config(&config, &scratch).unwrap(),
                Some(&scratch),
                &SessionState::new(),
            )
            .unwrap()
            .ensure()
            .await
            .unwrap();
        let state = OwnerState {
            project_bindings: bindings.clone(),
            creation_lock: Arc::default(),
            artifacts: Arc::new(ArtifactEgressStore::new_at(
                config.artifact_egress.clone(),
                temp.path().join("artifacts"),
            )),
            continuations: Arc::new(
                ConversationContinuationStore::new(temp.path().join("continuations.json")).unwrap(),
            ),
            config: Arc::new(config),
            chats,
            automation: None,
            open_browser: Arc::new(|_| Ok(())),
            catalog_cache: Arc::default(),
            global: Arc::default(),
            token: "test-token".into(),
        };
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        let client = reqwest::Client::new();
        let listing: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let rows = listing["chats"].as_array().unwrap();
        let original = rows.iter().find(|row| row["id"] == id).unwrap();
        assert_eq!(original["projectGroup"]["path"], original["projectPath"]);
        assert_eq!(original["projectGroup"]["name"], "first");
        assert!(
            rows.iter()
                .any(|row| row["isScratch"] == true && row["workspace"] == "Scratch conversation")
        );
        let body = json!({"expectedPath":original["projectPath"],"path":second});
        let endpoint = format!("{base}/api/chats/{id}/project");
        assert_eq!(
            client
                .post(&endpoint)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let changed = client
            .post(&endpoint)
            .bearer_auth("test-token")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(changed.status(), StatusCode::OK);
        let changed: Value = changed.json().await.unwrap();
        assert_eq!(
            changed["projectPath"],
            second.canonicalize().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(
            client
                .post(&endpoint)
                .bearer_auth("test-token")
                .json(&body)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        let listing: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let row = listing["chats"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap();
        assert_eq!(row["projectPath"], changed["projectPath"]);
        assert_eq!(row["projectGroup"], changed["projectGroup"]);
        assert_eq!(row["projectGroup"]["path"], changed["projectPath"]);
        assert_eq!(row["projectGroup"]["name"], "second");
        assert_eq!(row["goalStatus"], "completed");
        let page: Value = client
            .get(format!("{base}/api/chats/{id}"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let messages = page["_meta"][CHAT_WIDGET_META]["messages"]
            .as_array()
            .unwrap();
        assert!(
            messages
                .iter()
                .any(|message| message["markdown"] == "Keep this original history")
        );
        assert!(messages.iter().any(|message| message["role"] == "goal"
            && message["markdown"] == "Goal completed: Keep this goal"));
        assert_eq!(std::fs::read(chat.path()).unwrap(), bytes);
        server.abort();
    }

    #[tokio::test]
    async fn owner_api_lists_sends_and_reads_the_same_persisted_chat() {
        crate::tls::ensure_crypto_provider();
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(temp.path().into());
        config.markdown_chat.enabled = true;
        config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Steer;
        config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Queue;
        config.memory.dir = Some(temp.path().join("state").display().to_string());
        let identity = ConversationIdentity::from_openai_session("owner-api-test").unwrap();
        let chats = Arc::new(MarkdownChatStore::default());
        let chat = chats
            .chat(&config, Some(&identity), &SessionState::new())
            .unwrap();
        chat.ensure().await.unwrap();
        chat.queue_agent_work(false).await.unwrap();
        let artifacts = Arc::new(ArtifactEgressStore::new_at(
            config.artifact_egress.clone(),
            temp.path().join("artifacts"),
        ));
        let continuations = Arc::new(
            ConversationContinuationStore::new(temp.path().join("continuations-owner-api.json"))
                .unwrap(),
        );
        let state = OwnerState {
            project_bindings: Arc::new(
                crate::project_bindings::ProjectBindingStore::for_current_user(),
            ),
            creation_lock: Arc::default(),
            config: Arc::new(config),
            chats,
            artifacts,
            continuations,
            automation: None,
            open_browser: Arc::new(|_| Ok(())),
            catalog_cache: Arc::default(),
            global: Arc::default(),
            token: "test-token".into(),
        };
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();
        for (path, mime, size) in [
            ("/manifest.webmanifest", "application/manifest+json", 0),
            ("/icons/app-180.png", "image/png", 180),
            ("/icons/app-192.png", "image/png", 192),
            ("/icons/app-512.png", "image/png", 512),
        ] {
            let response = client.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], mime);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert!(
                response.headers()[header::CONTENT_SECURITY_POLICY]
                    .to_str()
                    .unwrap()
                    .contains("manifest-src 'self'")
            );
            let bytes = response.bytes().await.unwrap();
            if size == 0 {
                let manifest: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(manifest["name"], "Codexify");
                assert_eq!(manifest["display"], "standalone");
                assert_eq!(manifest["id"], "/");
                assert!(
                    manifest.get("start_url").is_none(),
                    "retain the existing private launch URL"
                );
                assert!(!String::from_utf8_lossy(&bytes).contains("test-token"));
            } else {
                let image = image::load_from_memory(&bytes).unwrap();
                assert_eq!((image.width(), image.height()), (size, size));
            }
            assert_eq!(
                client
                    .get(format!("{base}{path}"))
                    .header(header::HOST, "other.example")
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            client
                .get(format!("{base}/icons/not-an-icon.png"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .get(format!("{base}/api/settings"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let unauthorized = client
            .get(format!("{base}/api/chats"))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let rebinding = client
            .get(format!("{base}/api/chats"))
            .header(header::HOST, "other.example")
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap();
        assert_eq!(rebinding.status(), StatusCode::FORBIDDEN);
        let list: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list["chats"].as_array().unwrap().len(), 1);
        let id = list["chats"][0]["id"].as_str().unwrap();
        assert_eq!(list["chats"][0]["title"], "Untitled conversation");
        let waiting: Value = client
            .get(format!("{base}/api/chats/{id}"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(waiting["_meta"][CHAT_WIDGET_META]["input_required"], true);
        let sent: Value = client
            .post(format!("{base}/api/chats/{id}/send"))
            .bearer_auth("test-token")
            .json(&json!({"request_id":"owner-send", "message":"Check the build"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(sent["_meta"][CHAT_WIDGET_META]["sent"]["id"], "owner-send");
        assert_eq!(
            sent["_meta"][CHAT_WIDGET_META]["sent"]["tool_call_count"],
            0
        );
        assert!(!chat.queue_input_required().await.unwrap());
        assert!(
            chat.queued_messages().await.unwrap().is_empty(),
            "an answer must send even when Queue is the configured default"
        );
        let same_chat = chat.read(false).await.unwrap();
        assert!(same_chat.text.contains("Check the build"));
        let page: Value = client
            .get(format!("{base}/api/chats/{id}"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            page["_meta"][CHAT_WIDGET_META]["messages"][0]["markdown"],
            "Check the build"
        );
        assert_eq!(
            page["_meta"][CHAT_WIDGET_META]["messages"][0]["tool_call_count"],
            0
        );
        let renamed: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(renamed["chats"][0]["title"], "Check the build");
        assert!(renamed["chats"][0]["lastEntryEnd"].as_u64().unwrap() > 0);
        assert_eq!(renamed["chats"][0]["lastIncomingEntryEnd"], 0);
        for index in 0..55 {
            chat.append(format!("Agent answer {index}")).await.unwrap();
        }
        let latest: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(latest["chats"][0]["title"], "Agent answer 54");
        let before_rename = std::fs::read(chat.path()).unwrap();
        let unauthorized_rename = client
            .post(format!("{base}/api/chats/{id}/title"))
            .json(&json!({"title":"Hidden"}))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized_rename.status(), StatusCode::UNAUTHORIZED);
        let named: Value = client
            .post(format!("{base}/api/chats/{id}/title"))
            .bearer_auth("test-token")
            .json(&json!({"title":"Release checklist"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(named["title"], "Release checklist");
        assert_eq!(named["customTitle"], "Release checklist");
        assert_eq!(std::fs::read(chat.path()).unwrap(), before_rename);
        let reopened = MarkdownChatStore::default()
            .chat_at_path(chat.path().into(), true)
            .unwrap();
        assert_eq!(
            reopened.owner_summary().await.unwrap().title,
            "Release checklist"
        );
        for (query, expected) in [
            ("RELEASE CHECKLIST", true),
            ("check the build", true),
            ("Agent answer 0", true),
            ("[not a regex]", false),
            ("--hidden", false),
        ] {
            let found: Value = client
                .get(format!(
                    "{base}/api/chats?q={}",
                    percent_encoding::utf8_percent_encode(
                        query,
                        percent_encoding::NON_ALPHANUMERIC
                    )
                ))
                .bearer_auth("test-token")
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(
                found["matchedChatIds"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(id)),
                expected,
                "{query}"
            );
            assert_eq!(
                found["chats"].as_array().unwrap().len(),
                1,
                "search must preserve selected chat metadata"
            );
        }
        let invalid_search = client
            .get(format!("{base}/api/chats?q=line%0Abreak"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap();
        assert_eq!(invalid_search.status(), StatusCode::BAD_REQUEST);
        let cleared: Value = client
            .post(format!("{base}/api/chats/{id}/title"))
            .bearer_auth("test-token")
            .json(&json!({"title":"  "}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cleared["title"], "Agent answer 54");
        assert!(cleared["customTitle"].is_null());
        let reopened = MarkdownChatStore::default()
            .chat_at_path(chat.path().into(), true)
            .unwrap();
        assert_eq!(
            reopened.owner_summary().await.unwrap().title,
            "Agent answer 54"
        );
        let unauthorized_markdown = client
            .get(format!("{base}/api/chats/{id}/markdown"))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized_markdown.status(), StatusCode::UNAUTHORIZED);
        let copied = client
            .get(format!("{base}/api/chats/{id}/markdown"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap();
        assert_eq!(copied.status(), StatusCode::OK);
        assert_eq!(
            copied.headers()[header::CONTENT_TYPE],
            "text/markdown; charset=utf-8"
        );
        let copied = copied.text().await.unwrap();
        assert!(copied.starts_with("## You\n\nCheck the build\n\n## Agent\n\nAgent answer 0"));
        assert!(copied.ends_with("## Agent\n\nAgent answer 54"));
        assert_eq!(copied.matches("## Agent\n\n").count(), 55);
        chat.record_agent_call(crate::markdown_chat::now_ms())
            .await
            .unwrap();
        let active: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(active["chats"][0]["totalToolCalls"], 1);
        assert!(active["chats"][0]["lastAgentCallAtMs"].as_u64().unwrap() > 0);
        let unauthorized_organization = client
            .post(format!("{base}/api/chats/{id}/organization"))
            .json(&json!({"pinned":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized_organization.status(), StatusCode::UNAUTHORIZED);
        let malformed_organization = client
            .post(format!("{base}/api/chats/{id}/organization"))
            .bearer_auth("test-token")
            .json(&json!({"pinned":true,"archived":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(malformed_organization.status(), StatusCode::BAD_REQUEST);
        let original = std::fs::read(chat.path()).unwrap();
        let unread = chat.unread_snapshot().await.unwrap();
        let read_url = format!("{base}/api/chats/{id}/read");
        let through = chat.owner_summary().await.unwrap().last_entry_end;
        assert_eq!(chat.owner_summary().await.unwrap().owner_read_through, 0);
        assert_eq!(
            client
                .post(&read_url)
                .json(&json!({"through":through}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        for position in [through, 0, through] {
            let response = client
                .post(&read_url)
                .bearer_auth("test-token")
                .json(&json!({"through":position}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.json::<Value>().await.unwrap()["ownerReadThrough"],
                through
            );
        }
        assert_eq!(
            client
                .post(&read_url)
                .bearer_auth("test-token")
                .json(&json!({"through":u64::MAX}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        let other_browser: Value = reqwest::Client::new()
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(other_browser["chats"][0]["ownerReadThrough"], through);
        assert_eq!(other_browser["chats"][0]["lastEntryEnd"], through);
        assert_eq!(other_browser["chats"][0]["lastIncomingEntryEnd"], through);
        let reopened = MarkdownChatStore::default()
            .chat_at_path(chat.path().into(), true)
            .unwrap();
        assert_eq!(
            reopened.owner_summary().await.unwrap().owner_read_through,
            through
        );
        assert_eq!(chat.unread_snapshot().await.unwrap(), unread);
        assert_eq!(std::fs::read(chat.path()).unwrap(), original);
        let pinned: Value = client
            .post(format!("{base}/api/chats/{id}/organization"))
            .bearer_auth("test-token")
            .json(&json!({"pinned":true}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(pinned["pinnedAtMs"].as_u64().is_some());
        let archived: Value = client
            .post(format!("{base}/api/chats/{id}/organization"))
            .bearer_auth("test-token")
            .json(&json!({"archived":true}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(archived["pinnedAtMs"].is_null());
        assert!(archived["archivedAtMs"].as_u64().is_some());
        let listing: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            listing["chats"][0]["archivedAtMs"],
            archived["archivedAtMs"]
        );
        assert!(listing["chats"][0]["pinnedAtMs"].is_null());
        let cannot_pin = client
            .post(format!("{base}/api/chats/{id}/organization"))
            .bearer_auth("test-token")
            .json(&json!({"pinned":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(cannot_pin.status(), StatusCode::CONFLICT);
        assert_eq!(std::fs::read(chat.path()).unwrap(), original);
        assert_eq!(chat.unread_snapshot().await.unwrap(), unread);
        let failed_send = client
            .post(format!("{base}/api/chats/{id}/send"))
            .bearer_auth("test-token")
            .json(&json!({"request_id":"failed-restore","message":""}))
            .send()
            .await
            .unwrap();
        assert!(!failed_send.status().is_success());
        assert!(chat.owner_summary().await.unwrap().archived_at_ms.is_some());
        let restored: Value = client.post(format!("{base}/api/chats/{id}/send"))
            .bearer_auth("test-token").json(&json!({"request_id":"restore-with-message","message":"Continue this conversation","message_mode":"steer"})).send().await.unwrap().json().await.unwrap();
        assert!(restored["_meta"][CHAT_WIDGET_META]["organization"]["archivedAtMs"].is_null());
        assert!(chat.owner_summary().await.unwrap().archived_at_ms.is_none());
        assert!(chat.owner_summary().await.unwrap().pinned_at_ms.is_none());
        task.abort();
    }

    #[tokio::test]
    async fn completed_sidebar_badge_is_dismissed_by_later_user_messages_and_survives_reopen() {
        crate::tls::ensure_crypto_provider();
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(temp.path().into());
        config.markdown_chat.enabled = true;
        config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Steer;
        config.memory.dir = Some(temp.path().join("state").display().to_string());
        let identity = ConversationIdentity::from_openai_session("completed-sidebar").unwrap();
        let chats = Arc::new(MarkdownChatStore::default());
        let chat = chats
            .chat(&config, Some(&identity), &SessionState::new())
            .unwrap();
        chat.append_user("original".into(), "Complete this task".into())
            .await
            .unwrap();
        chat.append("Task finished".into()).await.unwrap();
        let store = AgentAutomationStore::for_config(&config).unwrap();
        let goal = store
            .set_goal(&identity, "Finish the task", crate::markdown_chat::now_ms())
            .unwrap();
        store
            .run_goal(&identity, goal.revision, crate::markdown_chat::now_ms())
            .unwrap();
        let finish = crate::agent_automation::GoalFinish::Completed {
            summary: Some("Finished".into()),
        };
        let completed = store
            .finish_goal(
                &identity,
                goal.revision,
                finish.clone(),
                crate::markdown_chat::now_ms(),
            )
            .unwrap();
        assert_eq!(
            completed.completed_chat_end,
            Some(std::fs::metadata(chat.path()).unwrap().len())
        );
        let make_state = |chats| OwnerState {
            project_bindings: Arc::new(
                crate::project_bindings::ProjectBindingStore::for_current_user(),
            ),
            creation_lock: Arc::default(),
            config: Arc::new(config.clone()),
            chats,
            artifacts: Arc::new(ArtifactEgressStore::new_at(
                config.artifact_egress.clone(),
                temp.path().join("artifacts"),
            )),
            continuations: Arc::new(
                ConversationContinuationStore::new(temp.path().join("continuations.json")).unwrap(),
            ),
            automation: None,
            open_browser: Arc::new(|_| Ok(())),
            catalog_cache: Arc::default(),
            global: Arc::default(),
            token: "test-token".into(),
        };
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = make_state(chats.clone());
        let server =
            tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
        let client = reqwest::Client::new();
        let listing = || async {
            client
                .get(format!("{base}/api/chats"))
                .bearer_auth("test-token")
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["chats"][0]
                .clone()
        };
        let first = listing().await;
        assert_eq!(first["goalStatus"], "completed");
        assert_eq!(first["goalCompletionDismissed"], false);
        let id = first["id"].as_str().unwrap();
        chat.append("Wrapping up the completion report".into())
            .await
            .unwrap();
        assert_eq!(
            listing().await["goalCompletionDismissed"],
            false,
            "agent updates do not dismiss the completion badge"
        );
        let sent = client
            .post(format!("{base}/api/chats/{id}/send"))
            .bearer_auth("test-token")
            .json(&json!({"request_id":"follow-up","message":"Fix the rendering now"}))
            .send()
            .await
            .unwrap();
        assert!(sent.status().is_success());
        assert_eq!(listing().await["goalCompletionDismissed"], true);
        assert_eq!(
            listing().await["goalStatus"],
            "completed",
            "a follow-up does not restart or stop the completed goal"
        );
        chat.read(true).await.unwrap();
        chat.append("Continuing the follow-up".into())
            .await
            .unwrap();
        assert_eq!(
            listing().await["goalCompletionDismissed"],
            true,
            "read receipts and later agent updates preserve dismissal"
        );
        let replay = store
            .finish_goal(
                &identity,
                goal.revision,
                finish,
                crate::markdown_chat::now_ms(),
            )
            .unwrap();
        assert_eq!(
            replay.completed_chat_end, completed.completed_chat_end,
            "replaying goal_finish must not move the completion boundary"
        );
        assert_eq!(listing().await["goalCompletionDismissed"], true);
        let reopened = MarkdownChatStore::default()
            .chat(&config, Some(&identity), &SessionState::new())
            .unwrap();
        let summary = reopened.owner_summary().await.unwrap();
        assert!(summary.last_user_entry_end > replay.completed_chat_end.unwrap());
        let fresh_listing = list_chats(
            State(make_state(Arc::new(MarkdownChatStore::default()))),
            Query(ListQuery::default()),
        )
        .await
        .unwrap();
        assert_eq!(fresh_listing.0["chats"][0]["goalCompletionDismissed"], true);

        // Older saved goals have timestamps but no transcript boundary.
        let path = store
            .task_directory(&identity)
            .unwrap()
            .join("automation.json");
        let mut legacy: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        legacy["goal"]
            .as_object_mut()
            .unwrap()
            .remove("completedChatEnd");
        legacy["goal"]["finishedAtMs"] = json!(summary.last_user_entry_at_ms.unwrap() - 1);
        std::fs::write(path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(listing().await["goalCompletionDismissed"], true);
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(chat.path())
                .unwrap();
            file.write_all(b"\nAnother manual follow-up\n").unwrap();
            file.sync_all().unwrap();
        }
        assert_eq!(
            listing().await["goalCompletionDismissed"],
            true,
            "an untimestamped append cannot revive a dismissed legacy badge"
        );

        let replacement = store
            .set_goal(&identity, "New goal", crate::markdown_chat::now_ms())
            .unwrap();
        store
            .run_goal(
                &identity,
                replacement.revision,
                crate::markdown_chat::now_ms(),
            )
            .unwrap();
        assert_eq!(listing().await["goalStatus"], "active");
        let completed = store
            .finish_goal(
                &identity,
                replacement.revision,
                crate::agent_automation::GoalFinish::Completed { summary: None },
                crate::markdown_chat::now_ms(),
            )
            .unwrap();
        assert!(completed.completed_chat_end.is_some());
        assert_eq!(
            listing().await["goalCompletionDismissed"],
            false,
            "a new completed goal gets a new badge"
        );
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(chat.path())
                .unwrap();
            file.write_all(b"\nManual follow-up without a timestamp\n")
                .unwrap();
            file.sync_all().unwrap();
        }
        assert_eq!(
            listing().await["goalCompletionDismissed"],
            true,
            "manual chat appends also dismiss the badge"
        );
        store.clear_goal(&identity, replacement.revision).unwrap();
        assert!(listing().await["goalStatus"].is_null());
        assert!(listing().await["automationError"].is_null());
        let pairing = store
            .issue_pairing(&identity, "https://chatgpt.com/c/sidebar-error", 0, 1)
            .unwrap();
        store
            .record_pairing_canonical_url(
                &identity,
                pairing.generation,
                "https://chatgpt.com/c/sidebar-error",
                2,
            )
            .unwrap();
        store
            .verify_pairing(&identity, pairing.generation, 0, &pairing.reference, 3)
            .unwrap();
        store
            .queue_action(
                &identity,
                "sidebar-error",
                crate::agent_automation::AutomationActionKind::OfflineChat,
                pairing.generation,
                None,
                4,
            )
            .unwrap();
        store
            .update_action_status(
                &identity,
                "sidebar-error",
                crate::agent_automation::AutomationActionStatus::Failed,
                Some("Connector menu unavailable"),
                None,
                5,
            )
            .unwrap();
        assert_eq!(
            listing().await["automationError"],
            "Connector menu unavailable",
            "ordinary chats expose browser failures in the sidebar"
        );
        store
            .reset_failed_action_for_retry(&identity, "sidebar-error", 6)
            .unwrap();
        store
            .queue_action(
                &identity,
                "sidebar-retry",
                crate::agent_automation::AutomationActionKind::OfflineChat,
                pairing.generation,
                None,
                7,
            )
            .unwrap();
        assert!(
            listing().await["automationError"].is_null(),
            "retrying clears the stale error indicator"
        );
        assert_eq!(listing().await["automationWaitingForBrowser"], false);
        let until = crate::markdown_chat::now_ms() + 60_000;
        assert!(
            store
                .defer_action_for_cooldown(&identity, "sidebar-retry", "sidebar-cooldown", until)
                .unwrap()
        );
        let waiting = listing().await;
        assert_eq!(waiting["automationWaitingForBrowser"], true);
        assert!(waiting["automationError"].is_null());
        assert_eq!(waiting["automationRetrying"], false);
        store
            .resume_cooldown_action(&identity, "sidebar-cooldown", until)
            .unwrap();
        assert_eq!(listing().await["automationWaitingForBrowser"], false);
        server.abort();
    }

    #[tokio::test]
    async fn owner_goal_commands_are_authenticated_idempotent_local_and_visible_in_state() {
        crate::tls::ensure_crypto_provider();
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(temp.path().join("project"));
        std::fs::create_dir_all(&config.work_dir).unwrap();
        config.markdown_chat.enabled = true;
        config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Steer;
        config.markdown_chat.automation.enabled = true;
        config.markdown_chat.automation.connector_name = "Codexify".into();
        config.memory.dir = Some(temp.path().join("state").display().to_string());
        let identity = ConversationIdentity::from_openai_session("owner-goal-api").unwrap();
        let chats = Arc::new(MarkdownChatStore::default());
        let chat = chats
            .chat(&config, Some(&identity), &SessionState::new())
            .unwrap();
        chat.ensure().await.unwrap();
        let artifacts = Arc::new(ArtifactEgressStore::new_at(
            config.artifact_egress.clone(),
            temp.path().join("artifacts"),
        ));
        let continuations = Arc::new(
            crate::conversation_continuations::ConversationContinuationStore::new(
                temp.path().join("continuations.json"),
            )
            .unwrap(),
        );
        let supervisor = crate::agent_automation::AgentAutomationSupervisor::new(
            config.markdown_chat.automation.clone(),
            Arc::new(OwnerNoopExecutor),
        );
        let state = OwnerState {
            project_bindings: Arc::new(
                crate::project_bindings::ProjectBindingStore::for_current_user(),
            ),
            creation_lock: Arc::default(),
            config: Arc::new(config),
            chats,
            artifacts,
            continuations,
            automation: Some(supervisor),
            open_browser: Arc::new(|_| Ok(())),
            catalog_cache: Arc::default(),
            global: Arc::default(),
            token: "test-token".into(),
        };
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();
        let list: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = list["chats"][0]["id"].as_str().unwrap();

        let set_goal = || {
            client
                .post(format!("{base}/api/chats/{id}/send"))
                .bearer_auth("test-token")
                .json(&json!({
                    "request_id":"goal-set-request",
                    "message":"/goal set Finish the owner automation API."
                }))
                .send()
        };
        let first: Value = set_goal().await.unwrap().json().await.unwrap();
        let repeated: Value = set_goal().await.unwrap().json().await.unwrap();
        assert_eq!(first["_meta"][CHAT_WIDGET_META]["goal"]["revision"], 1);
        assert_eq!(repeated["_meta"][CHAT_WIDGET_META]["goal"]["revision"], 1);
        assert!(chat.clean_markdown_transcript().await.unwrap().is_empty());

        let run: Value = client
            .post(format!("{base}/api/chats/{id}/send"))
            .bearer_auth("test-token")
            .json(&json!({"request_id":"goal-run-request", "message":"/goal run"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(run["_meta"][CHAT_WIDGET_META]["goal"]["status"], "active");

        let automation: Value = client
            .get(format!("{base}/api/chats/{id}/automation"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(automation["enabled"], true);
        assert_eq!(automation["goal"]["revision"], 1);
        assert_eq!(automation["goal"]["status"], "active");
        assert_eq!(automation["pairing"]["status"], "unpaired");
        let listed: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed["chats"][0]["goalStatus"], "active");
        assert_eq!(listed["automationEnabled"], true);
        assert_eq!(listed["chats"][0]["goal"], automation["goal"]);
        let serialized = automation.to_string();
        assert!(!serialized.contains("descriptorPath"));
        assert!(!serialized.contains("test-token"));
        assert!(!serialized.contains("Bearer"));

        chat.record_agent_call(crate::markdown_chat::now_ms())
            .await
            .unwrap();
        let edit_goal = || {
            client
            .post(format!("{base}/api/chats/{id}/send"))
            .bearer_auth("test-token")
            .json(&json!({"request_id":"edit-active-goal", "message":"/goal set Finish the updated goal while this turn continues."}))
            .send()
        };
        let edited: Value = edit_goal().await.unwrap().json().await.unwrap();
        assert_eq!(edited["_meta"][CHAT_WIDGET_META]["goal"]["revision"], 2);
        assert_eq!(
            edited["_meta"][CHAT_WIDGET_META]["goal"]["status"],
            "active"
        );
        let pending = chat.unread_snapshot().await.unwrap();
        assert_eq!(pending.messages.len(), 1);
        assert!(pending.messages[0].id.starts_with("goal-update-"));
        let visible: Value = client
            .get(format!("{base}/api/chats/{id}"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            visible["_meta"][CHAT_WIDGET_META]["messages"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(chat.clean_markdown_transcript().await.unwrap().is_empty());
        let notice = chat.read(true).await.unwrap().text;
        assert!(notice.contains("The goal has changed (revision 2)"));
        assert!(notice.contains("Finish the updated goal while this turn continues."));
        assert!(notice.contains("`goal_read`"));
        let repeated_edit: Value = edit_goal().await.unwrap().json().await.unwrap();
        assert_eq!(
            repeated_edit["_meta"][CHAT_WIDGET_META]["goal"]["revision"],
            2
        );
        assert!(chat.unread_snapshot().await.unwrap().messages.is_empty());
        assert_eq!(
            std::fs::read_to_string(chat.path())
                .unwrap()
                .matches("The goal has changed")
                .count(),
            1
        );

        let literal: Value = client
            .post(format!("{base}/api/chats/{id}/send"))
            .bearer_auth("test-token")
            .json(&json!({"request_id":"literal-goal", "message":"//goal status"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            literal["_meta"][CHAT_WIDGET_META]["sent"]["id"],
            "literal-goal"
        );
        assert!(
            chat.clean_markdown_transcript()
                .await
                .unwrap()
                .contains("/goal status")
        );
        let stop = || {
            client
                .post(format!("{base}/api/chats/{id}/send"))
                .bearer_auth("test-token")
                .json(&json!({"request_id":"stop-turn", "message":"ignored draft", "stop_agent":true}))
                .send()
        };
        let stopped: Value = stop().await.unwrap().json().await.unwrap();
        assert_eq!(stopped["_meta"][CHAT_WIDGET_META]["stopped"], true);
        assert_eq!(
            stopped["_meta"][CHAT_WIDGET_META]["goal"]["status"],
            "configured"
        );
        assert!(chat.agent_stop_requested().await.unwrap());
        assert!(
            !chat
                .clean_markdown_transcript()
                .await
                .unwrap()
                .contains("ignored draft")
        );
        let sent: Value = client
            .post(format!("{base}/api/chats/{id}/send"))
            .bearer_auth("test-token")
            .json(&json!({"request_id":"after-stop", "message":"An independent message"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(sent["_meta"][CHAT_WIDGET_META]["sent"].is_object());
        assert!(!chat.agent_stop_requested().await.unwrap());
        let paused: Value = client
            .get(format!("{base}/api/chats/{id}/automation"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            paused["goal"]["status"], "configured",
            "sending must not enable the goal"
        );
        client
            .post(format!("{base}/api/chats/{id}/send"))
            .bearer_auth("test-token")
            .json(&json!({"request_id":"play-after-stop", "message":"/goal run"}))
            .send()
            .await
            .unwrap();
        stop().await.unwrap();
        let active: Value = client
            .get(format!("{base}/api/chats/{id}/automation"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            active["goal"]["status"], "active",
            "repeating an old stop must not undo Play"
        );
        assert!(!chat.agent_stop_requested().await.unwrap());
        client
            .post(format!("{base}/api/chats/{id}/send"))
            .bearer_auth("test-token")
            .json(&json!({"request_id":"while-active", "message":"An additional instruction"}))
            .send()
            .await
            .unwrap();
        let active: Value = client
            .get(format!("{base}/api/chats/{id}/automation"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            active["goal"]["status"], "active",
            "sending must not stop an enabled goal"
        );

        chat.record_agent_call(crate::markdown_chat::now_ms())
            .await
            .unwrap();
        client
            .post(format!("{base}/api/chats/{id}/send"))
            .bearer_auth("test-token")
            .json(&json!({"request_id":"stop-before-new-goal", "message":"", "stop_agent":true}))
            .send()
            .await
            .unwrap();
        assert!(chat.agent_stop_requested().await.unwrap());
        let replacement_goal = || {
            client
                .post(format!("{base}/api/chats/{id}/send"))
                .bearer_auth("test-token")
                .json(&json!({
                    "request_id":"replacement-goal",
                    "message":"/goal set Complete the replacement goal."
                }))
                .send()
        };
        let replacement: Value = replacement_goal().await.unwrap().json().await.unwrap();
        assert_eq!(
            replacement["_meta"][CHAT_WIDGET_META]["goal"]["revision"],
            3
        );
        assert_eq!(
            replacement["_meta"][CHAT_WIDGET_META]["goal"]["status"],
            "active"
        );
        assert!(
            !chat.agent_stop_requested().await.unwrap(),
            "setting a new active goal must clear an earlier agent stop"
        );
        assert!(
            chat.offline_activity()
                .await
                .unwrap()
                .last_call_at_ms
                .is_none(),
            "the new goal must be eligible for immediate submission after Stop"
        );

        client
            .post(format!("{base}/api/chats/{id}/send"))
            .bearer_auth("test-token")
            .json(&json!({"request_id":"stop-replacement-goal", "message":"", "stop_agent":true}))
            .send()
            .await
            .unwrap();
        let replayed: Value = replacement_goal().await.unwrap().json().await.unwrap();
        assert_eq!(replayed["_meta"][CHAT_WIDGET_META]["replayed"], true);
        assert!(
            chat.agent_stop_requested().await.unwrap(),
            "replaying an old Set must not undo a later Stop"
        );
        let paused: Value = client
            .get(format!("{base}/api/chats/{id}/automation"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(paused["goal"]["revision"], 3);
        assert_eq!(paused["goal"]["status"], "configured");
        let listed: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed["chats"][0]["goalStatus"], "configured");
        client
            .post(format!("{base}/api/chats/{id}/send"))
            .bearer_auth("test-token")
            .json(&json!({"request_id":"clear-goal-for-sidebar", "message":"/goal clear"}))
            .send()
            .await
            .unwrap();
        let listed: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(listed["chats"][0]["goalStatus"].is_null());
        assert!(listed["chats"][0]["goal"].is_null());
        task.abort();
    }

    #[test]
    fn public_pairing_prefills_only_the_last_known_conversation_url() {
        let mut pairing = crate::agent_automation::PairingState {
            creating_chat: true,
            candidate_url: Some("https://chatgpt.com/g/g-p-project/project".into()),
            ..crate::agent_automation::PairingState::default()
        };
        assert!(public_pairing(&pairing, 0)["lastTriedUrl"].is_null());
        pairing.canonical_url = Some("https://chatgpt.com/c/created".into());
        assert_eq!(
            public_pairing(&pairing, 0)["lastTriedUrl"],
            "https://chatgpt.com/c/created"
        );
        pairing.last_attempted_url = Some("https://chatgpt.com/g/alias/c/attempted".into());
        assert_eq!(
            public_pairing(&pairing, 0)["lastTriedUrl"],
            "https://chatgpt.com/g/alias/c/attempted"
        );
    }

    #[tokio::test]
    async fn owner_automation_management_keeps_companion_credentials_server_side() {
        crate::tls::ensure_crypto_provider();
        let temp = tempfile::tempdir().unwrap();
        let (descriptor, companion_state, companion_task) =
            spawn_owner_companion(temp.path()).await;
        let bearer = OWNER_COMPANION_TOKEN;

        let mut config = crate::config::default_config(temp.path().join("project"));
        std::fs::create_dir_all(&config.work_dir).unwrap();
        config.markdown_chat.enabled = true;
        config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Steer;
        config.markdown_chat.automation.enabled = true;
        config.markdown_chat.automation.connector_name = "Codexify".into();
        config.markdown_chat.automation.descriptor_path = descriptor.display().to_string();
        config.memory.dir = Some(temp.path().join("state").display().to_string());
        let identity = ConversationIdentity::from_openai_session("owner-management-api").unwrap();
        let chats = Arc::new(MarkdownChatStore::default());
        let chat = chats
            .chat(&config, Some(&identity), &SessionState::new())
            .unwrap();
        let artifacts = Arc::new(ArtifactEgressStore::new_at(
            config.artifact_egress.clone(),
            temp.path().join("artifacts"),
        ));
        let continuations = Arc::new(
            ConversationContinuationStore::new(temp.path().join("continuations-management.json"))
                .unwrap(),
        );
        let supervisor = crate::agent_automation::AgentAutomationSupervisor::new(
            config.markdown_chat.automation.clone(),
            Arc::new(OwnerNoopExecutor),
        );
        let opened_urls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record_urls = opened_urls.clone();
        let state = OwnerState {
            project_bindings: Arc::new(
                crate::project_bindings::ProjectBindingStore::for_current_user(),
            ),
            creation_lock: Arc::default(),
            config: Arc::new(config),
            chats,
            artifacts,
            continuations,
            automation: Some(supervisor.clone()),
            open_browser: Arc::new(move |url| {
                record_urls.lock().unwrap().push(url.to_owned());
                Ok(())
            }),
            catalog_cache: Arc::default(),
            global: Arc::default(),
            token: "test-token".into(),
        };
        let open_state = state.clone();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let owner_task = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();
        let empty: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(empty["chats"].as_array().unwrap().is_empty());
        for route in [
            "/api/browser",
            "/api/browser/login",
            "/api/browser/capabilities",
        ] {
            let request = if route == "/api/browser" {
                client.get(format!("{base}{route}"))
            } else {
                client.post(format!("{base}{route}")).json(&json!({}))
            };
            assert_eq!(
                request.send().await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
        }
        let browser: Value = client
            .get(format!("{base}/api/browser"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(browser["companion"]["authenticated"], true);
        assert!(!browser.to_string().contains(bearer));
        let models: Value = client
            .post(format!("{base}/api/browser/capabilities"))
            .bearer_auth("test-token")
            .json(&json!({"refresh":true}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(models["models"][0]["id"], "gpt-6-sol");
        assert!(models.get("accountFingerprint").is_none());
        let login: Value = client
            .post(format!("{base}/api/browser/login"))
            .bearer_auth("test-token")
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(login["companion"]["authenticated"], true);

        let still_empty: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            still_empty["chats"].as_array().unwrap().is_empty(),
            "global browser controls must not create a conversation"
        );
        chat.ensure().await.unwrap();
        let list: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = list["chats"][0]["id"].as_str().unwrap();

        let unauthorized_open = client
            .post(format!("{base}/api/chats/{id}/automation/reveal"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized_open.status(), StatusCode::UNAUTHORIZED);
        let unpaired_open = client
            .post(format!("{base}/api/chats/{id}/automation/reveal"))
            .bearer_auth("test-token")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(unpaired_open.status(), StatusCode::CONFLICT);
        assert!(opened_urls.lock().unwrap().is_empty());

        let pair_without_selection: Value = client.post(format!("{base}/api/chats/{id}/automation/pair")).bearer_auth("test-token").json(&json!({"requestId":"pair-no-selection","url":"https://chatgpt.com/c/no-selection"})).send().await.unwrap().json().await.unwrap();
        assert_eq!(pair_without_selection["pairing"]["status"], "unverified");
        assert_eq!(supervisor.queued_count().await, 1);
        client
            .post(format!("{base}/api/chats/{id}/automation/unlink"))
            .bearer_auth("test-token")
            .json(&json!({"requestId":"unlink-no-selection"}))
            .send()
            .await
            .unwrap();
        assert_eq!(supervisor.queued_count().await, 0);

        let unauthorized = client
            .post(format!("{base}/api/chats/{id}/automation/capabilities"))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let capabilities_response = client
            .post(format!("{base}/api/chats/{id}/automation/capabilities"))
            .bearer_auth("test-token")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        let capabilities_status = capabilities_response.status();
        let capabilities_body = capabilities_response.text().await.unwrap();
        assert_eq!(capabilities_status, StatusCode::OK, "{capabilities_body}");
        let capabilities: Value = serde_json::from_str(&capabilities_body).unwrap();
        assert_eq!(capabilities["catalogRevision"], "owner_catalog_1");
        assert_eq!(capabilities["models"][0]["id"], "gpt-6-sol");
        assert!(capabilities.get("accountFingerprint").is_none());

        let cached_state: Value = client
            .get(format!("{base}/api/chats/{id}/automation"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cached_state["catalog"]["models"][0]["id"], "gpt-6-sol");
        let cached_again: Value = client
            .post(format!("{base}/api/chats/{id}/automation/capabilities"))
            .bearer_auth("test-token")
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cached_again["catalogRevision"], "owner_catalog_1");
        assert_eq!(companion_state.capabilities_calls.load(Ordering::SeqCst), 1);

        let selection: Value = client
            .post(format!("{base}/api/chats/{id}/automation/selection"))
            .bearer_auth("test-token")
            .json(&json!({
                "requestId":"selection-1",
                "model":"gpt-6-sol",
                "effort":"high"
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(selection["selection"]["model"], "gpt-6-sol");
        assert_eq!(selection["selection"]["effort"], "high");
        assert!(selection.to_string().find("owner_account_1").is_none());

        let paired: Value = client
            .post(format!("{base}/api/chats/{id}/automation/pair"))
            .bearer_auth("test-token")
            .json(&json!({
                "requestId":"pair-1",
                "url":"https://chatgpt.com/c/owner-management"
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(paired["pairing"]["status"], "unverified");
        assert_eq!(paired["action"]["status"], "queued");
        assert!(paired.get("reference").is_none());
        assert_eq!(supervisor.queued_count().await, 1);

        let queued: Value = client
            .get(format!("{base}/api/chats/{id}/automation"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(queued["queue"]["position"], 1);
        assert_eq!(queued["queue"]["total"], 1);

        let cancelled: Value = client
            .post(format!("{base}/api/chats/{id}/automation/action/cancel"))
            .bearer_auth("test-token")
            .json(&json!({
                "requestId":"cancel-pair-1",
                "actionId":paired["action"]["id"]
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cancelled["action"]["status"], "unpaired");
        assert!(cancelled["queue"].is_null());
        assert_eq!(supervisor.queued_count().await, 0);

        let paired_again: Value = client
            .post(format!("{base}/api/chats/{id}/automation/pair"))
            .bearer_auth("test-token")
            .json(&json!({
                "requestId":"pair-2",
                "url":"https://chatgpt.com/c/owner-management"
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(paired_again["pairing"]["status"], "unverified");
        assert_eq!(supervisor.queued_count().await, 1);

        let action_status: Value = client
            .get(format!("{base}/api/chats/{id}/automation/action"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(action_status["action"]["status"], "queued");
        assert_eq!(action_status["queue"]["position"], 1);

        let revealed: Value = client
            .post(format!("{base}/api/chats/{id}/automation/reveal"))
            .bearer_auth("test-token")
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(revealed["opened"], true);
        assert_eq!(
            *opened_urls.lock().unwrap(),
            ["https://chatgpt.com/c/owner-management"]
        );
        assert_eq!(supervisor.queued_count().await, 1);

        // Prefer the canonical project URL and do not depend on a companion or supervisor.
        let located = selected(&open_state, id).unwrap();
        let store =
            AgentAutomationStore::for_state_root(&open_state.config, &located.state_root).unwrap();
        let task = automation_task(&located).unwrap();
        let project_url = "https://chatgpt.com/g/g-p-owner-project/c/owner-management";
        store
            .record_pairing_canonical_url(
                &task,
                paired_again["pairing"]["generation"].as_u64().unwrap(),
                project_url,
                crate::markdown_chat::now_ms(),
            )
            .unwrap();
        let saved_before_open = store.load(&task).unwrap();
        let mut disconnected_config = (*open_state.config).clone();
        disconnected_config.markdown_chat.automation.descriptor_path = temp
            .path()
            .join("missing-descriptor.json")
            .display()
            .to_string();
        let disconnected_state = OwnerState {
            config: Arc::new(disconnected_config),
            automation: None,
            ..open_state.clone()
        };
        let Json(opened) = automation_reveal(
            State(disconnected_state.clone()),
            Path(id.to_owned()),
            Json(json!({})),
        )
        .await
        .unwrap();
        assert_eq!(opened["opened"], true);
        assert_eq!(
            *opened_urls.lock().unwrap(),
            ["https://chatgpt.com/c/owner-management", project_url]
        );
        assert_eq!(
            serde_json::to_value(store.load(&task).unwrap()).unwrap(),
            serde_json::to_value(saved_before_open).unwrap()
        );
        assert_eq!(supervisor.queued_count().await, 1);
        let failing_state = OwnerState {
            open_browser: Arc::new(|_| Err(std::io::Error::other("test opener failure"))),
            ..disconnected_state
        };
        let error = automation_reveal(State(failing_state), Path(id.to_owned()), Json(json!({})))
            .await
            .err()
            .unwrap();
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert!(error.1.contains("Could not open the default browser"));
        assert_eq!(opened_urls.lock().unwrap().len(), 2);

        let unlinked: Value = client
            .post(format!("{base}/api/chats/{id}/automation/unlink"))
            .bearer_auth("test-token")
            .json(&json!({"requestId":"unlink-1"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(unlinked["pairing"]["status"], "unpaired");
        assert_eq!(supervisor.queued_count().await, 0);

        let joined = serde_json::to_string(&[
            capabilities,
            login,
            selection,
            paired,
            cancelled,
            paired_again,
            action_status,
            revealed,
            unlinked,
        ])
        .unwrap();
        assert!(!joined.contains(bearer));
        assert!(!joined.contains("owner_account_1"));
        assert!(!joined.contains(&descriptor.display().to_string()));
        assert_eq!(companion_state.login_calls.load(Ordering::SeqCst), 1);
        assert_eq!(companion_state.capabilities_calls.load(Ordering::SeqCst), 1);
        assert_eq!(companion_state.reveal_calls.load(Ordering::SeqCst), 0);
        for age in [0, 240_000] {
            chat.record_agent_call(crate::markdown_chat::now_ms() - age)
                .await
                .unwrap();
            let selected = client
                .post(format!("{base}/api/chats/{id}/automation/selection"))
                .bearer_auth("test-token")
                .json(
                    &json!({"requestId":"online-selection", "model":"gpt-6-sol", "effort":"high"}),
                )
                .send()
                .await
                .unwrap();
            assert_eq!(selected.status(), StatusCode::OK);
        }
        assert_eq!(
            companion_state.capabilities_calls.load(Ordering::SeqCst),
            1,
            "online preferences must use the cached catalog without changing the browser"
        );
        let mut restarted = open_state.clone();
        restarted.catalog_cache = Arc::default();
        let (health, restored) = owner_catalog(&restarted, false).await.unwrap();
        assert_eq!(restored.catalog_revision, "owner_catalog_1");
        assert_eq!(companion_state.capabilities_calls.load(Ordering::SeqCst), 1);
        let refresh_lock = restarted.catalog_cache.lock().await.refresh_lock.clone();
        let held_refresh = refresh_lock.lock().await;
        let cached = tokio::time::timeout(Duration::from_secs(1), owner_catalog(&restarted, false))
            .await
            .expect("cached preference reads must not wait for browser discovery")
            .unwrap();
        assert_eq!(cached.1.models, restored.models);
        drop(held_refresh);
        let mut legacy_catalog = restored.clone();
        legacy_catalog.build = format!("2.3.0+protocol.{}", "0".repeat(64));
        let legacy = OwnerCatalogCache {
            key: hashed_catalog_key(
                legacy_catalog.build.split_once("+protocol.").unwrap().1,
                Some(&legacy_catalog.account_fingerprint),
            ),
            catalog: legacy_catalog,
        };
        write_catalog_cache(&restarted, &legacy).unwrap();
        restarted.catalog_cache = Arc::default();
        let (_, migrated) = owner_catalog(&restarted, false).await.unwrap();
        assert_ne!(migrated.build, health.build);
        assert_eq!(migrated.models, restored.models);
        assert_eq!(
            companion_state.capabilities_calls.load(Ordering::SeqCst),
            1,
            "a compatible update restores the legacy disk cache without browser discovery"
        );
        let invalid = OwnerCatalogCache {
            key: "unrelated-cache-key".into(),
            catalog: migrated,
        };
        write_catalog_cache(&restarted, &invalid).unwrap();
        assert!(read_catalog_cache(&restarted, &catalog_context(&health)).is_none());
        write_catalog_cache(&restarted, &legacy).unwrap();
        assert!(
            cached_owner_catalog(
                &restarted,
                &catalog_key(&health.build, Some("other-account"))
            )
            .await
            .is_none()
        );
        let cache_path =
            BrowserCompanionClient::catalog_cache_path(&restarted.config.markdown_chat.automation)
                .unwrap();
        let cached_bytes = std::fs::read(&cache_path).unwrap();
        restarted.catalog_cache = Arc::default();
        companion_state
            .capabilities_fail
            .store(true, Ordering::SeqCst);
        let now = crate::markdown_chat::now_ms();
        assert!(
            refresh_owner_catalog_if_due(&restarted, &health, now).await,
            "startup attempts one refresh even with a recent disk cache"
        );
        assert_eq!(companion_state.capabilities_calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            std::fs::read(&cache_path).unwrap(),
            cached_bytes,
            "failed refreshes preserve the last successful disk cache"
        );
        let (_, retained) = owner_catalog(&restarted, false).await.unwrap();
        assert_eq!(retained.models, restored.models);
        assert!(
            !refresh_owner_catalog_if_due(&restarted, &health, now + MODEL_CATALOG_REFRESH_MS - 1)
                .await
        );
        let mut busy = health.clone();
        busy.active_action_id = Some("another-chat-action".into());
        assert!(
            !refresh_owner_catalog_if_due(&restarted, &busy, now + MODEL_CATALOG_REFRESH_MS).await
        );
        companion_state
            .capabilities_fail
            .store(false, Ordering::SeqCst);
        assert!(
            refresh_owner_catalog_if_due(&restarted, &health, now + MODEL_CATALOG_REFRESH_MS).await
        );
        assert_eq!(companion_state.capabilities_calls.load(Ordering::SeqCst), 3);
        assert!(
            !refresh_owner_catalog_if_due(&restarted, &health, now + MODEL_CATALOG_REFRESH_MS + 1)
                .await
        );
        std::fs::remove_file(cache_path).unwrap();
        restarted.catalog_cache = Arc::default();
        let (first, second) = tokio::join!(
            owner_catalog(&restarted, false),
            owner_catalog(&restarted, false)
        );
        assert_eq!(first.unwrap().1.models, second.unwrap().1.models);
        assert_eq!(
            companion_state.capabilities_calls.load(Ordering::SeqCst),
            4,
            "simultaneous cache misses share one discovery"
        );
        owner_task.abort();
        companion_task.abort();
    }

    #[tokio::test]
    async fn owner_action_retry_and_cancel_preserve_unread_chat_until_tool_delivery() {
        crate::tls::ensure_crypto_provider();
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(temp.path().join("project"));
        std::fs::create_dir_all(&config.work_dir).unwrap();
        config.markdown_chat.enabled = true;
        config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Steer;
        config.markdown_chat.automation.enabled = true;
        config.markdown_chat.automation.connector_name = "Codexify".into();
        config.markdown_chat.automation.descriptor_path = temp
            .path()
            .join("browser/descriptor.json")
            .display()
            .to_string();
        config.memory.dir = Some(temp.path().join("state").display().to_string());
        let identity = ConversationIdentity::from_openai_session("owner-action-api").unwrap();
        let chats = Arc::new(MarkdownChatStore::default());
        let chat = chats
            .chat(&config, Some(&identity), &SessionState::new())
            .unwrap();
        chat.ensure().await.unwrap();
        let now_ms = crate::markdown_chat::now_ms();
        chat.record_agent_call(now_ms.saturating_sub(crate::markdown_chat::OFFLINE_AFTER_MS + 1))
            .await
            .unwrap();
        chat.append_user(
            "owner-action-message".into(),
            "Keep this pending through cancel and retry.".into(),
        )
        .await
        .unwrap();
        let store = Arc::new(AgentAutomationStore::for_config(&config).unwrap());
        store
            .set_selection(
                &identity,
                "gpt-6-sol",
                "high",
                "owner-catalog",
                "owner-account",
                now_ms,
            )
            .unwrap();
        let pairing = store
            .issue_pairing(&identity, "https://chatgpt.com/c/owner-action", 0, now_ms)
            .unwrap();
        store
            .record_pairing_canonical_url(
                &identity,
                pairing.generation,
                "https://chatgpt.com/c/owner-action",
                now_ms,
            )
            .unwrap();
        store
            .verify_pairing(&identity, pairing.generation, 0, &pairing.reference, now_ms)
            .unwrap();
        let supervisor = AgentAutomationSupervisor::new(
            config.markdown_chat.automation.clone(),
            Arc::new(OwnerNoopExecutor),
        );
        supervisor
            .register_task(AutomationTaskHandle::new(
                identity.clone(),
                0,
                store.clone(),
                chat.clone(),
            ))
            .await
            .unwrap();
        supervisor.scan_once(now_ms).await.unwrap();
        let failed_action = store.load(&identity).unwrap().action.id.unwrap();
        supervisor.discard_queued_actions(&identity).await;
        store
            .update_action_status(
                &identity,
                &failed_action,
                crate::agent_automation::AutomationActionStatus::Failed,
                Some("Selector verification failed before Send"),
                None,
                now_ms,
            )
            .unwrap();

        let artifacts = Arc::new(ArtifactEgressStore::new_at(
            config.artifact_egress.clone(),
            temp.path().join("artifacts"),
        ));
        let continuations = Arc::new(
            ConversationContinuationStore::new(temp.path().join("continuations-actions.json"))
                .unwrap(),
        );
        let state = OwnerState {
            project_bindings: Arc::new(
                crate::project_bindings::ProjectBindingStore::for_current_user(),
            ),
            creation_lock: Arc::default(),
            config: Arc::new(config),
            chats,
            artifacts,
            continuations,
            automation: Some(supervisor.clone()),
            open_browser: Arc::new(|_| Ok(())),
            catalog_cache: Arc::default(),
            global: Arc::default(),
            token: "test-token".into(),
        };
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let owner_task = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();
        let list: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = list["chats"][0]["id"].as_str().unwrap();

        assert_eq!(list["chats"][0]["automationRetrying"], true);

        let dismissed: Value = client
            .post(format!("{base}/api/chats/{id}/automation/action/cancel"))
            .bearer_auth("test-token")
            .json(&json!({"requestId":"cancel-failed-owner-action","actionId":failed_action}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(dismissed["action"]["status"], "cancelled");
        assert!(dismissed["action"]["lastError"].is_null());
        let listing: Value = client
            .get(format!("{base}/api/chats"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listing["chats"][0]["automationRetrying"], false);
        assert!(
            store
                .load(&identity)
                .unwrap()
                .automatic_retry_at_ms()
                .is_none()
        );

        let retried: Value = client
            .post(format!("{base}/api/chats/{id}/automation/action/retry"))
            .bearer_auth("test-token")
            .json(&json!({
                "requestId":"retry-owner-action",
                "actionId":failed_action
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(retried["action"]["status"], "queued");
        assert_eq!(retried["queue"]["position"], 1);
        let queued_action = retried["action"]["id"].as_str().unwrap().to_string();

        let queued: Value = client
            .get(format!("{base}/api/chats/{id}/automation"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(queued["goal"].is_null());
        assert_eq!(queued["action"]["canRunNow"], true);
        assert_eq!(queued["wake"]["phase"], "queued");
        assert_eq!(queued["queue"]["position"], 1);
        let now_body = json!({"requestId":"run-queued-owner-action","actionId":queued_action});
        assert_eq!(
            client
                .post(format!("{base}/api/chats/{id}/automation/action/now"))
                .json(&now_body)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let run_now: Value = client
            .post(format!("{base}/api/chats/{id}/automation/action/now"))
            .bearer_auth("test-token")
            .json(&now_body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(run_now["action"]["id"], queued_action);
        assert_eq!(run_now["action"]["bypassConversationCooldown"], true);
        assert!(store.load(&identity).unwrap().goal.is_none());

        let cancelled: Value = client
            .post(format!("{base}/api/chats/{id}/automation/action/cancel"))
            .bearer_auth("test-token")
            .json(&json!({
                "requestId":"cancel-owner-action",
                "actionId":queued_action
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cancelled["action"]["status"], "cancelled");
        assert!(cancelled["queue"].is_null());
        assert_eq!(supervisor.queued_count().await, 0);
        assert!(
            chat.read(false)
                .await
                .unwrap()
                .text
                .contains("Keep this pending through cancel and retry.")
        );
        assert!(
            store
                .load(&identity)
                .unwrap()
                .chat_mirror
                .message_ids
                .is_empty()
        );
        supervisor.scan_once(now_ms + 1_000_000).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 0);
        // A fresh user message resumes confirmed-unsent work through the normal send API.
        tokio::time::sleep(Duration::from_millis(2)).await;
        let sent = client
            .post(format!("{base}/api/chats/{id}/send"))
            .bearer_auth("test-token")
            .json(&json!({"request_id":"owner-after-cancel","message":"Continue now"}))
            .send()
            .await
            .unwrap();
        assert!(sent.status().is_success());
        assert_eq!(
            store.load(&identity).unwrap().action.status,
            crate::agent_automation::AutomationActionStatus::Idle
        );
        supervisor.scan_once(now_ms + 1_000_001).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 1);
        owner_task.abort();
    }

    #[test]
    fn embedded_owner_page_reuses_chat_component_without_exposing_an_initial_chat() {
        assert!(OWNER_HTML.contains("id=\"chat-template\""));
        assert!(OWNER_HTML.contains("window.mountCodexifyChat"));
        assert!(OWNER_HTML.contains("/api/chats"));
        assert_eq!(OWNER_HTML.matches("id=\"chat\"").count(), 1);
    }

    #[test]
    fn goal_commands_are_exact_bounded_and_double_slash_escapes_to_chat() {
        assert_eq!(
            parse_owner_message("/goal set Finish the browser automation."),
            Ok(OwnerMessage::Goal(
                crate::agent_automation::OwnerGoalCommand::Set {
                    markdown: "Finish the browser automation.".into()
                }
            ))
        );
        assert_eq!(
            parse_owner_message("/goal run"),
            Ok(OwnerMessage::Goal(
                crate::agent_automation::OwnerGoalCommand::Run
            ))
        );
        assert_eq!(
            parse_owner_message("/goal stop"),
            Ok(OwnerMessage::Goal(
                crate::agent_automation::OwnerGoalCommand::Stop
            ))
        );
        assert_eq!(
            parse_owner_message("/goal clear"),
            Ok(OwnerMessage::Goal(
                crate::agent_automation::OwnerGoalCommand::Clear
            ))
        );
        assert_eq!(
            parse_owner_message("/goal status"),
            Ok(OwnerMessage::Goal(
                crate::agent_automation::OwnerGoalCommand::Status
            ))
        );
        assert_eq!(
            parse_owner_message("//goal status"),
            Ok(OwnerMessage::Chat("/goal status".into()))
        );
        assert_eq!(
            parse_owner_message("ordinary text"),
            Ok(OwnerMessage::Chat("ordinary text".into()))
        );
        assert!(
            parse_owner_message("/goal pause")
                .unwrap_err()
                .contains("Unknown")
        );
        assert!(
            parse_owner_message("/goal resume")
                .unwrap_err()
                .contains("Unknown")
        );
        assert!(
            parse_owner_message("/goal set")
                .unwrap_err()
                .contains("Markdown")
        );
        assert!(
            parse_owner_message(&format!("/goal set {}", "x".repeat(10 * 1024 + 1)))
                .unwrap_err()
                .contains("10 KiB")
        );
    }
    #[derive(Default)]
    struct GoalRecoveryExecutor {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl AutomationExecutor for GoalRecoveryExecutor {
        async fn execute(
            &self,
            execution: AutomationExecution,
            events: tokio::sync::mpsc::UnboundedSender<crate::agent_automation::CompanionEvent>,
            _cancellation: CancellationToken,
        ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
            assert!(matches!(
                execution.request.prompt,
                crate::agent_automation::ActionPrompt::OfflineGoal { .. }
            ));
            self.calls.fetch_add(1, Ordering::SeqCst);
            for (index, kind) in [
                crate::agent_automation::CompanionEventKind::SendAccepted,
                crate::agent_automation::CompanionEventKind::SubmissionComplete {
                    canonical_url: execution.request.expected_url.clone(),
                },
            ]
            .into_iter()
            .enumerate()
            {
                events
                    .send(crate::agent_automation::CompanionEvent {
                        sequence: index as u64 + 1,
                        action_id: execution.request.action_id.clone(),
                        at_ms: crate::markdown_chat::now_ms(),
                        kind,
                    })
                    .unwrap();
            }

            Ok(CompanionActionSnapshot {
                protocol_version: PROTOCOL_VERSION,
                build: "goal-recovery-test".into(),
                action_id: execution.request.action_id,
                state: crate::agent_automation::CompanionActionState::Idle,
                send_accepted: true,
                canonical_url: Some(execution.request.expected_url),
                selected: Some(crate::agent_automation::CompanionSelection {
                    model: execution.request.model.unwrap(),
                    effort: execution.request.effort.unwrap(),
                }),
                last_event_sequence: 2,
                cooldown_until_ms: None,
                attention: None,
            })
        }
    }
    fn goal_recovery_state(
        root: &FilePath,
        _legacy_mode: bool,
        executor: Arc<GoalRecoveryExecutor>,
    ) -> OwnerState {
        let mut config = crate::config::default_config(root.join("project"));
        std::fs::create_dir_all(&config.work_dir).unwrap();

        config.markdown_chat.enabled = true;
        config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Steer;
        config.markdown_chat.automation.enabled = true;
        config.markdown_chat.automation.connector_name = "Codexify".into();
        config.memory.dir = Some(root.join("metadata").display().to_string());
        // The worker's pause state must come from this fixture, not the live browser.
        config.markdown_chat.automation.descriptor_path =
            root.join("browser/descriptor.json").display().to_string();
        let supervisor =
            AgentAutomationSupervisor::new(config.markdown_chat.automation.clone(), executor);
        OwnerState {
            project_bindings: Arc::new(crate::project_bindings::ProjectBindingStore::new(
                root.join("bindings"),
            )),
            creation_lock: Arc::default(),
            artifacts: Arc::new(ArtifactEgressStore::new_at(
                config.artifact_egress.clone(),
                root.join("artifacts"),
            )),
            continuations: Arc::new(
                ConversationContinuationStore::new(root.join("continuations.json")).unwrap(),
            ),
            config: Arc::new(config),
            chats: Arc::new(MarkdownChatStore::default()),
            automation: Some(supervisor),
            open_browser: Arc::new(|_| Ok(())),
            catalog_cache: Arc::default(),
            global: Arc::default(),
            token: "recovery-fixture".into(),
        }
    }
    async fn persisted_recovery_goal(
        state: &OwnerState,
        name: &str,
        status: crate::agent_automation::GoalStatus,
    ) -> (
        ConversationIdentity,
        Arc<crate::markdown_chat::ChatFile>,
        AgentAutomationStore,
    ) {
        let task = ConversationIdentity::from_openai_session(name).unwrap();
        let store = AgentAutomationStore::for_config(&state.config).unwrap();
        let chat = state
            .chats
            .chat(&state.config, Some(&task), &SessionState::new())
            .unwrap();
        chat.ensure().await.unwrap();
        chat.record_agent_call(1).await.unwrap();
        store
            .set_selection(
                &task,
                "gpt-5.6-sol",
                "max",
                "recovery-catalog",
                "recovery-account",
                1,
            )
            .unwrap();
        let url = format!("https://chatgpt.com/c/{name}");
        let issue = store.issue_pairing(&task, &url, 0, 1).unwrap();
        store
            .record_pairing_canonical_url(&task, issue.generation, &url, 1)
            .unwrap();
        store
            .verify_pairing(&task, issue.generation, 0, &issue.reference, 1)
            .unwrap();
        let goal = store
            .set_goal(&task, "Continue this enabled objective", 1)
            .unwrap();
        if status != crate::agent_automation::GoalStatus::Configured {
            store.run_goal(&task, goal.revision, 2).unwrap();
        }
        match status {
            crate::agent_automation::GoalStatus::Blocked => {
                store
                    .finish_goal(
                        &task,
                        goal.revision,
                        crate::agent_automation::GoalFinish::Blocked {
                            reason: "Needs the user's answer".into(),
                        },
                        3,
                    )
                    .unwrap();
            }
            crate::agent_automation::GoalStatus::Completed => {
                store
                    .finish_goal(
                        &task,
                        goal.revision,
                        crate::agent_automation::GoalFinish::Completed { summary: None },
                        3,
                    )
                    .unwrap();
            }
            _ => {}
        }
        assert!(!chat.path().with_file_name("message-queue.json").exists());
        assert!(store.load(&task).unwrap().turn_renewal.handoff.is_none());
        (task, chat, store)
    }

    #[tokio::test]
    async fn active_goals_restore_and_continue_without_opening_a_chat_or_having_a_queue() {
        use crate::agent_automation::GoalStatus;
        for (multi, orphaned) in [(false, false), (true, false), (false, true), (true, true)] {
            let root = tempfile::tempdir().unwrap();
            let executor = Arc::new(GoalRecoveryExecutor::default());
            let state = goal_recovery_state(root.path(), multi, executor.clone());
            let (active, chat, store) =
                persisted_recovery_goal(&state, "restart-active-goal", GoalStatus::Active).await;
            let (paused, _, _) =
                persisted_recovery_goal(&state, "restart-paused-goal", GoalStatus::Configured)
                    .await;
            let (blocked, _, _) =
                persisted_recovery_goal(&state, "restart-blocked-goal", GoalStatus::Blocked).await;
            let (completed, _, _) =
                persisted_recovery_goal(&state, "restart-completed-goal", GoalStatus::Completed)
                    .await;
            let (stopped, stopped_chat, _) =
                persisted_recovery_goal(&state, "restart-stopped-goal", GoalStatus::Active).await;
            stopped_chat
                .request_agent_stop("explicit-stop".into())
                .await
                .unwrap();
            if orphaned {
                let saved = store.load(&active).unwrap();
                store
                    .queue_action(
                        &active,
                        "interrupted-before-restart",
                        crate::agent_automation::AutomationActionKind::OfflineGoal,
                        saved.pairing.generation,
                        Some(saved.goal.unwrap().revision),
                        1,
                    )
                    .unwrap();
            }
            // These are new in-memory registries over the same saved state, as after restart.
            let mut restarted = state.clone();
            restarted.chats = Arc::new(MarkdownChatStore::default());
            let supervisor = restarted.automation.as_ref().unwrap();
            let worker = supervisor.start();
            restore_chat_state(restarted.clone()).await;
            assert!(supervisor.is_registered(&active).await);
            for task in [&paused, &blocked, &completed] {
                assert!(!supervisor.is_registered(task).await);
            }
            assert!(supervisor.is_registered(&stopped).await);
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let saved = store.load(&active).unwrap();
                    if executor.calls.load(Ordering::SeqCst) == 1
                        && saved.action.send_accepted == Some(true)
                        && saved.action.status
                            == crate::agent_automation::AutomationActionStatus::Idle
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                supervisor.queued_count().await,
                0,
                "only the active, unstopped goal resumes"
            );
            let now = crate::markdown_chat::now_ms();
            assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                store.load(&active).unwrap().goal.unwrap().status,
                GoalStatus::Active
            );
            supervisor.scan_once(now + 1000).await.unwrap();
            assert_eq!(
                supervisor.queued_count().await,
                0,
                "discovery must not duplicate a submitted continuation"
            );
            drop(worker);
            assert!(
                !std::fs::read_to_string(chat.path())
                    .unwrap()
                    .contains("Continue this enabled objective")
            );
        }
    }

    #[tokio::test]
    async fn periodic_discovery_retries_an_unreadable_goal_without_needing_the_ui() {
        use crate::agent_automation::GoalStatus;
        let root = tempfile::tempdir().unwrap();
        let state =
            goal_recovery_state(root.path(), true, Arc::new(GoalRecoveryExecutor::default()));
        let (task, chat, store) =
            persisted_recovery_goal(&state, "retry-discovery-goal", GoalStatus::Active).await;
        let path = chat.path().with_file_name("automation.json");
        let saved = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"temporarily incomplete JSON").unwrap();
        let rollout = chat.path().with_file_name("rollout.md");
        std::fs::remove_file(&rollout).unwrap();
        let maintenance = tokio::spawn(maintain_chat_automation(
            state.clone(),
            std::time::Duration::from_millis(30),
        ));
        let supervisor = state.automation.as_ref().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !rollout.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!supervisor.is_registered(&task).await);
        let (other, _, _) =
            persisted_recovery_goal(&state, "late-discovered-goal", GoalStatus::Active).await;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !supervisor.is_registered(&other).await {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        std::fs::write(&path, saved).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !supervisor.is_registered(&task).await {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        supervisor
            .scan_once(crate::markdown_chat::now_ms())
            .await
            .unwrap();
        assert_eq!(supervisor.queued_count().await, 2);
        assert_eq!(
            store.load(&task).unwrap().goal.unwrap().status,
            GoalStatus::Active
        );
        maintenance.abort();
    }

    #[tokio::test]
    async fn private_queue_resumes_after_service_restart_without_opening_a_chat() {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().into());
        config.memory.dir = Some(root.path().join("metadata").display().to_string());
        config.markdown_chat.enabled = true;
        config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Steer;
        let task = ConversationIdentity::from_openai_session("queue-restart-owner").unwrap();
        let store = AgentAutomationStore::for_config(&config).unwrap();
        let goal = store
            .set_goal(&task, "Keep queued text private", 1)
            .unwrap();
        let old = MarkdownChatStore::default();
        let chat = old
            .chat(&config, Some(&task), &SessionState::new())
            .unwrap();
        chat.ensure().await.unwrap();
        chat.record_agent_call(1).await.unwrap();
        for id in ["first", "second"] {
            chat.queue_user_message(id.into(), format!("private {id}"), vec![])
                .await
                .unwrap();
        }
        let path = chat.path().to_path_buf();
        drop(chat);
        drop(old);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        store.run_goal(&task, goal.revision, 2).unwrap();
        store
            .finish_goal(
                &task,
                goal.revision,
                crate::agent_automation::GoalFinish::Completed { summary: None },
                3,
            )
            .unwrap();
        let legacy_id = ConversationIdentity::from_openai_session("legacy-chat-rollout").unwrap();
        let legacy = MarkdownChatStore::default()
            .chat(&config, Some(&legacy_id), &SessionState::new())
            .unwrap();
        legacy.ensure().await.unwrap();
        let legacy_rollout = legacy.path().with_file_name("rollout.md");
        std::fs::remove_file(&legacy_rollout).unwrap();
        let chats = Arc::new(MarkdownChatStore::default());
        let state = OwnerState {
            project_bindings: Arc::new(crate::project_bindings::ProjectBindingStore::new(
                root.path().join("bindings"),
            )),
            creation_lock: Arc::default(),
            artifacts: Arc::new(ArtifactEgressStore::new_at(
                config.artifact_egress.clone(),
                root.path().join("artifacts"),
            )),
            continuations: Arc::new(
                ConversationContinuationStore::new(root.path().join("continuations.json")).unwrap(),
            ),
            config: Arc::new(config),
            chats,
            automation: None,
            open_browser: Arc::new(|_| Ok(())),
            catalog_cache: Arc::default(),
            global: Arc::default(),
            token: "fixture-token".into(),
        };
        restore_chat_state(state.clone()).await;
        assert!(
            legacy_rollout.exists(),
            "all legacy chats get a progress file without opening the UI"
        );
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let body = std::fs::read_to_string(&path).unwrap();
                if body.contains("private first") {
                    assert!(!body.contains("private second"));
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
    }
}
