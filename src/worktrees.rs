use std::collections::{BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Output;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::timeout;

use crate::exec_sessions::{resolve_shell, wrap_for_shell};
use crate::process_env::scrub_untrusted_child_env;
use crate::types::{AppConfig, WorktreeUpstreamRefreshMode};

const METADATA_VERSION: u32 = 1;
const METADATA_FILENAME: &str = ".codexify-worktree.json";
const LEGACY_METADATA_FILENAME: &str = ".codex-free-worktree.json";
const LOCAL_ENVIRONMENT_CONFIG_KEY: &str = "codex.localEnvironmentConfigPath";
const NO_LOCAL_ENVIRONMENT: &str = "__none__";
const MAX_COMMAND_OUTPUT_BYTES: usize = 16_384;
const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(120);
const UPSTREAM_REFRESH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedWorktree {
    pub source_project_root: PathBuf,
    pub project_root: PathBuf,
    pub source_git_root: PathBuf,
    pub worktree_git_root: PathBuf,
    pub worktrees_root: PathBuf,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedWorktreeMetadata {
    pub version: u32,
    pub source_project_root: String,
    pub project_root: String,
    pub source_git_root: String,
    pub worktree_git_root: String,
    pub worktrees_root: String,
    pub created_at_ms: u64,
}

#[derive(Debug, Deserialize)]
struct LocalEnvironmentConfig {
    #[serde(default = "default_environment_version")]
    version: u32,
    name: String,
    setup: EnvironmentScript,
}

#[derive(Debug, Deserialize)]
struct EnvironmentScript {
    script: String,
    darwin: Option<PlatformScript>,
    linux: Option<PlatformScript>,
    win32: Option<PlatformScript>,
}

#[derive(Debug, Deserialize)]
struct PlatformScript {
    script: String,
}

fn default_environment_version() -> u32 {
    1
}

pub async fn create_managed_worktree(
    config: &AppConfig,
    source_project_root: &Path,
) -> Result<ManagedWorktree, String> {
    create_managed_worktree_at(config, source_project_root, None).await
}

pub async fn create_managed_worktree_at(
    config: &AppConfig,
    source_project_root: &Path,
    requested_commit: Option<&str>,
) -> Result<ManagedWorktree, String> {
    let source_project_root = fs::canonicalize(source_project_root).map_err(|error| {
        format!(
            "Could not resolve project root {} before creating a worktree: {error}",
            source_project_root.display()
        )
    })?;
    let source_git_root = resolve_git_root(config, &source_project_root).await?;
    let workspace_relative = source_project_root
        .strip_prefix(&source_git_root)
        .map_err(|_| {
            format!(
                "Selected project {} is outside its Git root {}",
                source_project_root.display(),
                source_git_root.display()
            )
        })?
        .to_path_buf();

    let worktrees_root = prepare_worktrees_root(&config.worktrees.root)?;
    let (shard_root, worktree_git_root) =
        allocate_worktree_path(&worktrees_root, &source_git_root)?;
    let project_root = worktree_git_root.join(&workspace_relative);
    let mut warnings = Vec::new();
    let starting_commit = match requested_commit {
        Some(commit) => {
            required_git_text(
                config,
                &source_git_root,
                &[
                    OsString::from("rev-parse"),
                    OsString::from("--verify"),
                    OsString::from(format!("{commit}^{{commit}}")),
                ],
                "resolve the requested worktree commit",
            )
            .await?
        }
        None => resolve_starting_commit(config, &source_git_root, &mut warnings).await?,
    };
    let filter_overrides = safe_attribute_filter_overrides(config, &source_git_root).await?;

    let creation = async {
        let mut args = filter_overrides;
        args.extend([
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("--detach"),
            worktree_git_root.as_os_str().to_owned(),
            OsString::from(starting_commit),
        ]);
        git_success(
            config,
            &source_git_root,
            &args,
            &[],
            DEFAULT_GIT_TIMEOUT,
            "git worktree add",
        )
        .await?;

        let metadata = ManagedWorktreeMetadata {
            version: METADATA_VERSION,
            source_project_root: source_project_root.to_string_lossy().into_owned(),
            project_root: project_root.to_string_lossy().into_owned(),
            source_git_root: source_git_root.to_string_lossy().into_owned(),
            worktree_git_root: worktree_git_root.to_string_lossy().into_owned(),
            worktrees_root: worktrees_root.to_string_lossy().into_owned(),
            created_at_ms: now_ms(),
        };
        write_metadata(&shard_root, &metadata)?;

        copy_ignored_agent_overrides(
            config,
            &source_git_root,
            &worktree_git_root,
            &worktrees_root,
        )
        .await?;
        copy_worktree_includes(
            config,
            &source_git_root,
            &worktree_git_root,
            &worktrees_root,
        )
        .await?;
        fs::create_dir_all(&project_root).map_err(|error| {
            format!(
                "Could not create selected workspace directory {} in the worktree: {error}",
                project_root.display()
            )
        })?;

        // The setup script runs an arbitrary command outside the
        // `allowedCommands`/exec policy, and both the environment file and its
        // script path are selectable through the source repository's local git
        // config — so an untrusted repo could plant a script that executes on
        // the next binding. Gate the whole prepare-and-run path behind an
        // explicit opt-in that defaults off; when disabled we neither copy the
        // environment into the worktree nor run anything.
        if config.worktrees.allow_setup_script
            && let Some(environment_path) =
                prepare_local_environment(config, &source_git_root, &worktree_git_root, &shard_root)
                    .await?
        {
            run_setup_script(
                config,
                &environment_path,
                &source_project_root,
                &project_root,
                &worktree_git_root,
            )
            .await?;
        }

        Ok::<(), String>(())
    }
    .await;

    if let Err(error) = creation {
        cleanup_failed_creation(config, &source_git_root, &worktree_git_root, &shard_root).await;
        return Err(error);
    }

    Ok(ManagedWorktree {
        source_project_root,
        project_root,
        source_git_root,
        worktree_git_root,
        worktrees_root,
        warnings,
    })
}

pub async fn rollback_managed_worktree(
    config: &AppConfig,
    worktree: &ManagedWorktree,
) -> Result<(), String> {
    let shard_root = worktree.worktree_git_root.parent().ok_or_else(|| {
        format!(
            "Could not determine the managed-worktree directory for {}",
            worktree.worktree_git_root.display()
        )
    })?;
    let output = git_output(
        config,
        &worktree.source_git_root,
        &[
            OsString::from("worktree"),
            OsString::from("remove"),
            OsString::from("--force"),
            worktree.worktree_git_root.as_os_str().to_owned(),
        ],
        &[],
        DEFAULT_GIT_TIMEOUT,
    )
    .await?;
    if !output.status.success() {
        return Err(format!(
            "Could not roll back managed worktree {}: {}",
            worktree.worktree_git_root.display(),
            bounded_output(&output)
        ));
    }
    fs::remove_dir_all(shard_root).map_err(|error| {
        format!(
            "Could not remove managed-worktree directory {} after rollback: {error}",
            shard_root.display()
        )
    })?;
    Ok(())
}

pub fn metadata_path_for_worktree(worktree_git_root: &Path) -> Option<PathBuf> {
    worktree_git_root
        .parent()
        .map(|parent| parent.join(METADATA_FILENAME))
}

pub fn load_metadata(path: &Path) -> Result<ManagedWorktreeMetadata, String> {
    let (raw, source_path, legacy_source) = match fs::read_to_string(path) {
        Ok(raw) => (raw, path.to_path_buf(), None),
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && path.file_name() == Some(OsStr::new(METADATA_FILENAME)) =>
        {
            let legacy = path.with_file_name(LEGACY_METADATA_FILENAME);
            let raw = fs::read_to_string(&legacy).map_err(|legacy_error| {
                if legacy_error.kind() == std::io::ErrorKind::NotFound {
                    format!(
                        "Could not read managed-worktree metadata {}: {error}",
                        path.display()
                    )
                } else {
                    format!(
                        "Could not read legacy managed-worktree metadata {}: {legacy_error}",
                        legacy.display()
                    )
                }
            })?;
            (raw, legacy.clone(), Some(legacy))
        }
        Err(error) => {
            return Err(format!(
                "Could not read managed-worktree metadata {}: {error}",
                path.display()
            ));
        }
    };
    let metadata: ManagedWorktreeMetadata = serde_json::from_str(&raw).map_err(|error| {
        format!(
            "Managed-worktree metadata {} is invalid: {error}",
            source_path.display()
        )
    })?;
    if metadata.version != METADATA_VERSION {
        return Err(format!(
            "Managed-worktree metadata {} uses unsupported version {}",
            source_path.display(),
            metadata.version
        ));
    }
    if let Some(legacy) = legacy_source {
        let _ = fs::rename(legacy, path);
    }
    Ok(metadata)
}

pub async fn cleanup_managed_worktrees(
    config: &AppConfig,
    referenced_project_roots: &HashSet<PathBuf>,
) -> Vec<String> {
    if !config.worktrees.auto_cleanup_enabled {
        return Vec::new();
    }
    let root = match prepare_worktrees_root(&config.worktrees.root) {
        Ok(root) => root,
        Err(error) => return vec![error],
    };
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) => {
            return vec![format!(
                "Could not inspect managed-worktree root {}: {error}",
                root.display()
            )];
        }
    };

    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let shard = entry.path();
        let Some(name) = shard.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if !is_native_shard_name(name) {
            continue;
        }
        let metadata_path = shard.join(METADATA_FILENAME);
        let Ok(metadata) = load_metadata(&metadata_path) else {
            continue;
        };
        let project_root = PathBuf::from(&metadata.project_root);
        if referenced_project_roots.contains(&project_root) {
            continue;
        }
        candidates.push((metadata.created_at_ms, shard, metadata));
    }

    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    let mut warnings = Vec::new();
    for (_, shard, metadata) in candidates.into_iter().skip(config.worktrees.keep_count) {
        let source_git_root = PathBuf::from(&metadata.source_git_root);
        let worktree_git_root = PathBuf::from(&metadata.worktree_git_root);
        if !worktree_git_root.exists() {
            if let Err(error) = fs::remove_dir_all(&shard) {
                warnings.push(format!(
                    "Could not remove stale managed-worktree metadata directory {}: {error}",
                    shard.display()
                ));
            }
            continue;
        }

        let status = match git_output(
            config,
            &worktree_git_root,
            &[OsString::from("status"), OsString::from("--porcelain")],
            &[],
            DEFAULT_GIT_TIMEOUT,
        )
        .await
        {
            Ok(output) if output.status.success() => output,
            Ok(output) => {
                warnings.push(format!(
                    "Could not inspect stale managed worktree {}: {}",
                    worktree_git_root.display(),
                    bounded_output(&output)
                ));
                continue;
            }
            Err(error) => {
                warnings.push(error);
                continue;
            }
        };
        if !status.stdout.is_empty() {
            continue;
        }

        let result = git_output(
            config,
            &source_git_root,
            &[
                OsString::from("worktree"),
                OsString::from("remove"),
                worktree_git_root.as_os_str().to_owned(),
            ],
            &[],
            DEFAULT_GIT_TIMEOUT,
        )
        .await;
        match result {
            Ok(output) if output.status.success() => {
                let _ = fs::remove_dir_all(&shard);
            }
            Ok(output) => warnings.push(format!(
                "Could not remove stale managed worktree {}: {}",
                worktree_git_root.display(),
                bounded_output(&output)
            )),
            Err(error) => warnings.push(error),
        }
    }
    warnings
}

