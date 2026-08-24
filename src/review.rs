//! Project-scoped Git review checkpoints.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::process_env::scrub_untrusted_child_env;
use crate::project_bindings::ConversationIdentity;
use crate::types::AppConfig;

const REVIEW_REF_ROOT: &str = "refs/codexify/review";
const SYNTHETIC_IDENTITY_NAME: &str = "Codexify Review";
const SYNTHETIC_IDENTITY_EMAIL: &str = "review@codexify.local";
const MAX_GIT_ERROR_BYTES: usize = 64 * 1024;

static TRANSPORT_REVIEW_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewBaseline {
    LastReview,
    ProjectOpen,
}

impl ReviewBaseline {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("last_review") {
            "last_review" => Ok(Self::LastReview),
            "project_open" => Ok(Self::ProjectOpen),
            other => Err(format!(
                "since must be `last_review` or `project_open`, got {other:?}"
            )),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::LastReview => "last review",
            Self::ProjectOpen => "project open",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReviewRequest {
    pub since: ReviewBaseline,
    pub advance: bool,
    pub include_patch: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSummary {
    pub files: usize,
    pub additions: u64,
    pub deletions: u64,
    pub binary_files: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewFile {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_path: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u64>,
    pub binary: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewResult {
    pub since: ReviewBaseline,
    pub advance_requested: bool,
    pub checkpoint_advanced: bool,
    pub scope: String,
    pub summary: ReviewSummary,
    pub files: Vec<ReviewFile>,
    pub files_omitted: usize,
    pub patch: String,
    pub patch_included: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_omitted_reason: Option<String>,
    pub warnings: Vec<String>,
}

impl ReviewResult {
    pub fn render_text(&self) -> String {
        let mut lines = Vec::new();
        if self.summary.files == 0 {
            lines.push(format!("No changes since {}.", self.since.label()));
        } else {
            lines.push(format!(
                "Changes since {}: {} file{} (+{} -{}, {} binary).",
                self.since.label(),
                self.summary.files,
                if self.summary.files == 1 { "" } else { "s" },
                self.summary.additions,
                self.summary.deletions,
                self.summary.binary_files
            ));
        }
        lines.push(format!("Repository scope: {}", self.scope));

        for file in &self.files {
            let path = match &file.previous_path {
                Some(previous) => format!("{previous} -> {}", file.path),
                None => file.path.clone(),
            };
            let stats = if file.binary {
                "binary".to_string()
            } else {
                format!(
                    "+{} -{}",
                    file.additions.unwrap_or(0),
                    file.deletions.unwrap_or(0)
                )
            };
            lines.push(format!("- {} {path} ({stats})", file.status));
        }
        if self.files_omitted > 0 {
            lines.push(format!(
                "... {} additional file{} omitted from the result.",
                self.files_omitted,
                if self.files_omitted == 1 { "" } else { "s" }
            ));
        }

        if self.advance_requested {
            lines.push(if self.checkpoint_advanced {
                "Last-review checkpoint advanced.".to_string()
            } else {
                "Last-review checkpoint was not advanced.".to_string()
            });
        }
        lines.extend(
            self.warnings
                .iter()
                .map(|warning| format!("Warning: {warning}")),
        );

        if self.patch_included && !self.patch.is_empty() {
            lines.push(format!(
                "Patch included in structuredContent ({} bytes).",
                self.patch_bytes.unwrap_or(self.patch.len())
            ));
        } else if let Some(reason) = &self.patch_omitted_reason {
            lines.push(format!("Patch omitted: {reason}"));
        }

        lines.join("\n")
    }
}

#[derive(Debug, Clone)]
struct CheckpointPair {
    project_open: String,
    last_review: String,
}

struct TransportCheckpoint {
    pair: CheckpointPair,
    objects: TransportObjectStore,
}

struct TransportObjectStore {
    _temp: tempfile::TempDir,
    objects: PathBuf,
    alternate: PathBuf,
}

impl TransportObjectStore {
    fn environment(&self) -> Vec<(OsString, OsString)> {
        vec![
            (
                OsString::from("GIT_OBJECT_DIRECTORY"),
                self.objects.as_os_str().to_os_string(),
            ),
            (
                OsString::from("GIT_ALTERNATE_OBJECT_DIRECTORIES"),
                self.alternate.as_os_str().to_os_string(),
            ),
        ]
    }
}

#[derive(Clone)]
pub struct TransportReviewState {
    id: String,
    checkpoints: Arc<AsyncMutex<HashMap<String, TransportCheckpoint>>>,
}

impl TransportReviewState {
    pub fn new() -> Self {
        let number = TRANSPORT_REVIEW_COUNTER.fetch_add(1, Ordering::SeqCst);
        Self {
            id: format!("transport-{}-{number}", std::process::id()),
            checkpoints: Arc::new(AsyncMutex::new(HashMap::new())),
        }
    }
}

impl Default for TransportReviewState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub enum ReviewOwner {
    Conversation(String),
    Transport(TransportReviewState),
}

impl ReviewOwner {
    pub fn conversation(identity: &ConversationIdentity) -> Self {
        Self::Conversation(identity.stable_key().to_string())
    }

    pub fn transport(state: TransportReviewState) -> Self {
        Self::Transport(state)
    }

    fn lock_key(&self, workspace_key: &str) -> String {
        match self {
            Self::Conversation(key) => format!("conversation:{key}:{workspace_key}"),
            Self::Transport(state) => format!("{}:{workspace_key}", state.id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewAvailability {
    Ready,
    Unavailable(String),
}

pub struct ReviewMutationGuard {
    _guard: OwnedMutexGuard<()>,
}

#[derive(Default)]
pub struct ReviewCheckpointManager {
    locks: StdMutex<HashMap<String, Weak<AsyncMutex<()>>>>,
}

impl ReviewCheckpointManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn ensure_initialized(
        &self,
        config: &AppConfig,
        owner: ReviewOwner,
    ) -> Result<ReviewAvailability, String> {
        let project_root = canonical_project_root(&config.work_dir)?;
        let workspace_key = hash_path(&project_root);
        let lock = self.scope_lock(&owner.lock_key(&workspace_key));
        let _guard = lock.lock().await;
        Self::ensure_initialized_locked(config, &owner, project_root).await
    }

    pub async fn begin_mutation(
        &self,
        config: &AppConfig,
        owner: ReviewOwner,
    ) -> Result<(ReviewAvailability, ReviewMutationGuard), String> {
        let project_root = canonical_project_root(&config.work_dir)?;
        let workspace_key = hash_path(&project_root);
        let lock = self.scope_lock(&owner.lock_key(&workspace_key));
        let guard = lock.lock_owned().await;
        let availability = Self::ensure_initialized_locked(config, &owner, project_root).await?;
        Ok((availability, ReviewMutationGuard { _guard: guard }))
    }

    async fn ensure_initialized_locked(
        config: &AppConfig,
        owner: &ReviewOwner,
        project_root: PathBuf,
    ) -> Result<ReviewAvailability, String> {
        let workspace = match resolve_workspace(config, project_root).await {
            Ok(workspace) => workspace,
            Err(WorkspaceResolutionError::Unavailable(reason)) => {
                return Ok(ReviewAvailability::Unavailable(reason));
            }
            Err(WorkspaceResolutionError::Failed(error)) => return Err(error),
        };
        match owner {
            ReviewOwner::Conversation(owner_key) => {
                load_or_initialize_persistent(config, &workspace, owner_key).await?;
            }
            ReviewOwner::Transport(state) => {
                let mut checkpoints = state.checkpoints.lock().await;
                if !checkpoints.contains_key(&workspace.key) {
                    let checkpoint = create_transport_checkpoint(config, &workspace).await?;
                    checkpoints.insert(workspace.key.clone(), checkpoint);
                }
            }
        }
        Ok(ReviewAvailability::Ready)
    }

    pub async fn show_changes(
        &self,
        config: &AppConfig,
        owner: ReviewOwner,
        request: ReviewRequest,
    ) -> Result<ReviewResult, String> {
        let project_root = canonical_project_root(&config.work_dir)?;
        let workspace = resolve_workspace(config, project_root)
            .await
            .map_err(|error| error.to_string())?;
        let lock = self.scope_lock(&owner.lock_key(&workspace.key));
        let _guard = lock.lock().await;

        match owner {
            ReviewOwner::Conversation(owner_key) => {
                let pair = load_or_initialize_persistent(config, &workspace, &owner_key).await?;
                let current = create_snapshot(config, &workspace, &[]).await?;
                let mut result = compare(config, &workspace, &pair, &current, request, &[]).await?;
                if request.advance {
                    let refs = checkpoint_refs(&workspace.key, &owner_key);
                    if update_ref_compare_and_swap(
                        config,
                        &workspace.git_root,
                        &refs.last_review,
                        &current,
                        &pair.last_review,
                    )
                    .await?
                    {
                        result.checkpoint_advanced = true;
                    } else {
                        result.warnings.push(
                            "the last-review checkpoint changed concurrently; call show_changes again before advancing"
                                .to_string(),
                        );
                    }
                }
                Ok(result)
            }
            ReviewOwner::Transport(state) => {
                let mut checkpoints = state.checkpoints.lock().await;
                if !checkpoints.contains_key(&workspace.key) {
                    let checkpoint = create_transport_checkpoint(config, &workspace).await?;
                    checkpoints.insert(workspace.key.clone(), checkpoint);
                }
                let checkpoint = checkpoints
                    .get(&workspace.key)
                    .ok_or_else(|| "transport review checkpoint disappeared".to_string())?;
                let pair = checkpoint.pair.clone();
                let environment = checkpoint.objects.environment();
                let current = create_snapshot(config, &workspace, &environment).await?;
                let mut result =
                    compare(config, &workspace, &pair, &current, request, &environment).await?;
                if request.advance
                    && let Some(stored) = checkpoints.get_mut(&workspace.key)
                {
                    stored.pair.last_review = current;
                    result.checkpoint_advanced = true;
                }
                Ok(result)
            }
        }
    }

    fn scope_lock(&self, key: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.locks.lock().unwrap();
        locks.retain(|_, weak| weak.strong_count() > 0);
        if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(key.to_string(), Arc::downgrade(&lock));
        lock
    }
}

struct ReviewWorkspace {
    git_root: PathBuf,
    pathspec: PathBuf,
    scope: String,
    key: String,
}

struct CheckpointRefs {
    project_open: String,
    last_review: String,
}

#[derive(Debug)]
enum WorkspaceResolutionError {
    Unavailable(String),
    Failed(String),
}

impl std::fmt::Display for WorkspaceResolutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(reason) | Self::Failed(reason) => formatter.write_str(reason),
        }
    }
}

fn is_not_git_worktree(stderr: &[u8]) -> bool {
    let detail = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    [
        "not a git repository",
        "must be run in a work tree",
        "must be run in a worktree",
        "not a git work tree",
        "not a git worktree",
    ]
    .iter()
    .any(|message| detail.contains(message))
}

fn checkpoint_refs(workspace_key: &str, owner_key: &str) -> CheckpointRefs {
    let prefix = format!("{REVIEW_REF_ROOT}/{workspace_key}/{owner_key}");
    CheckpointRefs {
        project_open: format!("{prefix}/project-open"),
        last_review: format!("{prefix}/last-review"),
    }
}

fn canonical_project_root(path: &Path) -> Result<PathBuf, String> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("could not resolve project root {}: {error}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!(
            "project root is not a directory: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

async fn resolve_workspace(
    config: &AppConfig,
    project_root: PathBuf,
) -> Result<ReviewWorkspace, WorkspaceResolutionError> {
    let args = strings(&["rev-parse", "--show-toplevel"]);
    let output = git_output(config, &project_root, &args, &[])
        .await
        .map_err(WorkspaceResolutionError::Failed)?;
    if !output.status.success() {
        let error = git_failure("git rev-parse", &output.stderr, output.status.code());
        if is_not_git_worktree(&output.stderr) {
            return Err(WorkspaceResolutionError::Unavailable(format!(
                "show_changes requires a Git worktree: {error}"
            )));
        }
        return Err(WorkspaceResolutionError::Failed(error));
    }
    let top = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let git_root = std::fs::canonicalize(&top).map_err(|error| {
        WorkspaceResolutionError::Failed(format!("could not resolve Git root {top}: {error}"))
    })?;
    let relative = project_root.strip_prefix(&git_root).map_err(|_| {
        WorkspaceResolutionError::Failed(format!(
            "project root {} is outside Git root {}",
            project_root.display(),
            git_root.display()
        ))
    })?;
    let pathspec = if relative.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        relative.to_path_buf()
    };
    let scope = if relative.as_os_str().is_empty() {
        ".".to_string()
    } else {
        relative.to_string_lossy().replace('\\', "/")
    };
    Ok(ReviewWorkspace {
        key: hash_path(&project_root),
        git_root,
        pathspec,
        scope,
    })
}

fn hash_path(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"codexify/review-project/v1\0");
    hasher.update(path.to_string_lossy().as_bytes());
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

async fn load_or_initialize_persistent(
    config: &AppConfig,
    workspace: &ReviewWorkspace,
    owner_key: &str,
) -> Result<CheckpointPair, String> {
    let refs = checkpoint_refs(&workspace.key, owner_key);
    let project_open = read_ref(config, &workspace.git_root, &refs.project_open).await?;
    let last_review = read_ref(config, &workspace.git_root, &refs.last_review).await?;

    match (project_open, last_review) {
        (Some(project_open), Some(last_review)) => Ok(CheckpointPair {
            project_open,
            last_review,
        }),
        (Some(project_open), None) => {
            create_ref_if_absent(
                config,
                &workspace.git_root,
                &refs.last_review,
                &project_open,
            )
            .await?;
            let last_review = read_ref(config, &workspace.git_root, &refs.last_review)
                .await?
                .ok_or_else(|| "could not initialise the last-review checkpoint".to_string())?;
            Ok(CheckpointPair {
                project_open,
                last_review,
            })
        }
        (None, Some(_)) => Err(
            "review checkpoint state is inconsistent: last-review exists without project-open"
                .to_string(),
        ),
        (None, None) => {
            let snapshot = create_snapshot(config, workspace, &[]).await?;
            create_ref_if_absent(config, &workspace.git_root, &refs.project_open, &snapshot)
                .await?;
            let project_open = read_ref(config, &workspace.git_root, &refs.project_open)
                .await?
                .ok_or_else(|| "could not initialise the project-open checkpoint".to_string())?;
            create_ref_if_absent(
                config,
                &workspace.git_root,
                &refs.last_review,
                &project_open,
            )
            .await?;
            let last_review = read_ref(config, &workspace.git_root, &refs.last_review)
                .await?
                .ok_or_else(|| "could not initialise the last-review checkpoint".to_string())?;
            Ok(CheckpointPair {
                project_open,
                last_review,
            })
        }
    }
}

async fn create_transport_checkpoint(
    config: &AppConfig,
    workspace: &ReviewWorkspace,
) -> Result<TransportCheckpoint, String> {
    let alternate = git_checked(
        config,
        &workspace.git_root,
        strings(&["rev-parse", "--git-path", "objects"]),
        &[],
    )
    .await?;
    let alternate = PathBuf::from(String::from_utf8_lossy(&alternate).trim().to_string());
    let alternate = if alternate.is_absolute() {
        alternate
    } else {
        workspace.git_root.join(alternate)
    };
    let alternate = std::fs::canonicalize(&alternate).map_err(|error| {
        format!(
            "could not resolve Git object directory {}: {error}",
            alternate.display()
        )
    })?;
    let temp = tempfile::tempdir()
        .map_err(|error| format!("could not create transport review object store: {error}"))?;
    let objects = temp.path().join("objects");
    std::fs::create_dir(&objects)
        .map_err(|error| format!("could not create transport review object directory: {error}"))?;
    let store = TransportObjectStore {
        _temp: temp,
        objects,
        alternate,
    };
    let environment = store.environment();
    let snapshot = create_snapshot(config, workspace, &environment).await?;
    Ok(TransportCheckpoint {
        pair: CheckpointPair {
            project_open: snapshot.clone(),
            last_review: snapshot,
        },
        objects: store,
    })
}

async fn create_snapshot(
    config: &AppConfig,
    workspace: &ReviewWorkspace,
    object_environment: &[(OsString, OsString)],
) -> Result<String, String> {
    let temp = tempfile::tempdir()
        .map_err(|error| format!("could not create temporary review index: {error}"))?;
    let index = temp.path().join("index");
    let environment = snapshot_environment(&index, object_environment);

    let scoped_index_entries = git_checked(
        config,
        &workspace.git_root,
        vec![
            OsString::from("--literal-pathspecs"),
            OsString::from("ls-files"),
            OsString::from("--stage"),
            OsString::from("-z"),
            OsString::from("--"),
            workspace.pathspec.as_os_str().to_os_string(),
        ],
        object_environment,
    )
    .await?;
    git_checked(
        config,
        &workspace.git_root,
        strings(&["read-tree", "--empty"]),
        &environment,
    )
    .await?;
    if !scoped_index_entries.is_empty() {
        git_checked_input(
            config,
            &workspace.git_root,
            strings(&["update-index", "-z", "--index-info"]),
            &environment,
            &scoped_index_entries,
        )
        .await?;
    }
    let add_output = git_output(
        config,
        &workspace.git_root,
        &[
            OsString::from("--literal-pathspecs"),
            OsString::from("add"),
            OsString::from("-A"),
            OsString::from("--"),
            workspace.pathspec.as_os_str().to_os_string(),
        ],
        &environment,
    )
    .await?;
    if !add_output.status.success()
        && !(add_output.status.code() == Some(1)
            && String::from_utf8_lossy(&add_output.stderr).contains("paths are ignored"))
    {
        return Err(git_failure(
            "git add",
            &add_output.stderr,
            add_output.status.code(),
        ));
    }
    let tree = git_checked(
        config,
        &workspace.git_root,
        strings(&["write-tree"]),
        &environment,
    )
    .await?;
    let tree = String::from_utf8_lossy(&tree).trim().to_string();
    let commit = git_checked(
        config,
        &workspace.git_root,
        vec![
            OsString::from("commit-tree"),
            OsString::from(tree),
            OsString::from("-m"),
            OsString::from("Codexify review snapshot"),
        ],
        &environment,
    )
    .await?;
    let commit = String::from_utf8_lossy(&commit).trim().to_string();
    if commit.is_empty() {
        return Err("Git returned an empty review snapshot id".to_string());
    }
    Ok(commit)
}

fn snapshot_environment(
    index: &Path,
    object_environment: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    let mut environment = vec![
        (
            OsString::from("GIT_INDEX_FILE"),
            index.as_os_str().to_os_string(),
        ),
        (
            OsString::from("GIT_AUTHOR_NAME"),
            OsString::from(SYNTHETIC_IDENTITY_NAME),
        ),
        (
            OsString::from("GIT_AUTHOR_EMAIL"),
            OsString::from(SYNTHETIC_IDENTITY_EMAIL),
        ),
        (
            OsString::from("GIT_COMMITTER_NAME"),
            OsString::from(SYNTHETIC_IDENTITY_NAME),
        ),
        (
            OsString::from("GIT_COMMITTER_EMAIL"),
            OsString::from(SYNTHETIC_IDENTITY_EMAIL),
        ),
        (
            OsString::from("GIT_AUTHOR_DATE"),
            OsString::from("@0 +0000"),
        ),
        (
            OsString::from("GIT_COMMITTER_DATE"),
            OsString::from("@0 +0000"),
        ),
    ];
    environment.extend(object_environment.iter().cloned());
    environment
}

async fn compare(
    config: &AppConfig,
    workspace: &ReviewWorkspace,
    pair: &CheckpointPair,
    current: &str,
    request: ReviewRequest,
    object_environment: &[(OsString, OsString)],
) -> Result<ReviewResult, String> {
    let baseline = match request.since {
        ReviewBaseline::LastReview => &pair.last_review,
        ReviewBaseline::ProjectOpen => &pair.project_open,
    };
    let name_status = git_checked(
        config,
        &workspace.git_root,
        diff_args("--name-status", baseline, current, &workspace.pathspec),
        object_environment,
    )
    .await?;
    let numstat = git_checked(
        config,
        &workspace.git_root,
        diff_args("--numstat", baseline, current, &workspace.pathspec),
        object_environment,
    )
    .await?;
    let names = parse_name_status(&name_status)?;
    let stats = parse_numstat(&numstat)?;
    let mut files = merge_records(workspace, names, stats)?;
    let summary = summarize(&files);
    let max_files = crate::output_budget::entry_budget(config);
    let files_omitted = if max_files == 0 || files.len() <= max_files {
        0
    } else {
        files.len() - max_files
    };
    if files_omitted > 0 {
        files.truncate(max_files);
    }

    let patch_result = if !request.include_patch {
        PatchResult::omitted("disabled by the show_changes request")
    } else if config.review.max_patch_bytes == 0 {
        PatchResult::omitted("disabled by review.maxPatchBytes=0")
    } else {
        git_patch_bounded(
            config,
            workspace,
            baseline,
            current,
            config.review.max_patch_bytes,
            object_environment,
        )
        .await?
    };

    Ok(ReviewResult {
        since: request.since,
        advance_requested: request.advance,
        checkpoint_advanced: false,
        scope: workspace.scope.clone(),
        summary,
        files,
        files_omitted,
        patch: patch_result.patch,
        patch_included: patch_result.included,
        patch_bytes: patch_result.bytes,
        patch_omitted_reason: patch_result.omitted_reason,
        warnings: Vec::new(),
    })
}

fn diff_args(kind: &str, baseline: &str, current: &str, pathspec: &Path) -> Vec<OsString> {
    vec![
        OsString::from("--literal-pathspecs"),
        OsString::from("diff"),
        OsString::from("--no-ext-diff"),
        OsString::from("--no-textconv"),
        OsString::from("--find-renames"),
        OsString::from(kind),
        OsString::from("-z"),
        OsString::from(baseline),
        OsString::from(current),
        OsString::from("--"),
        pathspec.as_os_str().to_os_string(),
    ]
}

#[derive(Debug)]
struct NameRecord {
    status: String,
    previous_path: Option<String>,
    path: String,
}

#[derive(Debug)]
struct StatRecord {
    additions: Option<u64>,
    deletions: Option<u64>,
    previous_path: Option<String>,
    path: String,
}

fn parse_name_status(output: &[u8]) -> Result<Vec<NameRecord>, String> {
    let tokens = nul_tokens(output);
    let mut records = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let status = token_string(tokens[index]);
        index += 1;
        if status.is_empty() {
            continue;
        }
        if status.starts_with('R') || status.starts_with('C') {
            let previous = tokens
                .get(index)
                .ok_or_else(|| "truncated Git rename status".to_string())?;
            let path = tokens
                .get(index + 1)
                .ok_or_else(|| "truncated Git rename destination".to_string())?;
            records.push(NameRecord {
                status,
                previous_path: Some(token_string(previous)),
                path: token_string(path),
            });
            index += 2;
        } else {
            let path = tokens
                .get(index)
                .ok_or_else(|| "truncated Git name-status record".to_string())?;
            records.push(NameRecord {
                status,
                previous_path: None,
                path: token_string(path),
            });
            index += 1;
        }
    }
    Ok(records)
}

fn parse_numstat(output: &[u8]) -> Result<Vec<StatRecord>, String> {
    let mut records = Vec::new();
    let mut cursor = 0;
    while cursor < output.len() {
        let end = output[cursor..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|offset| cursor + offset)
            .unwrap_or(output.len());
        let header = &output[cursor..end];
        cursor = end.saturating_add(1);
        if header.is_empty() {
            continue;
        }
        let mut fields = header.splitn(3, |byte| *byte == b'\t');
        let additions = parse_stat(fields.next())?;
        let deletions = parse_stat(fields.next())?;
        let path_field = fields
            .next()
            .ok_or_else(|| "invalid Git numstat record".to_string())?;
        if !path_field.is_empty() {
            records.push(StatRecord {
                additions,
                deletions,
                previous_path: None,
                path: token_string(path_field),
            });
            continue;
        }

        let previous = take_nul_token(output, &mut cursor)
            .ok_or_else(|| "truncated Git numstat rename source".to_string())?;
        let path = take_nul_token(output, &mut cursor)
            .ok_or_else(|| "truncated Git numstat rename destination".to_string())?;
        records.push(StatRecord {
            additions,
            deletions,
            previous_path: Some(token_string(previous)),
            path: token_string(path),
        });
    }
    Ok(records)
}

fn parse_stat(value: Option<&[u8]>) -> Result<Option<u64>, String> {
    let value = value.ok_or_else(|| "missing Git numstat value".to_string())?;
    if value == b"-" {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(value);
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| format!("invalid Git numstat value {value:?}"))
}

fn nul_tokens(output: &[u8]) -> Vec<&[u8]> {
    output
        .split(|byte| *byte == 0)
        .filter(|token| !token.is_empty())
        .collect()
}

fn take_nul_token<'a>(output: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    if *cursor > output.len() {
        return None;
    }
    let end = output[*cursor..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|offset| *cursor + offset)
        .unwrap_or(output.len());
    let token = &output[*cursor..end];
    *cursor = end.saturating_add(1);
    Some(token)
}

fn token_string(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}

fn merge_records(
    workspace: &ReviewWorkspace,
    names: Vec<NameRecord>,
    stats: Vec<StatRecord>,
) -> Result<Vec<ReviewFile>, String> {
    if names.len() != stats.len() {
        return Err(format!(
            "Git returned {} name records but {} stat records",
            names.len(),
            stats.len()
        ));
    }
    names
        .into_iter()
        .zip(stats)
        .map(|(name, stat)| {
            if name.path != stat.path || name.previous_path != stat.previous_path {
                return Err("Git name-status and numstat records do not align".to_string());
            }
            let binary = stat.additions.is_none() || stat.deletions.is_none();
            Ok(ReviewFile {
                path: project_relative_path(workspace, &name.path)?,
                previous_path: name
                    .previous_path
                    .as_deref()
                    .map(|path| project_relative_path(workspace, path))
                    .transpose()?,
                status: status_name(&name.status).to_string(),
                additions: stat.additions,
                deletions: stat.deletions,
                binary,
            })
        })
        .collect()
}

fn project_relative_path(
    workspace: &ReviewWorkspace,
    repository_path: &str,
) -> Result<String, String> {
    if workspace.scope == "." {
        return Ok(repository_path.to_string());
    }
    let prefix = format!("{}/", workspace.scope);
    repository_path
        .strip_prefix(&prefix)
        .map(String::from)
        .ok_or_else(|| {
            format!("Git returned a path outside the logical review scope: {repository_path}")
        })
}

fn status_name(status: &str) -> &'static str {
    match status.as_bytes().first().copied() {
        Some(b'A') => "added",
        Some(b'M') => "modified",
        Some(b'D') => "deleted",
        Some(b'R') => "renamed",
        Some(b'C') => "copied",
        Some(b'T') => "type_changed",
        Some(b'U') => "unmerged",
        Some(b'X') => "unknown",
        Some(b'B') => "broken_pairing",
        _ => "unknown",
    }
}

fn summarize(files: &[ReviewFile]) -> ReviewSummary {
    files
        .iter()
        .fold(ReviewSummary::default(), |mut summary, file| {
            summary.files += 1;
            summary.additions += file.additions.unwrap_or(0);
            summary.deletions += file.deletions.unwrap_or(0);
            summary.binary_files += usize::from(file.binary);
            summary
        })
}

struct PatchResult {
    patch: String,
    included: bool,
    bytes: Option<usize>,
    omitted_reason: Option<String>,
}

impl PatchResult {
    fn omitted(reason: impl Into<String>) -> Self {
        Self {
            patch: String::new(),
            included: false,
            bytes: None,
            omitted_reason: Some(reason.into()),
        }
    }
}

async fn git_patch_bounded(
    config: &AppConfig,
    workspace: &ReviewWorkspace,
    baseline: &str,
    current: &str,
    max_bytes: usize,
    object_environment: &[(OsString, OsString)],
) -> Result<PatchResult, String> {
    let mut args = vec![
        OsString::from("--literal-pathspecs"),
        OsString::from("diff"),
        OsString::from("--no-ext-diff"),
        OsString::from("--no-textconv"),
        OsString::from("--find-renames"),
        OsString::from("--binary"),
        OsString::from("--full-index"),
        OsString::from("--no-color"),
    ];
    if workspace.scope != "." {
        args.push(OsString::from(format!("--relative={}", workspace.scope)));
    }
    args.extend([
        OsString::from(baseline),
        OsString::from(current),
        OsString::from("--"),
        workspace.pathspec.as_os_str().to_os_string(),
    ]);
    let mut command = git_command(config, &workspace.git_root, &args, object_environment);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start git diff: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "git diff stdout was unavailable".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "git diff stderr was unavailable".to_string())?;
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let _ = stderr
            .take(MAX_GIT_ERROR_BYTES as u64)
            .read_to_end(&mut bytes)
            .await;
        bytes
    });

    let mut bytes = Vec::new();
    stdout
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("failed to read git diff: {error}"))?;
    let oversized = bytes.len() > max_bytes;
    if oversized {
        let _ = child.kill().await;
    }
    let status = child
        .wait()
        .await
        .map_err(|error| format!("failed to wait for git diff: {error}"))?;
    let stderr = stderr_task.await.unwrap_or_default();

    if oversized {
        return Ok(PatchResult::omitted(format!(
            "exceeds review.maxPatchBytes ({max_bytes} bytes)"
        )));
    }
    if !status.success() {
        return Err(git_failure("git diff", &stderr, status.code()));
    }
    let patch = String::from_utf8_lossy(&bytes).into_owned();
    Ok(PatchResult {
        patch,
        included: true,
        bytes: Some(bytes.len()),
        omitted_reason: None,
    })
}

