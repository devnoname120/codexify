use super::*;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingWorktree {
    pub name: String,
    pub path: PathBuf,
    pub git_root: PathBuf,
    pub branch: Option<String>,
    pub last_used_at_ms: Option<u64>,
    pub managed_worktree: bool,
    pub source_checkout: bool,
}

fn usage_path(config: &AppConfig, path: &Path) -> PathBuf {
    let mut effective = config.clone();
    effective.work_dir = path.to_path_buf();
    crate::memory::memory_dir(&effective).join("workspace-last-used")
}

pub fn last_used_at_ms(config: &AppConfig, path: &Path) -> Option<u64> {
    let text = fs::read_to_string(usage_path(config, path)).ok()?;
    if text.len() > 24 {
        return None;
    }
    text.trim().parse().ok()
}

pub fn touch_workspace(config: &AppConfig, path: &Path) -> Result<(), String> {
    let now = now_ms();
    if last_used_at_ms(config, path).is_some_and(|last| last <= now && now - last < 60_000) {
        return Ok(());
    }
    let target = usage_path(config, path);
    let parent = target
        .parent()
        .ok_or("Missing workspace metadata directory")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    writeln!(temp, "{now}").map_err(|error| error.to_string())?;
    temp.persist(target).map_err(|error| error.to_string())?;
    Ok(())
}

fn git_dir(root: &Path) -> Result<PathBuf, String> {
    let dotgit = root.join(".git");
    if dotgit.is_dir() {
        return fs::canonicalize(dotgit).map_err(|error| error.to_string());
    }
    let text = fs::read_to_string(&dotgit)
        .map_err(|error| format!("Cannot read {}: {error}", dotgit.display()))?;
    let raw = text
        .trim_end_matches(['\r', '\n'])
        .strip_prefix("gitdir: ")
        .ok_or("Invalid .git worktree pointer")?;
    fs::canonicalize(root.join(raw)).map_err(|error| error.to_string())
}

fn common_dir(git: &Path) -> Result<PathBuf, String> {
    match fs::read_to_string(git.join("commondir")) {
        Ok(raw) => fs::canonicalize(git.join(raw.trim_end_matches(['\r', '\n'])))
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(git.to_path_buf()),
        Err(error) => Err(error.to_string()),
    }
}

pub fn validate_registered_worktree(
    source_project: &Path,
    target_project: &Path,
    target_git_root: &Path,
) -> Result<(), String> {
    let source_git_root = source_project
        .ancestors()
        .find(|path| path.join(".git").exists())
        .ok_or("Source project has no Git root")?;
    let source_common = common_dir(&git_dir(source_git_root)?)?;
    let target_dir = git_dir(target_git_root)?;
    if common_dir(&target_dir)? != source_common {
        return Err("Worktree belongs to another Git repository".into());
    }
    if target_dir != source_common {
        if target_dir.parent() != Some(source_common.join("worktrees").as_path()) {
            return Err("Worktree is not registered in this Git repository".into());
        }
        let back =
            fs::read_to_string(target_dir.join("gitdir")).map_err(|error| error.to_string())?;
        let back = fs::canonicalize(target_dir.join(back.trim_end_matches(['\r', '\n'])))
            .map_err(|error| error.to_string())?;
        if back
            != fs::canonicalize(target_git_root.join(".git")).map_err(|error| error.to_string())?
        {
            return Err("Worktree registration points to another directory".into());
        }
    }
    let relative = source_project
        .strip_prefix(source_git_root)
        .map_err(|error| error.to_string())?;
    let expected =
        fs::canonicalize(target_git_root.join(relative)).map_err(|error| error.to_string())?;
    if expected != target_project || !expected.starts_with(target_git_root) {
        return Err("Worktree project path does not match its source project".into());
    }
    Ok(())
}

pub async fn list_existing_worktrees(
    config: &AppConfig,
    source_project: &Path,
) -> Result<Vec<ExistingWorktree>, String> {
    let source = fs::canonicalize(source_project).map_err(|error| error.to_string())?;
    let root = resolve_git_root(config, &source).await?;
    let relative = source
        .strip_prefix(&root)
        .map_err(|error| error.to_string())?;
    let output = git_success(
        config,
        &root,
        &[
            "worktree".into(),
            "list".into(),
            "--porcelain".into(),
            "-z".into(),
        ],
        &[],
        DEFAULT_GIT_TIMEOUT,
        "list project worktrees",
    )
    .await?;
    let mut rows = Vec::new();
    let mut fields = Vec::new();
    for field in output.stdout.split(|byte| *byte == 0) {
        if !field.is_empty() {
            fields.push(field);
            continue;
        }
        if fields.is_empty() {
            continue;
        }
        let record = std::mem::take(&mut fields);
        if record
            .iter()
            .any(|field| field.starts_with(b"prunable") || *field == b"bare")
        {
            continue;
        }
        let Some(raw_path) = record
            .iter()
            .find_map(|field| field.strip_prefix(b"worktree "))
        else {
            continue;
        };
        let Ok(git_root) = fs::canonicalize(bytes_to_path(raw_path)?) else {
            continue;
        };
        let Ok(path) = fs::canonicalize(git_root.join(relative)) else {
            continue;
        };
        if validate_registered_worktree(&source, &path, &git_root).is_err() {
            continue;
        }
        let branch = record
            .iter()
            .find_map(|field| field.strip_prefix(b"branch refs/heads/"))
            .map(|name| String::from_utf8_lossy(name).into_owned());
        let managed = metadata_path_for_worktree(&git_root)
            .and_then(|path| load_metadata(&path).ok())
            .is_some_and(|meta| {
                Path::new(&meta.source_project_root) == source
                    && Path::new(&meta.project_root) == path
                    && Path::new(&meta.worktree_git_root) == git_root
            });
        let label = if managed {
            git_root.parent().and_then(Path::file_name)
        } else {
            git_root.file_name()
        }
        .unwrap_or_default()
        .to_string_lossy();
        let name = branch
            .clone()
            .unwrap_or_else(|| format!("{label} (detached)"));
        rows.push(ExistingWorktree {
            name,
            last_used_at_ms: last_used_at_ms(config, &path),
            source_checkout: path == source,
            path,
            git_root,
            branch,
            managed_worktree: managed,
        });
    }
    rows.sort_by(|a, b| {
        b.last_used_at_ms
            .cmp(&a.last_used_at_ms)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(rows)
}

pub async fn existing_worktree_choice(
    config: &AppConfig,
    source_project: &Path,
    path: &Path,
) -> Result<ExistingWorktree, String> {
    if !path.is_absolute() {
        return Err("Worktree path must be absolute".into());
    }
    let path = fs::canonicalize(path)
        .map_err(|error| format!("Worktree is no longer available: {error}"))?;
    list_existing_worktrees(config, source_project).await?.into_iter().find(|row| row.path == path).ok_or_else(|| "This path is not an available worktree of the selected project. Refresh the worktree list.".into())
}