async fn resolve_git_root(config: &AppConfig, project_root: &Path) -> Result<PathBuf, String> {
    let output = git_success(
        config,
        project_root,
        &[
            OsString::from("rev-parse"),
            OsString::from("--path-format=absolute"),
            OsString::from("--show-toplevel"),
        ],
        &[],
        DEFAULT_GIT_TIMEOUT,
        "git rev-parse --show-toplevel",
    )
    .await?;
    let path = path_from_git_output(&output.stdout)?;
    fs::canonicalize(&path).map_err(|error| {
        format!(
            "Could not resolve Git root returned for {}: {}: {error}",
            project_root.display(),
            path.display()
        )
    })
}

fn prepare_worktrees_root(configured: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(configured).map_err(|error| {
        format!(
            "Could not create managed-worktree root {}: {error}",
            configured.display()
        )
    })?;
    let canonical = fs::canonicalize(configured).map_err(|error| {
        format!(
            "Could not resolve managed-worktree root {}: {error}",
            configured.display()
        )
    })?;
    Ok(strip_verbatim_prefix(canonical))
}

/// On Windows, `fs::canonicalize` returns an extended-length `\\?\C:\…` path.
/// `git worktree add` cannot create leading directories for such a path and
/// fails with "Invalid argument", so strip the verbatim disk prefix back to a
/// plain `C:\…` before any of these paths reach git. Only the drive-letter
/// form is unwrapped; UNC and other prefixes, and every non-Windows path, are
/// returned unchanged.
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut components = path.components();
        if let Some(Component::Prefix(prefix)) = components.next()
            && let Prefix::VerbatimDisk(drive) = prefix.kind()
        {
            // The remaining components still include the root directory, which
            // renders with a leading separator, so append it to a bare `C:` to
            // land on `C:\…` rather than `C:\\…`.
            let mut rebuilt = std::ffi::OsString::from(format!("{}:", drive as char));
            rebuilt.push(components.as_path().as_os_str());
            return PathBuf::from(rebuilt);
        }
    }
    path
}