async fn read_ref(
    config: &AppConfig,
    git_root: &Path,
    reference: &str,
) -> Result<Option<String>, String> {
    let args = vec![
        OsString::from("rev-parse"),
        OsString::from("--verify"),
        OsString::from("--quiet"),
        OsString::from(format!("{reference}^{{commit}}")),
    ];
    let output = git_output(config, git_root, &args, &[]).await?;
    if output.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ));
    }
    if output.status.code() == Some(1) && output.stderr.is_empty() {
        return Ok(None);
    }
    Err(git_failure(
        "git rev-parse",
        &output.stderr,
        output.status.code(),
    ))
}

async fn create_ref_if_absent(
    config: &AppConfig,
    git_root: &Path,
    reference: &str,
    value: &str,
) -> Result<(), String> {
    let output = git_output(
        config,
        git_root,
        &[
            OsString::from("update-ref"),
            OsString::from(reference),
            OsString::from(value),
            OsString::new(),
        ],
        &[],
    )
    .await?;
    if output.status.success() || read_ref(config, git_root, reference).await?.is_some() {
        return Ok(());
    }
    Err(git_failure(
        "git update-ref",
        &output.stderr,
        output.status.code(),
    ))
}

async fn update_ref_compare_and_swap(
    config: &AppConfig,
    git_root: &Path,
    reference: &str,
    value: &str,
    expected: &str,
) -> Result<bool, String> {
    let output = git_output(
        config,
        git_root,
        &[
            OsString::from("update-ref"),
            OsString::from(reference),
            OsString::from(value),
            OsString::from(expected),
        ],
        &[],
    )
    .await?;
    if output.status.success() {
        return Ok(true);
    }
    let current = read_ref(config, git_root, reference).await?;
    if current.as_deref() != Some(expected) {
        return Ok(false);
    }
    Err(git_failure(
        "git update-ref",
        &output.stderr,
        output.status.code(),
    ))
}

