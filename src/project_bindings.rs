use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use rmcp::model::RequestMetaObject;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::project_clone::{
    ProjectReference, normalize_repository_url, project_matches_repository, repository_url_matches,
    resolve_git_project,
};
use crate::types::{AppConfig, WorktreeMode};
use crate::util::home_dir;
use crate::worktrees::{
    ManagedWorktree, create_managed_worktree_at, load_metadata, metadata_path_for_worktree,
    rollback_managed_worktree,
};

pub const OPENAI_SESSION_META_KEY: &str = "openai/session";

const BINDING_VERSION: u32 = 2;
const LEGACY_BINDING_VERSION: u32 = 1;
const LOCK_STALE_MS: u128 = 10 * 60 * 1_000;
const LOCK_TIMEOUT_MS: u128 = 5 * 60 * 1_000;
const LOCK_RETRY_MS: u64 = 50;

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

    pub fn audit_hash(&self) -> &str {
        &self.key[..24]
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
    pub source_project_root: PathBuf,
    pub project_root: PathBuf,
    pub repository_url: Option<String>,
    pub cloned: bool,
    pub managed_worktree: bool,
    pub worktree_git_root: Option<PathBuf>,
    pub worktrees_root: Option<PathBuf>,
    pub worktree_mode: WorktreeMode,
    pub warnings: Vec<String>,
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
    #[serde(default)]
    source_project_root: Option<String>,
    #[serde(default)]
    repository_url: Option<String>,
    #[serde(default)]
    managed_worktree: bool,
    #[serde(default)]
    worktree_git_root: Option<String>,
    #[serde(default)]
    worktrees_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedProjectBinding {
    source_project_root: PathBuf,
    project_root: PathBuf,
    repository_url: Option<String>,
    managed_worktree: bool,
    worktree_git_root: Option<PathBuf>,
    worktrees_root: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct PreparedProjectRoot {
    pub access_root: PathBuf,
    pub source_project_root: PathBuf,
    pub repository_url: Option<String>,
    pub checkout_commit: Option<String>,
    pub source_matches_checkout: bool,
    pub cloned: bool,
    pub warnings: Vec<String>,
}

struct AssignmentScan {
    assigned: bool,
    warnings: Vec<String>,
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

        let Some(binding) = self.selected_binding(config, identity)? else {
            return Err(format!(
                "No project root is selected for this ChatGPT conversation. Call `set_project_root` with an existing directory beneath the access root `{}`, an HTTPS/SSH Git repository URL ending in `.git`, or an exact GitHub repository, branch, pull-request, or commit URL. Repository URLs reuse a matching checkout or clone into the configured clone directory. If only a project name or purpose is known, call `list_projects` first. Then call `get_agent_brief` before using project tools. The selection is stored by ChatGPT conversation ID and will survive MCP reconnects and server restarts.",
                config.work_dir.display()
            ));
        };

        let mut effective = config.clone();
        effective.work_dir = binding.project_root;
        Ok(effective)
    }

    pub fn selected_project_root(
        &self,
        config: &AppConfig,
        identity: &ConversationIdentity,
    ) -> Result<Option<PathBuf>, String> {
        Ok(self
            .selected_binding(config, identity)?
            .map(|binding| binding.project_root))
    }

    pub async fn select_project_root(
        &self,
        config: &AppConfig,
        identity: &ConversationIdentity,
        input: &str,
    ) -> Result<ProjectRootSelection, String> {
        if !config.multi_project {
            return Err(
                "Project-root selection is disabled. Start codexify with `--multi-project` or set `multiProject` to true."
                    .to_string(),
            );
        }
        let reference = ProjectReference::parse(input)?;
        let access_root = canonical_access_root(config)?;
        let binding_path = self.binding_path(&access_root, identity);
        let binding_lock = acquire_lock(&binding_path).await?;

        if let Some(mut current) = self.read_binding(&binding_path, &access_root)? {
            if project_reference_matches(
                config,
                &reference,
                &current.source_project_root,
                current.repository_url.as_deref(),
            )
            .await?
            {
                if current.repository_url.is_none()
                    && let ProjectReference::Git(repository) = &reference
                {
                    current.repository_url = Some(repository.web_url().to_string());
                }
                return Ok(selection_from_binding(
                    access_root,
                    current,
                    config.worktrees.mode,
                    false,
                    false,
                    ProjectBindingScope::ChatGptConversation,
                    Vec::new(),
                ));
            }

            return Err(format!(
                "This ChatGPT conversation is already bound to source project `{}` and cannot switch to `{}`. Start a new chat for another project.",
                current.source_project_root.display(),
                reference.display()
            ));
        }

        let prepared = prepare_project_root(config, &reference).await?;
        if prepared.access_root != access_root {
            return Err(
                "The configured project access root changed while resolving the project; retry the selection"
                    .to_string(),
            );
        }
        let source_project_root = prepared.source_project_root;
        let checkout_commit = prepared.checkout_commit;
        let requires_target_worktree =
            checkout_commit.is_some() && !prepared.source_matches_checkout;

        let assignment_path = self.assignment_path(&access_root, &source_project_root);
        let assignment_lock = acquire_lock(&assignment_path).await?;
        let scan = self.scan_source_assignments(&access_root, &source_project_root);
        let create_worktree = match config.worktrees.mode {
            WorktreeMode::Auto => scan.assigned || requires_target_worktree,
            WorktreeMode::Always => true,
            WorktreeMode::Never if requires_target_worktree => {
                return Err(format!(
                    "The requested GitHub branch, pull request, or commit is not the current checkout in `{}` and worktree isolation is disabled. Enable worktrees with mode `auto` or `always`, or check out the requested target in that source repository before selecting it.",
                    source_project_root.display()
                ));
            }
            WorktreeMode::Never => false,
        };
        let mut warnings = prepared.warnings;
        warnings.extend(scan.warnings);

        let managed = if create_worktree {
            let worktree = create_managed_worktree_at(
                config,
                &source_project_root,
                checkout_commit.as_deref(),
            )
            .await?;
            warnings.extend(worktree.warnings.iter().cloned());
            Some(worktree)
        } else {
            None
        };

        let resolved = resolved_from_managed(
            &source_project_root,
            managed.as_ref(),
            prepared.repository_url,
        );
        let stored = StoredProjectBinding {
            version: BINDING_VERSION,
            access_root: access_root.to_string_lossy().into_owned(),
            source_project_root: Some(source_project_root.to_string_lossy().into_owned()),
            project_root: resolved.project_root.to_string_lossy().into_owned(),
            repository_url: resolved.repository_url.clone(),
            managed_worktree: resolved.managed_worktree,
            worktree_git_root: resolved
                .worktree_git_root
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            worktrees_root: resolved
                .worktrees_root
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        };
        if let Err(error) = self.write_binding(&binding_path, &stored) {
            let rollback = match managed.as_ref() {
                Some(worktree) => rollback_managed_worktree(config, worktree).await.err(),
                None => None,
            };
            return Err(match rollback {
                Some(rollback) => {
                    format!("{error}\nManaged-worktree rollback also failed: {rollback}")
                }
                None => error,
            });
        }

        drop(assignment_lock);
        drop(binding_lock);

        Ok(selection_from_binding(
            access_root,
            resolved,
            config.worktrees.mode,
            true,
            prepared.cloned,
            ProjectBindingScope::ChatGptConversation,
            warnings,
        ))
    }

    pub fn referenced_managed_project_roots(
        &self,
        config: &AppConfig,
    ) -> Result<HashSet<PathBuf>, String> {
        if !config.multi_project {
            return Ok(HashSet::new());
        }
        let access_root = canonical_access_root(config)?;
        let mut roots = HashSet::new();
        for path in self.binding_files(&access_root) {
            if let Ok(Some(binding)) = self.read_binding(&path, &access_root)
                && binding.managed_worktree
            {
                roots.insert(binding.project_root);
            }
        }
        Ok(roots)
    }

    fn selected_binding(
        &self,
        config: &AppConfig,
        identity: &ConversationIdentity,
    ) -> Result<Option<ResolvedProjectBinding>, String> {
        let access_root = canonical_access_root(config)?;
        let path = self.binding_path(&access_root, identity);
        self.read_binding(&path, &access_root)
    }

    fn scan_source_assignments(
        &self,
        access_root: &Path,
        source_project_root: &Path,
    ) -> AssignmentScan {
        let mut assigned = false;
        let mut warnings = Vec::new();
        for path in self.binding_files(access_root) {
            match self.read_binding(&path, access_root) {
                Ok(Some(binding)) if binding.source_project_root == source_project_root => {
                    assigned = true;
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    assigned = true;
                    warnings.push(format!(
                        "A stored conversation binding could not be validated, so the shared checkout was treated as assigned: {error}"
                    ));
                }
            }
        }
        AssignmentScan { assigned, warnings }
    }

    fn binding_files(&self, access_root: &Path) -> Vec<PathBuf> {
        let directory = self.access_root_dir(access_root);
        let Ok(entries) = std::fs::read_dir(directory) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                (entry.file_type().ok()?.is_file()
                    && path
                        .extension()
                        .is_some_and(|extension| extension == "json"))
                .then_some(path)
            })
            .collect()
    }

    fn access_root_dir(&self, access_root: &Path) -> PathBuf {
        self.base_dir.join(access_root_key(access_root))
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

    fn assignment_path(&self, access_root: &Path, source_project_root: &Path) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(b"codexify/source-project/v1\0");
        hasher.update(source_project_root.to_string_lossy().as_bytes());
        self.access_root_dir(access_root)
            .join("assignments")
            .join(encode_hex(&hasher.finalize(), 64))
    }

    fn read_binding(
        &self,
        path: &Path,
        access_root: &Path,
    ) -> Result<Option<ResolvedProjectBinding>, String> {
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
        if !matches!(binding.version, LEGACY_BINDING_VERSION | BINDING_VERSION) {
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

        let source_stored = binding
            .source_project_root
            .as_deref()
            .unwrap_or(&binding.project_root);
        let source_project_root = canonical_binding_dir(
            source_stored,
            "source project",
            path,
            "Start a new chat for another project.",
        )?;
        if source_project_root != access_root && !source_project_root.starts_with(access_root) {
            return Err(format!(
                "The source project bound to this ChatGPT conversation now resolves outside the configured access root: {}. Start a new chat or remove the binding file at {}.",
                source_project_root.display(),
                path.display()
            ));
        }

        let project_root = canonical_binding_dir(
            &binding.project_root,
            "active project",
            path,
            "Start a new chat for another project.",
        )?;
        let repository_url = binding
            .repository_url
            .as_deref()
            .map(normalize_repository_url)
            .transpose()
            .map_err(|error| {
                format!(
                    "The Git repository URL in the ChatGPT conversation binding at {} is invalid: {error}. Start a new chat or remove that binding file.",
                    path.display()
                )
            })?;
        if !binding.managed_worktree {
            if project_root != source_project_root {
                return Err(format!(
                    "The direct project binding at {} has inconsistent source and active roots. Start a new chat or remove that binding file.",
                    path.display()
                ));
            }
            return Ok(Some(ResolvedProjectBinding {
                source_project_root,
                project_root,
                repository_url,
                managed_worktree: false,
                worktree_git_root: None,
                worktrees_root: None,
            }));
        }

        if binding.version != BINDING_VERSION {
            return Err(format!(
                "The stored project binding at {} marks a managed worktree using a legacy format. Start a new chat or remove that binding file.",
                path.display()
            ));
        }
        let worktree_git_root = canonical_binding_dir(
            binding.worktree_git_root.as_deref().ok_or_else(|| {
                format!(
                    "The managed-worktree binding at {} does not record its Git root. Start a new chat or remove that binding file.",
                    path.display()
                )
            })?,
            "managed worktree Git root",
            path,
            "Start a new chat for another project.",
        )?;
        let worktrees_root = canonical_binding_dir(
            binding.worktrees_root.as_deref().ok_or_else(|| {
                format!(
                    "The managed-worktree binding at {} does not record its worktree root. Start a new chat or remove that binding file.",
                    path.display()
                )
            })?,
            "managed worktree root",
            path,
            "Start a new chat for another project.",
        )?;
        if worktree_git_root == worktrees_root || !worktree_git_root.starts_with(&worktrees_root) {
            return Err(format!(
                "The managed worktree Git root {} is outside its recorded worktree root {}. Start a new chat or remove the binding file at {}.",
                worktree_git_root.display(),
                worktrees_root.display(),
                path.display()
            ));
        }
        if project_root != worktree_git_root && !project_root.starts_with(&worktree_git_root) {
            return Err(format!(
                "The active project {} is outside its managed worktree Git root {}. Start a new chat or remove the binding file at {}.",
                project_root.display(),
                worktree_git_root.display(),
                path.display()
            ));
        }

        let metadata_path = metadata_path_for_worktree(&worktree_git_root).ok_or_else(|| {
            format!(
                "Could not locate managed-worktree metadata for {}",
                worktree_git_root.display()
            )
        })?;
        let metadata = load_metadata(&metadata_path)?;
        validate_metadata_path(
            &metadata.source_project_root,
            &source_project_root,
            "source project root",
            &metadata_path,
        )?;
        validate_metadata_path(
            &metadata.project_root,
            &project_root,
            "active project root",
            &metadata_path,
        )?;
        validate_metadata_path(
            &metadata.worktree_git_root,
            &worktree_git_root,
            "worktree Git root",
            &metadata_path,
        )?;
        validate_metadata_path(
            &metadata.worktrees_root,
            &worktrees_root,
            "worktree root",
            &metadata_path,
        )?;

        Ok(Some(ResolvedProjectBinding {
            source_project_root,
            project_root,
            repository_url,
            managed_worktree: true,
            worktree_git_root: Some(worktree_git_root),
            worktrees_root: Some(worktrees_root),
        }))
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
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)?;
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

pub async fn prepare_project_root(
    config: &AppConfig,
    reference: &ProjectReference,
) -> Result<PreparedProjectRoot, String> {
    match reference {
        ProjectReference::Path(path) => {
            let (access_root, source_project_root) = resolve_project_root(config, path)?;
            Ok(PreparedProjectRoot {
                access_root,
                source_project_root,
                repository_url: None,
                checkout_commit: None,
                source_matches_checkout: true,
                cloned: false,
                warnings: Vec::new(),
            })
        }
        ProjectReference::Git(repository) => {
            if !config.multi_project {
                return Err(
                    "Project-root selection is disabled. Start codexify with `--multi-project` or set `multiProject` to true."
                        .to_string(),
                );
            }
            let access_root = canonical_access_root(config)?;
            let resolved = resolve_git_project(config, &access_root, repository).await?;
            Ok(PreparedProjectRoot {
                access_root,
                source_project_root: resolved.source_project_root,
                repository_url: Some(resolved.repository_url),
                checkout_commit: resolved.checkout_commit,
                source_matches_checkout: resolved.source_matches_checkout,
                cloned: resolved.cloned,
                warnings: resolved.warnings,
            })
        }
    }
}

pub async fn project_reference_matches(
    config: &AppConfig,
    reference: &ProjectReference,
    source_project_root: &Path,
    repository_url: Option<&str>,
) -> Result<bool, String> {
    match reference {
        ProjectReference::Path(path) => {
            let (_, requested) = resolve_project_root(config, path)?;
            Ok(requested == source_project_root)
        }
        ProjectReference::Git(repository) => {
            if repository_url.is_some_and(|url| repository_url_matches(url, repository)) {
                return Ok(true);
            }
            if repository.has_explicit_checkout() {
                return Ok(false);
            }
            project_matches_repository(config, source_project_root, repository).await
        }
    }
}

fn resolved_from_managed(
    source_project_root: &Path,
    managed: Option<&ManagedWorktree>,
    repository_url: Option<String>,
) -> ResolvedProjectBinding {
    match managed {
        Some(worktree) => ResolvedProjectBinding {
            source_project_root: source_project_root.to_path_buf(),
            project_root: worktree.project_root.clone(),
            repository_url,
            managed_worktree: true,
            worktree_git_root: Some(worktree.worktree_git_root.clone()),
            worktrees_root: Some(worktree.worktrees_root.clone()),
        },
        None => ResolvedProjectBinding {
            source_project_root: source_project_root.to_path_buf(),
            project_root: source_project_root.to_path_buf(),
            repository_url,
            managed_worktree: false,
            worktree_git_root: None,
            worktrees_root: None,
        },
    }
}

fn selection_from_binding(
    access_root: PathBuf,
    binding: ResolvedProjectBinding,
    worktree_mode: WorktreeMode,
    newly_selected: bool,
    cloned: bool,
    scope: ProjectBindingScope,
    warnings: Vec<String>,
) -> ProjectRootSelection {
    ProjectRootSelection {
        access_root,
        source_project_root: binding.source_project_root,
        project_root: binding.project_root,
        repository_url: binding.repository_url,
        cloned,
        managed_worktree: binding.managed_worktree,
        worktree_git_root: binding.worktree_git_root,
        worktrees_root: binding.worktrees_root,
        worktree_mode,
        warnings,
        newly_selected,
        scope,
    }
}

fn canonical_binding_dir(
    stored: &str,
    label: &str,
    binding_path: &Path,
    recovery: &str,
) -> Result<PathBuf, String> {
    let stored = PathBuf::from(stored);
    let canonical = std::fs::canonicalize(&stored).map_err(|error| {
        format!(
            "The {label} in the ChatGPT conversation binding no longer exists or cannot be resolved: {}: {error}. {recovery}",
            stored.display()
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!(
            "The {label} in the ChatGPT conversation binding is no longer a directory: {}. Remove the binding file at {} or start a new chat.",
            canonical.display(),
            binding_path.display()
        ));
    }
    Ok(canonical)
}

fn validate_metadata_path(
    stored: &str,
    expected: &Path,
    label: &str,
    metadata_path: &Path,
) -> Result<(), String> {
    let stored_path = PathBuf::from(stored);
    let canonical = std::fs::canonicalize(&stored_path).map_err(|error| {
        format!(
            "The {label} recorded in managed-worktree metadata {} cannot be resolved: {}: {error}",
            metadata_path.display(),
            stored_path.display()
        )
    })?;
    if canonical != expected {
        return Err(format!(
            "The {label} recorded in managed-worktree metadata {} does not match the conversation binding.",
            metadata_path.display()
        ));
    }
    Ok(())
}

fn canonical_access_root(config: &AppConfig) -> Result<PathBuf, String> {
    std::fs::canonicalize(&config.work_dir).map_err(|error| {
        format!(
            "Could not resolve project access root {}: {error}",
            config.work_dir.display()
        )
    })
}

fn access_root_key(access_root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"codexify/access-root/v1\0");
    hasher.update(access_root.to_string_lossy().as_bytes());
    encode_hex(&hasher.finalize(), 24)
}

async fn acquire_lock(target: &Path) -> Result<BindingLock, String> {
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

    let mut lock_path = target.as_os_str().to_os_string();
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
                        target.display()
                    ));
                }
                tokio::time::sleep(Duration::from_millis(LOCK_RETRY_MS)).await;
            }
            Err(error) => {
                return Err(format!(
                    "Could not lock project binding {}: {error}",
                    target.display()
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