fn allocate_worktree_path(
    worktrees_root: &Path,
    source_git_root: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    let repository_name = source_git_root
        .file_name()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| OsStr::new("repository"));

    for _ in 0..256 {
        let shard_name = random_shard()?;
        let shard_root = worktrees_root.join(shard_name);
        match fs::create_dir(&shard_root) {
            Ok(()) => {
                let worktree_git_root = shard_root.join(repository_name);
                return Ok((shard_root, worktree_git_root));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "Could not reserve a managed-worktree directory under {}: {error}",
                    worktrees_root.display()
                ));
            }
        }
    }

    Err(format!(
        "Could not allocate a unique managed-worktree directory under {}",
        worktrees_root.display()
    ))
}

fn random_shard() -> Result<String, String> {
    let mut bytes = [0_u8; 2];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| format!("Could not generate a managed-worktree identifier: {error}"))?;
    Ok(format!("{:02x}{:02x}", bytes[0], bytes[1]))
}

async fn resolve_starting_commit(
    config: &AppConfig,
    source_git_root: &Path,
    warnings: &mut Vec<String>,
) -> Result<String, String> {
    let branch = optional_git_text(
        config,
        source_git_root,
        &[
            OsString::from("symbolic-ref"),
            OsString::from("--quiet"),
            OsString::from("--short"),
            OsString::from("HEAD"),
        ],
    )
    .await?;

    if config.worktrees.upstream_refresh_mode == WorktreeUpstreamRefreshMode::BestEffort
        && let Some(branch) = branch.as_deref()
    {
        refresh_upstream(config, source_git_root, branch, warnings).await;
    }

    let head = required_git_text(
        config,
        source_git_root,
        &[
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("HEAD^{commit}"),
        ],
        "resolve the current commit",
    )
    .await?;

    let Some(_) = branch else {
        return Ok(head);
    };
    let Some(upstream_ref) = optional_git_text(
        config,
        source_git_root,
        &[
            OsString::from("rev-parse"),
            OsString::from("--symbolic-full-name"),
            OsString::from("@{upstream}"),
        ],
    )
    .await?
    else {
        return Ok(head);
    };
    let Some(upstream) = optional_git_text(
        config,
        source_git_root,
        &[
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from(format!("{upstream_ref}^{{commit}}")),
        ],
    )
    .await?
    else {
        return Ok(head);
    };
    if upstream == head {
        return Ok(head);
    }

    let output = git_output(
        config,
        source_git_root,
        &[
            OsString::from("merge-base"),
            OsString::from("--is-ancestor"),
            OsString::from(&head),
            OsString::from(&upstream),
        ],
        &[],
        DEFAULT_GIT_TIMEOUT,
    )
    .await?;
    match output.status.code() {
        Some(0) => Ok(upstream),
        Some(1) => Ok(head),
        _ => {
            warnings.push(format!(
                "Could not compare the current commit with its upstream; using local HEAD: {}",
                bounded_output(&output)
            ));
            Ok(head)
        }
    }
}

