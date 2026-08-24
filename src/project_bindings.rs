//! Durable project selection for clients that provide a stable conversation ID.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use rmcp::model::RequestMetaObject;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::AppConfig;
use crate::util::home_dir;

pub const OPENAI_SESSION_META_KEY: &str = "openai/session";

const BINDING_VERSION: u32 = 1;
const LOCK_STALE_MS: u128 = 10_000;
const LOCK_TIMEOUT_MS: u128 = 2_000;
const LOCK_RETRY_MS: u64 = 20;

static ATOMIC_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ConversationIdentity {
    key: String,
}

impl std::fmt::Debug for ConversationIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConversationIdentity")
            .field("key", &"<redacted>")
            .finish()
    }
}

impl ConversationIdentity {
    pub fn from_request_meta(meta: &RequestMetaObject) -> Option<Self> {
        meta.get(OPENAI_SESSION_META_KEY)
            .and_then(serde_json::Value::as_str)
            .and_then(Self::from_openai_session)
    }

    pub fn from_openai_session(session: &str) -> Option<Self> {
        let session = session.trim();
        if session.is_empty() {
            return None;
        }

        let mut hasher = Sha256::new();
        hasher.update(b"codexify/openai-session/v1\0");
        hasher.update(session.as_bytes());
        Some(Self {
            key: encode_hex(&hasher.finalize(), 64),
        })
    }

    pub(crate) fn stable_key(&self) -> &str {
        &self.key
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectBindingScope {
    ChatGptConversation,
    McpTransportSession,
}

impl ProjectBindingScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatGptConversation => "chatgpt_conversation",
            Self::McpTransportSession => "mcp_transport_session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRootSelection {
    pub access_root: PathBuf,
    pub project_root: PathBuf,
    pub newly_selected: bool,
    pub scope: ProjectBindingScope,
}

#[derive(Debug, Clone)]
pub struct ProjectBindingStore {
    base_dir: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredProjectBinding {
    version: u32,
    access_root: String,
    project_root: String,
}

struct BindingLock {
    path: PathBuf,
}

impl Drop for BindingLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl ProjectBindingStore {
    pub fn for_current_user() -> Self {
        let home = home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self::new(home.join(".codexify").join("conversation-projects"))
    }

    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn effective_config(
        &self,
        config: &AppConfig,
        identity: &ConversationIdentity,
    ) -> Result<AppConfig, String> {
        if !config.multi_project {
            return Ok(config.clone());
        }

        let Some(project_root) = self.selected_project_root(config, identity)? else {
            return Err(format!(
                "No project root is selected for this ChatGPT conversation. If the exact path is unknown, call `list_projects` first. Then call `set_project_root` with a directory relative to the access root `{}`, followed by `get_agent_brief` before using project tools. The selection is stored by ChatGPT conversation ID and will survive MCP reconnects and server restarts.",
                config.work_dir.display()
            ));
        };

        let mut effective = config.clone();
        effective.work_dir = project_root;
        Ok(effective)
    }

    pub fn selected_project_root(
        &self,
        config: &AppConfig,
        identity: &ConversationIdentity,
    ) -> Result<Option<PathBuf>, String> {
        let access_root = canonical_access_root(config)?;
        let path = self.binding_path(&access_root, identity);
        self.read_binding(&path, &access_root)
    }

    pub fn select_project_root(
        &self,
        config: &AppConfig,
        identity: &ConversationIdentity,
        input: &str,
    ) -> Result<ProjectRootSelection, String> {
        let (access_root, project_root) = resolve_project_root(config, input)?;
        let binding_path = self.binding_path(&access_root, identity);
        let _lock = acquire_lock(&binding_path)?;

        if let Some(current) = self.read_binding(&binding_path, &access_root)? {
            if current == project_root {
                return Ok(ProjectRootSelection {
                    access_root,
                    project_root,
                    newly_selected: false,
                    scope: ProjectBindingScope::ChatGptConversation,
                });
            }

            return Err(format!(
                "This ChatGPT conversation is already bound to project root `{}` and cannot switch to `{}`. Start a new chat for another project.",
                current.display(),
                project_root.display()
            ));
        }

        let binding = StoredProjectBinding {
            version: BINDING_VERSION,
            access_root: access_root.to_string_lossy().into_owned(),
            project_root: project_root.to_string_lossy().into_owned(),
        };
        self.write_binding(&binding_path, &binding)?;

        Ok(ProjectRootSelection {
            access_root,
            project_root,
            newly_selected: true,
            scope: ProjectBindingScope::ChatGptConversation,
        })
    }

    fn binding_path(&self, access_root: &Path, identity: &ConversationIdentity) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(b"codexify/access-root/v1\0");
        hasher.update(access_root.to_string_lossy().as_bytes());
        let access_key = encode_hex(&hasher.finalize(), 24);
        self.base_dir
            .join(access_key)
            .join(format!("{}.json", identity.stable_key()))
    }

