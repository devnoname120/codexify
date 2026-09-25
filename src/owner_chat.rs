//! Local-only owner view of the persisted agent chats. This is deliberately not
//! nested under `/mcp` and never shares the connector's tunnel listener.

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

use crate::artifact_egress::ArtifactEgressStore;
use crate::markdown_chat::{MarkdownChatStore, OwnerChatSummary};
use crate::markdown_chat_ui::{CHAT_UI_HTML, CHAT_WIDGET_META};
use crate::types::AppConfig;
use crate::util::home_dir;

type ApiResult = Result<Json<Value>, (StatusCode, String)>;

#[derive(Clone)]
struct OwnerState {
    config: Arc<AppConfig>,
    chats: Arc<MarkdownChatStore>,
    artifacts: Arc<ArtifactEgressStore>,
    token: String,
}

#[derive(Serialize, Deserialize)]
struct RuntimeInfo {
    port: u16,
    token: String,
}

pub struct OwnerServer {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for OwnerServer {
    fn drop(&mut self) {
        self.task.abort();
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
    work_dir: Option<PathBuf>,
    workspace: String,
}

fn state_roots(config: &AppConfig) -> Vec<PathBuf> {
    if !config.multi_project {
        return vec![crate::memory::memory_dir(config)];
    }
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
    std::fs::read_dir(base)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_real_dir(path))
        .collect()
}

fn work_dir_for_root(config: &AppConfig, root: &FilePath) -> Option<PathBuf> {
    if !config.multi_project {
        return Some(config.work_dir.clone());
    }
    let path = root.join("memory.json");
    if !is_real_file(&path) {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > 1024 * 1024 {
        return None;
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get("workDir")?.as_str().map(PathBuf::from)
}

fn locate(config: &AppConfig) -> Vec<LocatedChat> {
    let mut found = Vec::new();
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
            let active_work_dir = work_dir.clone();
            let workspace = active_work_dir
                .as_ref()
                .and_then(|path| path.file_name())
                .unwrap_or_else(|| root.file_name().unwrap_or_default())
                .to_string_lossy()
                .into_owned();
            found.push(LocatedChat {
                id: format!("{:x}", hasher.finalize()),
                owner,
                path,
                work_dir: active_work_dir,
                workspace,
            });
        }
    }
    found
}

fn selected(config: &AppConfig, id: &str) -> Result<LocatedChat, (StatusCode, String)> {
    if !stable_owner(id) {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".into()));
    }
    locate(config)
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
    workspace: String,
    last_entry_end: u64,
    last_entry_at_ms: u64,
    last_agent_call_at_ms: Option<u64>,
    agent_waiting_until_ms: Option<u64>,
    total_tool_calls: u64,
}

async fn list_chats(State(state): State<OwnerState>) -> ApiResult {
    let mut list = Vec::new();
    for located in locate(&state.config) {
        let chat = state
            .chats
            .chat_at_path(located.path.clone(), true)
            .map_err(internal_error)?;
        let OwnerChatSummary {
            title,
            last_entry_end,
            last_entry_at_ms,
            last_agent_call_at_ms,
            agent_waiting_until_ms,
            total_tool_calls,
        } = match chat.owner_summary().await {
            Ok(summary) => summary,
            Err(error) => {
                tracing::warn!("owner chat transcript unavailable: {error}");
                let metadata = std::fs::metadata(&located.path).map_err(internal_error)?;
                OwnerChatSummary {
                    title: "Conversation unavailable".into(),
                    last_entry_end: metadata.len(),
                    last_entry_at_ms: metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| duration.as_millis() as u64)
                        .unwrap_or_default(),
                    last_agent_call_at_ms: None,
                    agent_waiting_until_ms: None,
                    total_tool_calls: 0,
                }
            }
        };
        list.push(ChatListing {
            id: located.id,
            title,
            workspace: located.workspace,
            last_entry_end,
            last_entry_at_ms,
            last_agent_call_at_ms,
            agent_waiting_until_ms,
            total_tool_calls,
        });
    }
    list.sort_by(|a, b| {
        b.last_entry_at_ms
            .cmp(&a.last_entry_at_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(Json(
        json!({"chats": list, "serverTimeMs": crate::markdown_chat::now_ms()}),
    ))
}

#[derive(Deserialize)]
struct PageQuery {
    before: Option<u64>,
    revision: Option<String>,
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
    let located = selected(&state.config, &id)?;
    let chat = state
        .chats
        .chat_at_path(located.path, true)
        .map_err(internal_error)?;
    let page = chat
        .widget_page(query.before, query.revision)
        .await
        .map_err(internal_error)?;
    Ok(wrap(serde_json::to_value(page).expect("chat page")))
}

#[derive(Deserialize)]
struct SendBody {
    request_id: String,
    message: String,
}

async fn chat_send(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<SendBody>,
) -> ApiResult {
    let located = selected(&state.config, &id)?;
    let chat = state
        .chats
        .chat_at_path(located.path, true)
        .map_err(internal_error)?;
    let sent = chat
        .append_user(body.request_id, body.message)
        .await
        .map_err(internal_error)?;
    Ok(wrap(json!({"sent": sent})))
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
    let located = selected(&state.config, &id)?;
    let work_dir = crate::project_bindings::ProjectBindingStore::for_current_user()
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
    let host_ok = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| host.starts_with("127.0.0.1:"));
    if !host_ok {
        return StatusCode::FORBIDDEN.into_response();
    }
    if request.uri().path() != "/" {
        let expected = format!("Bearer {}", state.token);
        if request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            != Some(&expected)
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
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
        header::HeaderValue::from_static("default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src blob: data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"),
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
});

pub async fn start(
    config: Arc<AppConfig>,
    chats: Arc<MarkdownChatStore>,
    artifacts: Arc<ArtifactEgressStore>,
) -> anyhow::Result<OwnerServer> {
    let listener = bind_owner_listener(&config).await?;
    let port = listener.local_addr()?.port();
    let info = load_or_create_runtime(&runtime_path(), port)?;
    let state = OwnerState {
        config,
        chats,
        artifacts,
        token: info.token,
    };
    let app = router(state);
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::error!("owner chat listener stopped: {error}");
        }
    });
    Ok(OwnerServer { task })
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
        .route("/api/chats", get(list_chats))
        .route("/api/chats/{id}", get(chat_state))
        .route("/api/chats/{id}/send", post(chat_send))
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
    use super::*;
    use crate::exec_sessions::SessionState;
    use crate::project_bindings::ConversationIdentity;

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
    async fn owner_api_lists_sends_and_reads_the_same_persisted_chat() {
        crate::tls::ensure_crypto_provider();
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(temp.path().into());
        config.markdown_chat.enabled = true;
        config.memory.dir = Some(temp.path().join("state").display().to_string());
        let identity = ConversationIdentity::from_openai_session("owner-api-test").unwrap();
        let chats = Arc::new(MarkdownChatStore::default());
        let chat = chats
            .chat(&config, Some(&identity), &SessionState::new())
            .unwrap();
        chat.ensure().await.unwrap();
        let artifacts = Arc::new(ArtifactEgressStore::new_at(
            config.artifact_egress.clone(),
            temp.path().join("artifacts"),
        ));
        let state = OwnerState {
            config: Arc::new(config),
            chats,
            artifacts,
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
        task.abort();
    }

    #[test]
    fn embedded_owner_page_reuses_chat_component_without_exposing_an_initial_chat() {
        assert!(OWNER_HTML.contains("id=\"chat-template\""));
        assert!(OWNER_HTML.contains("window.mountCodexifyChat"));
        assert!(OWNER_HTML.contains("/api/chats"));
        assert_eq!(OWNER_HTML.matches("id=\"chat\"").count(), 1);
    }
}