async fn refresh_upstream(
    config: &AppConfig,
    source_git_root: &Path,
    branch: &str,
    warnings: &mut Vec<String>,
) {
    let remote_key = format!("branch.{branch}.remote");
    let merge_key = format!("branch.{branch}.merge");
    let Ok(Some(remote)) = git_config_value(config, source_git_root, &remote_key).await else {
        return;
    };
    let Ok(Some(merge_ref)) = git_config_value(config, source_git_root, &merge_key).await else {
        return;
    };
    if remote == "." {
        return;
    }
    let Ok(Some(remote_url)) =
        git_config_value(config, source_git_root, &format!("remote.{remote}.url")).await
    else {
        return;
    };
    let Ok(Some(upstream_ref)) = optional_git_text(
        config,
        source_git_root,
        &[
            OsString::from("for-each-ref"),
            OsString::from("--format=%(upstream)"),
            OsString::from("--count=1"),
            OsString::from(format!("refs/heads/{branch}")),
        ],
    )
    .await
    else {
        return;
    };
    if upstream_ref.is_empty() {
        return;
    }

    let null_config = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let env = [
        (OsString::from("GIT_CONFIG"), OsString::from(null_config)),
        (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
        (
            OsString::from("GIT_PROTOCOL_FROM_USER"),
            OsString::from("0"),
        ),
    ];
    let output = git_output(
        config,
        source_git_root,
        &[
            OsString::from("-c"),
            OsString::from("protocol.file.allow=always"),
            OsString::from("fetch"),
            OsString::from("--progress"),
            OsString::from("--no-tags"),
            OsString::from("--no-recurse-submodules"),
            OsString::from("--"),
            OsString::from(&remote_url),
            OsString::from(format!("+{merge_ref}:{upstream_ref}")),
        ],
        &env,
        UPSTREAM_REFRESH_TIMEOUT,
    )
    .await;

    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => warnings.push(format!(
            "Could not refresh the upstream before creating the worktree; cached Git state was used: {}",
            bounded_output(&output).replace(&remote_url, "<remote-url>")
        )),
        Err(error) => warnings.push(format!(
            "Could not refresh the upstream before creating the worktree; cached Git state was used: {error}"
        )),
    }
}