fn strings(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn git_command(
    config: &AppConfig,
    cwd: &Path,
    args: &[OsString],
    environment: &[(OsString, OsString)],
) -> Command {
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    scrub_untrusted_child_env(&mut command, config);
    // Keep diagnostics parseable and stop read-only Git commands from refreshing
    // the user's real index as a side effect.
    command
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0");
    command.envs(environment.iter().cloned());
    command
}

async fn git_output(
    config: &AppConfig,
    cwd: &Path,
    args: &[OsString],
    environment: &[(OsString, OsString)],
) -> Result<Output, String> {
    git_command(config, cwd, args, environment)
        .output()
        .await
        .map_err(|error| format!("failed to run git: {error}"))
}

async fn git_checked_input(
    config: &AppConfig,
    cwd: &Path,
    args: Vec<OsString>,
    environment: &[(OsString, OsString)],
    input: &[u8],
) -> Result<Vec<u8>, String> {
    let command_name = args
        .first()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "git".to_string());
    let mut command = git_command(config, cwd, &args, environment);
    command.stdin(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to run git: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input)
            .await
            .map_err(|error| format!("failed to write git input: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| format!("failed to wait for git: {error}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(git_failure(
            &format!("git {command_name}"),
            &output.stderr,
            output.status.code(),
        ))
    }
}

async fn git_checked(
    config: &AppConfig,
    cwd: &Path,
    args: Vec<OsString>,
    environment: &[(OsString, OsString)],
) -> Result<Vec<u8>, String> {
    let command_name = args
        .first()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "git".to_string());
    let output = git_output(config, cwd, &args, environment).await?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(git_failure(
            &format!("git {command_name}"),
            &output.stderr,
            output.status.code(),
        ))
    }
}

fn git_failure(command: &str, stderr: &[u8], code: Option<i32>) -> String {
    let detail = String::from_utf8_lossy(stderr).trim().to_string();
    if detail.is_empty() {
        format!("{command} failed with exit code {}", code.unwrap_or(-1))
    } else {
        format!("{command} failed: {detail}")
    }
}