    fn read_binding(&self, path: &Path, access_root: &Path) -> Result<Option<PathBuf>, String> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "Could not read the stored ChatGPT conversation project binding at {}: {error}",
                    path.display()
                ));
            }
        };

        let binding: StoredProjectBinding = serde_json::from_str(&raw).map_err(|error| {
            format!(
                "The stored ChatGPT conversation project binding at {} is invalid: {error}. Start a new chat or remove that binding file.",
                path.display()
            )
        })?;
        if binding.version != BINDING_VERSION {
            return Err(format!(
                "The stored ChatGPT conversation project binding at {} uses unsupported version {}. Start a new chat or remove that binding file.",
                path.display(),
                binding.version
            ));
        }

        if Path::new(&binding.access_root) != access_root {
            return Err(format!(
                "The stored ChatGPT conversation project binding at {} belongs to a different access root. Start a new chat or remove that binding file.",
                path.display()
            ));
        }

        let stored_root = PathBuf::from(&binding.project_root);
        let project_root = std::fs::canonicalize(&stored_root).map_err(|error| {
            format!(
                "The project bound to this ChatGPT conversation no longer exists or cannot be resolved: {}: {error}. Start a new chat for another project.",
                stored_root.display()
            )
        })?;
        if !project_root.is_dir() {
            return Err(format!(
                "The project bound to this ChatGPT conversation is no longer a directory: {}. Start a new chat for another project.",
                project_root.display()
            ));
        }
        if project_root != access_root && !project_root.starts_with(access_root) {
            return Err(format!(
                "The project bound to this ChatGPT conversation now resolves outside the configured access root: {}. Start a new chat or remove the binding file at {}.",
                project_root.display(),
                path.display()
            ));
        }

        Ok(Some(project_root))
    }

    fn write_binding(&self, target: &Path, binding: &StoredProjectBinding) -> Result<(), String> {
        let parent = target.parent().ok_or_else(|| {
            format!(
                "Could not determine the directory for project binding {}",
                target.display()
            )
        })?;
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Could not create project-binding directory {}: {error}",
                parent.display()
            )
        })?;

        let counter = ATOMIC_COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut temporary = target.as_os_str().to_os_string();
        temporary.push(format!(".tmp.{}.{}", std::process::id(), counter));
        let temporary = PathBuf::from(temporary);
        let json = serde_json::to_string_pretty(binding)
            .map_err(|error| format!("Could not serialize project binding: {error}"))?;

        let result = (|| -> std::io::Result<()> {
            {
                let mut file = std::fs::File::create(&temporary)?;
                file.write_all(json.as_bytes())?;
                file.write_all(b"\n")?;
                file.sync_all()?;
            }
            std::fs::rename(&temporary, target)?;
            Ok(())
        })();

        if let Err(error) = result {
            let _ = std::fs::remove_file(&temporary);
            return Err(format!(
                "Could not persist the ChatGPT conversation project binding at {}: {error}",
                target.display()
            ));
        }

        Ok(())
    }
}

pub fn resolve_project_root(config: &AppConfig, input: &str) -> Result<(PathBuf, PathBuf), String> {
    if !config.multi_project {
        return Err(
            "Project-root selection is disabled. Start codexify with `--multi-project` or set `multiProject` to true."
                .to_string(),
        );
    }

    let input = input.trim();
    if input.is_empty() {
        return Err("path must be a non-empty string".to_string());
    }

    let access_root = canonical_access_root(config)?;
    let input_path = Path::new(input);
    let candidate = if input_path.is_absolute() {
        input_path.to_path_buf()
    } else {
        config.work_dir.join(input_path)
    };
    let project_root = std::fs::canonicalize(&candidate).map_err(|error| {
        format!(
            "Project root does not exist or cannot be resolved: {}: {error}",
            candidate.display()
        )
    })?;

    if !project_root.is_dir() {
        return Err(format!(
            "Project root is not a directory: {}",
            project_root.display()
        ));
    }
    if project_root != access_root && !project_root.starts_with(&access_root) {
        return Err(format!(
            "Project root escapes the configured access root: {}",
            project_root.display()
        ));
    }

    Ok((access_root, project_root))
}

fn canonical_access_root(config: &AppConfig) -> Result<PathBuf, String> {
    std::fs::canonicalize(&config.work_dir).map_err(|error| {
        format!(
            "Could not resolve project access root {}: {error}",
            config.work_dir.display()
        )
    })
}

fn acquire_lock(binding_path: &Path) -> Result<BindingLock, String> {
    let parent = binding_path.parent().ok_or_else(|| {
        format!(
            "Could not determine the directory for project binding {}",
            binding_path.display()
        )
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "Could not create project-binding directory {}: {error}",
            parent.display()
        )
    })?;

    let mut lock_path = binding_path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    let deadline = now_ms() + LOCK_TIMEOUT_MS;

    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "{} {}", std::process::id(), now_ms());
                return Ok(BindingLock { path: lock_path });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                match std::fs::metadata(&lock_path).and_then(|metadata| metadata.modified()) {
                    Ok(modified) => {
                        let age = SystemTime::now()
                            .duration_since(modified)
                            .map(|duration| duration.as_millis())
                            .unwrap_or(0);
                        if age > LOCK_STALE_MS {
                            let _ = std::fs::remove_file(&lock_path);
                            continue;
                        }
                    }
                    Err(_) => continue,
                }
                if now_ms() >= deadline {
                    return Err(format!(
                        "Timed out waiting to update project binding {}",
                        binding_path.display()
                    ));
                }
                std::thread::sleep(Duration::from_millis(LOCK_RETRY_MS));
            }
            Err(error) => {
                return Err(format!(
                    "Could not lock project binding {}: {error}",
                    binding_path.display()
                ));
            }
        }
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn encode_hex(bytes: &[u8], chars: usize) -> String {
    let mut encoded = String::with_capacity(chars);
    for byte in bytes {
        if encoded.len() >= chars {
            break;
        }
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded.truncate(chars);
    encoded
}