async fn safe_attribute_filter_overrides(
    config: &AppConfig,
    source_git_root: &Path,
) -> Result<Vec<OsString>, String> {
    let output = git_output(
        config,
        source_git_root,
        &[
            OsString::from("config"),
            OsString::from("--name-only"),
            OsString::from("--get-regexp"),
            OsString::from(r"^filter\..*\.(clean|smudge|process|required)$"),
        ],
        &[],
        DEFAULT_GIT_TIMEOUT,
    )
    .await?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(format!(
            "Could not inspect Git attribute filters before creating a worktree: {}",
            bounded_output(&output)
        ));
    }

    let mut names = BTreeSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("filter.") else {
            continue;
        };
        let Some((name, field)) = rest.rsplit_once('.') else {
            continue;
        };
        if matches!(field, "clean" | "smudge" | "process" | "required")
            && !name.is_empty()
            && !name.chars().any(char::is_control)
        {
            names.insert(name.to_string());
        }
    }

    let mut args = vec![
        OsString::from("-c"),
        OsString::from("attr.tree="),
        OsString::from("-c"),
        OsString::from("core.attributesFile="),
    ];
    for name in names {
        for assignment in [
            format!("filter.{name}.clean="),
            format!("filter.{name}.smudge="),
            format!("filter.{name}.process="),
            format!("filter.{name}.required=false"),
        ] {
            args.push(OsString::from("-c"));
            args.push(OsString::from(assignment));
        }
    }
    Ok(args)
}

async fn copy_ignored_agent_overrides(
    config: &AppConfig,
    source_git_root: &Path,
    worktree_git_root: &Path,
    worktrees_root: &Path,
) -> Result<(), String> {
    let paths = git_null_paths(
        config,
        source_git_root,
        &[
            OsString::from("ls-files"),
            OsString::from("--others"),
            OsString::from("--ignored"),
            OsString::from("--exclude-standard"),
            OsString::from("-z"),
            OsString::from("--"),
            OsString::from(":(glob)**/AGENTS.override.md"),
        ],
        "locate ignored AGENTS.override.md files",
    )
    .await?;
    copy_relative_files(
        source_git_root,
        worktree_git_root,
        worktrees_root,
        paths,
        "ignored AGENTS.override.md",
    )
}

async fn copy_worktree_includes(
    config: &AppConfig,
    source_git_root: &Path,
    worktree_git_root: &Path,
    worktrees_root: &Path,
) -> Result<(), String> {
    let include_file = source_git_root.join(".worktreeinclude");
    match fs::metadata(&include_file) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(format!(
                "{} exists but is not a regular file",
                include_file.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "Could not inspect {}: {error}",
                include_file.display()
            ));
        }
    }

    let ignored = git_null_paths(
        config,
        source_git_root,
        &[
            OsString::from("ls-files"),
            OsString::from("--others"),
            OsString::from("--ignored"),
            OsString::from("--exclude-standard"),
            OsString::from("-z"),
        ],
        "locate ignored files",
    )
    .await?;
    let included = git_null_paths(
        config,
        source_git_root,
        &[
            OsString::from("ls-files"),
            OsString::from("--others"),
            OsString::from("--ignored"),
            OsString::from("--exclude-from=.worktreeinclude"),
            OsString::from("-z"),
        ],
        "evaluate .worktreeinclude",
    )
    .await?;
    let ignored: HashSet<PathBuf> = ignored.into_iter().collect();
    let selected = included
        .into_iter()
        .filter(|path| ignored.contains(path))
        .collect::<BTreeSet<_>>();
    copy_relative_files(
        source_git_root,
        worktree_git_root,
        worktrees_root,
        selected,
        ".worktreeinclude",
    )
}

fn copy_relative_files(
    source_git_root: &Path,
    worktree_git_root: &Path,
    worktrees_root: &Path,
    paths: impl IntoIterator<Item = PathBuf>,
    label: &str,
) -> Result<(), String> {
    let excluded_managed_root = worktrees_root
        .strip_prefix(source_git_root)
        .ok()
        .map(Path::to_path_buf);

    for relative in paths {
        validate_relative_git_path(&relative, label)?;
        if excluded_managed_root
            .as_ref()
            .is_some_and(|excluded| relative == *excluded || relative.starts_with(excluded))
        {
            continue;
        }
        let source = source_git_root.join(&relative);
        let destination = worktree_git_root.join(&relative);
        copy_regular_file(&source, &destination, worktree_git_root, label)?;
    }
    Ok(())
}

fn validate_relative_git_path(path: &Path, label: &str) -> Result<(), String> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(format!(
            "{label} selected an invalid path: {}",
            path.display()
        ));
    }
    for component in path.components() {
        match component {
            Component::Normal(name) if name != OsStr::new(".git") => {}
            _ => {
                return Err(format!(
                    "{label} selected a path outside the Git worktree: {}",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

fn copy_regular_file(
    source: &Path,
    destination: &Path,
    worktree_git_root: &Path,
    label: &str,
) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "Could not inspect {label} source {}: {error}",
                source.display()
            ));
        }
    };
    if !metadata.file_type().is_file() {
        return Ok(false);
    }

    let parent = destination.parent().ok_or_else(|| {
        format!(
            "Could not determine destination directory for {}",
            destination.display()
        )
    })?;
    create_safe_parent_directories(worktree_git_root, parent, label)?;

    let mut input = fs::File::open(source)
        .map_err(|error| format!("Could not read {}: {error}", source.display()))?;
    let mut output = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => {
            return Err(format!(
                "Could not create {} from {label}: {error}",
                destination.display()
            ));
        }
    };
    std::io::copy(&mut input, &mut output).map_err(|error| {
        format!(
            "Could not copy {} to {}: {error}",
            source.display(),
            destination.display()
        )
    })?;
    fs::set_permissions(destination, metadata.permissions()).map_err(|error| {
        format!(
            "Could not preserve permissions on {}: {error}",
            destination.display()
        )
    })?;
    Ok(true)
}

fn create_safe_parent_directories(
    worktree_git_root: &Path,
    parent: &Path,
    label: &str,
) -> Result<(), String> {
    let relative = parent.strip_prefix(worktree_git_root).map_err(|_| {
        format!(
            "Refusing to copy {label} outside managed worktree {}",
            worktree_git_root.display()
        )
    })?;
    let mut current = worktree_git_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(format!(
                "Refusing to copy {label} through an invalid workspace path"
            ));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "Refusing to copy {label} through symlinked workspace path {}",
                    current.display()
                ));
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "Refusing to copy {label} through non-directory path {}",
                    current.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|error| {
                    format!(
                        "Could not create workspace directory {} for {label}: {error}",
                        current.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "Could not inspect workspace path {} for {label}: {error}",
                    current.display()
                ));
            }
        }
    }
    Ok(())
}

async fn prepare_local_environment(
    config: &AppConfig,
    source_git_root: &Path,
    worktree_git_root: &Path,
    shard_root: &Path,
) -> Result<Option<PathBuf>, String> {
    let Some(configured) = git_config_value(config, source_git_root, LOCAL_ENVIRONMENT_CONFIG_KEY)
        .await?
        .filter(|value| value != NO_LOCAL_ENVIRONMENT)
    else {
        set_worktree_config(
            config,
            source_git_root,
            worktree_git_root,
            LOCAL_ENVIRONMENT_CONFIG_KEY,
            NO_LOCAL_ENVIRONMENT,
        )
        .await?;
        return Ok(None);
    };

    let source = {
        let path = PathBuf::from(&configured);
        if path.is_absolute() {
            path
        } else {
            source_git_root.join(path)
        }
    };
    let metadata = fs::symlink_metadata(&source).map_err(|error| {
        format!(
            "Could not inspect selected Codex environment {}: {error}",
            source.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "Selected Codex environment {} is not a regular file",
            source.display()
        ));
    }

    let target = if let Ok(relative) = source.strip_prefix(source_git_root) {
        validate_relative_git_path(relative, "selected Codex environment")?;
        let target = worktree_git_root.join(relative);
        if !target.exists() {
            copy_regular_file(
                &source,
                &target,
                worktree_git_root,
                "selected Codex environment",
            )?;
        }
        target
    } else {
        let target = shard_root.join("environment.toml");
        let mut input = fs::File::open(&source)
            .map_err(|error| format!("Could not read {}: {error}", source.display()))?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|error| format!("Could not copy {}: {error}", source.display()))?;
        std::io::copy(&mut input, &mut output).map_err(|error| {
            format!(
                "Could not copy selected Codex environment {}: {error}",
                source.display()
            )
        })?;
        target
    };

    set_worktree_config(
        config,
        source_git_root,
        worktree_git_root,
        LOCAL_ENVIRONMENT_CONFIG_KEY,
        &target.to_string_lossy(),
    )
    .await?;
    Ok(Some(target))
}

async fn set_worktree_config(
    config: &AppConfig,
    source_git_root: &Path,
    worktree_git_root: &Path,
    key: &str,
    value: &str,
) -> Result<(), String> {
    let args = [
        OsString::from("config"),
        OsString::from("--worktree"),
        OsString::from(key),
        OsString::from(value),
    ];
    let first = git_output(config, worktree_git_root, &args, &[], DEFAULT_GIT_TIMEOUT).await?;
    if first.status.success() {
        return Ok(());
    }
    if !bounded_output(&first)
        .to_ascii_lowercase()
        .contains("worktreeconfig")
    {
        return Err(format!(
            "Could not store worktree-scoped Codex setting {key}: {}",
            bounded_output(&first)
        ));
    }

    git_success(
        config,
        source_git_root,
        &[
            OsString::from("config"),
            OsString::from("extensions.worktreeConfig"),
            OsString::from("true"),
        ],
        &[],
        DEFAULT_GIT_TIMEOUT,
        "enable Git worktree-specific configuration",
    )
    .await?;
    git_success(
        config,
        worktree_git_root,
        &args,
        &[],
        DEFAULT_GIT_TIMEOUT,
        "store worktree-scoped Codex configuration",
    )
    .await?;
    Ok(())
}

async fn run_setup_script(
    config: &AppConfig,
    environment_path: &Path,
    source_project_root: &Path,
    project_root: &Path,
    worktree_git_root: &Path,
) -> Result<(), String> {
    let raw = fs::read_to_string(environment_path).map_err(|error| {
        format!(
            "Could not read Codex environment {}: {error}",
            environment_path.display()
        )
    })?;
    let environment: LocalEnvironmentConfig = toml::from_str(&raw).map_err(|error| {
        format!(
            "Codex environment {} is invalid: {error}",
            environment_path.display()
        )
    })?;
    if environment.version == 0 {
        return Err(format!(
            "Codex environment {} has invalid version 0",
            environment_path.display()
        ));
    }
    let script = platform_script(&environment.setup);
    if script.trim().is_empty() {
        return Ok(());
    }

    let mut shell = resolve_shell(config.exec.default_shell.as_deref());
    let shell_bin = shell.remove(0);
    let command_text = wrap_for_shell(script, &shell_bin);
    let mut command = Command::new(&shell_bin);
    command
        .args(shell)
        .arg(command_text)
        .current_dir(worktree_git_root)
        .env("CODEX_SOURCE_TREE_PATH", source_project_root)
        .env("CODEX_WORKTREE_PATH", project_root)
        .kill_on_drop(true);
    scrub_untrusted_child_env(&mut command, config);
    let duration = Duration::from_millis(config.command.max_timeout.max(1));
    let output = timeout(duration, command.output())
        .await
        .map_err(|_| {
            format!(
                "Codex environment setup '{}' timed out after {} ms",
                environment.name, config.command.max_timeout
            )
        })?
        .map_err(|error| {
            format!(
                "Could not run Codex environment setup '{}': {error}",
                environment.name
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "Codex environment setup '{}' failed: {}",
            environment.name,
            bounded_output(&output)
        ));
    }
    Ok(())
}

fn platform_script(script: &EnvironmentScript) -> &str {
    if cfg!(target_os = "macos") {
        script
            .darwin
            .as_ref()
            .map(|platform| platform.script.as_str())
            .filter(|script| !script.is_empty())
            .unwrap_or(&script.script)
    } else if cfg!(windows) {
        script
            .win32
            .as_ref()
            .map(|platform| platform.script.as_str())
            .filter(|script| !script.is_empty())
            .unwrap_or(&script.script)
    } else {
        script
            .linux
            .as_ref()
            .map(|platform| platform.script.as_str())
            .filter(|script| !script.is_empty())
            .unwrap_or(&script.script)
    }
}

fn write_metadata(shard_root: &Path, metadata: &ManagedWorktreeMetadata) -> Result<(), String> {
    let target = shard_root.join(METADATA_FILENAME);
    let temporary = shard_root.join(format!(
        "{METADATA_FILENAME}.tmp.{}.{}",
        std::process::id(),
        now_ms()
    ));
    let json = serde_json::to_vec_pretty(metadata)
        .map_err(|error| format!("Could not serialize managed-worktree metadata: {error}"))?;
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&json)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, &target)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "Could not persist managed-worktree metadata at {}: {error}",
            target.display()
        ));
    }
    Ok(())
}

async fn cleanup_failed_creation(
    config: &AppConfig,
    source_git_root: &Path,
    worktree_git_root: &Path,
    shard_root: &Path,
) {
    if worktree_git_root.exists() {
        let _ = git_output(
            config,
            source_git_root,
            &[
                OsString::from("worktree"),
                OsString::from("remove"),
                OsString::from("--force"),
                worktree_git_root.as_os_str().to_owned(),
            ],
            &[],
            DEFAULT_GIT_TIMEOUT,
        )
        .await;
    }
    let _ = fs::remove_dir_all(shard_root);
}

async fn git_config_value(
    config: &AppConfig,
    cwd: &Path,
    key: &str,
) -> Result<Option<String>, String> {
    optional_git_text(
        config,
        cwd,
        &[
            OsString::from("config"),
            OsString::from("--get"),
            OsString::from(key),
        ],
    )
    .await
}

async fn required_git_text(
    config: &AppConfig,
    cwd: &Path,
    args: &[OsString],
    operation: &str,
) -> Result<String, String> {
    let output = git_success(config, cwd, args, &[], DEFAULT_GIT_TIMEOUT, operation).await?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn optional_git_text(
    config: &AppConfig,
    cwd: &Path,
    args: &[OsString],
) -> Result<Option<String>, String> {
    let output = git_output(config, cwd, args, &[], DEFAULT_GIT_TIMEOUT).await?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok((!text.is_empty()).then_some(text));
    }
    if output.status.code() == Some(1) || output.status.code() == Some(128) {
        return Ok(None);
    }
    Err(format!(
        "Git command failed while inspecting the project: {}",
        bounded_output(&output)
    ))
}

async fn git_null_paths(
    config: &AppConfig,
    cwd: &Path,
    args: &[OsString],
    operation: &str,
) -> Result<BTreeSet<PathBuf>, String> {
    let output = git_success(config, cwd, args, &[], DEFAULT_GIT_TIMEOUT, operation).await?;
    split_null_paths(&output.stdout)
}

async fn git_success(
    config: &AppConfig,
    cwd: &Path,
    args: &[OsString],
    env: &[(OsString, OsString)],
    duration: Duration,
    operation: &str,
) -> Result<Output, String> {
    let output = git_output(config, cwd, args, env, duration).await?;
    if output.status.success() {
        return Ok(output);
    }
    Err(format!("{operation} failed: {}", bounded_output(&output)))
}

async fn git_output(
    config: &AppConfig,
    cwd: &Path,
    args: &[OsString],
    env: &[(OsString, OsString)],
    duration: Duration,
) -> Result<Output, String> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .envs(env.iter().cloned())
        .kill_on_drop(true);
    scrub_untrusted_child_env(&mut command, config);
    timeout(duration, command.output())
        .await
        .map_err(|_| {
            format!(
                "Git command in {} timed out after {} seconds",
                cwd.display(),
                duration.as_secs()
            )
        })?
        .map_err(|error| format!("Could not run Git in {}: {error}", cwd.display()))
}

fn bounded_output(output: &Output) -> String {
    let mut bytes = Vec::with_capacity(output.stderr.len() + output.stdout.len() + 1);
    bytes.extend_from_slice(&output.stdout);
    if !output.stdout.is_empty() && !output.stderr.is_empty() {
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(&output.stderr);
    if bytes.len() > MAX_COMMAND_OUTPUT_BYTES {
        bytes.truncate(MAX_COMMAND_OUTPUT_BYTES);
        bytes.extend_from_slice(b"\n[output truncated]");
    }
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    if text.is_empty() {
        format!("exit status {}", output.status)
    } else {
        text
    }
}

fn path_from_git_output(bytes: &[u8]) -> Result<PathBuf, String> {
    let trimmed = trim_ascii_newlines(bytes);
    if trimmed.is_empty() {
        return Err("Git returned an empty filesystem path".to_string());
    }
    bytes_to_path(trimmed)
}

fn split_null_paths(bytes: &[u8]) -> Result<BTreeSet<PathBuf>, String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(bytes_to_path)
        .collect()
}

fn trim_ascii_newlines(mut bytes: &[u8]) -> &[u8] {
    while bytes
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
    {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> Result<PathBuf, String> {
    String::from_utf8(bytes.to_vec())
        .map(PathBuf::from)
        .map_err(|_| "Git returned a filesystem path that is not valid UTF-8".to_string())
}

fn is_native_shard_name(name: &str) -> bool {
    name.len() == 4 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_worktree_metadata_is_loaded_and_renamed() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join(METADATA_FILENAME);
        let legacy = root.path().join(LEGACY_METADATA_FILENAME);
        let metadata = ManagedWorktreeMetadata {
            version: METADATA_VERSION,
            source_project_root: "/source".to_string(),
            project_root: "/worktree/project".to_string(),
            source_git_root: "/source".to_string(),
            worktree_git_root: "/worktree".to_string(),
            worktrees_root: "/worktrees".to_string(),
            created_at_ms: 123,
        };
        std::fs::write(&legacy, serde_json::to_vec_pretty(&metadata).unwrap()).unwrap();

        assert_eq!(load_metadata(&current).unwrap().created_at_ms, 123);
        assert!(current.is_file());
        assert!(!legacy.exists());
    }
}
