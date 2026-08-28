use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use tokio::process::Command;
use tokio::time::timeout;

use crate::process_env::scrub_untrusted_child_env;
use crate::project_catalog::discover_project_catalog;
use crate::types::AppConfig;

const GIT_INSPECTION_TIMEOUT: Duration = Duration::from_secs(15);
const GIT_FETCH_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const GIT_CLONE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CLONE_LOCK_STALE_MS: u128 = 10 * 60 * 1_000;
const CLONE_LOCK_TIMEOUT_MS: u128 = 5 * 60 * 1_000;
const CLONE_LOCK_RETRY_MS: u64 = 100;
const MAX_GIT_OUTPUT_BYTES: usize = 16 * 1024;

static CLONE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectReference {
    Path(String),
    GitHub(GitHubRepository),
}

impl ProjectReference {
    pub fn parse(input: &str) -> Result<Self, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("path must be a non-empty string".to_string());
        }

        if looks_like_remote(input) {
            return parse_github_repository(input).map(Self::GitHub);
        }

        Ok(Self::Path(input.to_string()))
    }

    pub fn display(&self) -> &str {
        match self {
            Self::Path(path) => path,
            Self::GitHub(repository) => repository.web_url(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloneTransport {
    Https,
    Ssh,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRepository {
    owner: String,
    name: String,
    identity: String,
    checkout: GitHubCheckout,
    web_url: String,
    clone_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GitHubCheckout {
    DefaultBranch,
    Branch(String),
    PullRequest(u64),
    Commit(String),
}

impl GitHubRepository {
    pub fn web_url(&self) -> &str {
        &self.web_url
    }

    pub fn clone_url(&self) -> &str {
        &self.clone_url
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn has_explicit_checkout(&self) -> bool {
        !matches!(self.checkout, GitHubCheckout::DefaultBranch)
    }

    fn same_identity(&self, other: &Self) -> bool {
        self.identity == other.identity
    }

    fn same_selection(&self, other: &Self) -> bool {
        self.same_identity(other) && self.checkout == other.checkout
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGitHubProject {
    pub source_project_root: PathBuf,
    pub repository_url: String,
    pub checkout_commit: Option<String>,
    pub source_matches_checkout: bool,
    pub cloned: bool,
    pub warnings: Vec<String>,
}

pub fn repository_url_matches(url: &str, repository: &GitHubRepository) -> bool {
    parse_github_repository(url)
        .map(|candidate| candidate.same_selection(repository))
        .unwrap_or(false)
}

fn remote_url_matches(url: &str, repository: &GitHubRepository) -> bool {
    parse_github_repository(url)
        .map(|candidate| candidate.same_identity(repository))
        .unwrap_or(false)
}

pub fn normalize_repository_url(url: &str) -> Result<String, String> {
    parse_github_repository(url).map(|repository| repository.web_url)
}

pub async fn project_matches_repository(
    config: &AppConfig,
    project_root: &Path,
    repository: &GitHubRepository,
) -> Result<bool, String> {
    let Some(git_root) = git_top_level(config, project_root).await? else {
        return Ok(false);
    };
    remote_matches(config, &git_root, repository).await
}

pub async fn resolve_github_project(
    config: &AppConfig,
    access_root: &Path,
    repository: &GitHubRepository,
) -> Result<ResolvedGitHubProject, String> {
    let clone_dir = canonical_clone_dir(config, access_root)?;
    let target = clone_dir.join(repository.name());
    let mut warnings = Vec::new();

    let (matches, discovery_warnings) =
        find_existing_repositories(config, access_root, &clone_dir, &target, repository).await?;
    warnings.extend(discovery_warnings);
    let existing = unique_existing_match(matches, repository)?;
    if !repository.has_explicit_checkout()
        && let Some(project_root) = existing.as_ref()
    {
        return Ok(resolved(
            materialize_existing_repository(config, project_root.clone(), repository).await?,
            repository,
            warnings,
        ));
    }

    if existing.is_none() && fs::symlink_metadata(&target).is_ok() {
        return Err(destination_collision(&target, repository));
    }

    let _lock = acquire_clone_lock(&clone_dir, repository).await?;

    let (matches, discovery_warnings) =
        find_existing_repositories(config, access_root, &clone_dir, &target, repository).await?;
    warnings.extend(discovery_warnings);
    warnings.sort();
    warnings.dedup();
    if let Some(project_root) = unique_existing_match(matches, repository)? {
        return Ok(resolved(
            materialize_existing_repository(config, project_root, repository).await?,
            repository,
            warnings,
        ));
    }
    if fs::symlink_metadata(&target).is_ok() {
        return Err(destination_collision(&target, repository));
    }

    let materialized =
        clone_repository(config, access_root, &clone_dir, &target, repository).await?;
    Ok(resolved(materialized, repository, warnings))
}

fn resolved(
    materialized: MaterializedRepository,
    repository: &GitHubRepository,
    warnings: Vec<String>,
) -> ResolvedGitHubProject {
    ResolvedGitHubProject {
        source_project_root: materialized.source_project_root,
        repository_url: repository.web_url().to_string(),
        checkout_commit: materialized.checkout_commit,
        source_matches_checkout: materialized.source_matches_checkout,
        cloned: materialized.cloned,
        warnings,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MaterializedRepository {
    source_project_root: PathBuf,
    checkout_commit: Option<String>,
    source_matches_checkout: bool,
    cloned: bool,
}

fn looks_like_remote(input: &str) -> bool {
    input.contains("://")
        || input
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("git@"))
}

fn parse_github_repository(input: &str) -> Result<GitHubRepository, String> {
    if input.chars().any(char::is_control) {
        return Err("GitHub repository URL contains control characters".to_string());
    }
    if input.contains(['?', '#']) {
        return Err("GitHub repository URL must not contain a query or fragment".to_string());
    }

    let lower = input.to_ascii_lowercase();
    if lower.starts_with("git@github.com:") {
        let path = &input["git@github.com:".len()..];
        return repository_from_path(path, CloneTransport::Ssh, false);
    }

    let Some((scheme, remainder)) = input.split_once("://") else {
        return Err(
            "Only HTTPS or SSH GitHub repository URLs are supported (for example https://github.com/owner/repo or git@github.com:owner/repo.git)"
                .to_string(),
        );
    };
    let Some((authority, path)) = remainder.split_once('/') else {
        return Err("GitHub repository URL is missing an owner and repository name".to_string());
    };

    match scheme.to_ascii_lowercase().as_str() {
        "https" => {
            if authority.contains('@') {
                return Err(
                    "Credential-bearing GitHub URLs are rejected; use a credential helper or SSH URL"
                        .to_string(),
                );
            }
            if !matches_github_https_authority(authority) {
                return Err("Only github.com repository URLs are supported".to_string());
            }
            repository_from_path(path, CloneTransport::Https, true)
        }
        "ssh" => {
            if !matches_github_ssh_authority(authority) {
                return Err(
                    "GitHub SSH URLs must use git@github.com (or git@ssh.github.com:443)"
                        .to_string(),
                );
            }
            let mut repository = repository_from_path(path, CloneTransport::Ssh, false)?;
            if authority.eq_ignore_ascii_case("git@ssh.github.com:443") {
                repository.clone_url = format!(
                    "ssh://git@ssh.github.com:443/{}/{}.git",
                    repository.owner, repository.name
                );
            }
            Ok(repository)
        }
        _ => Err(
            "Only HTTPS or SSH GitHub repository URLs are supported; insecure and arbitrary Git transports are rejected"
                .to_string(),
        ),
    }
}

fn matches_github_https_authority(authority: &str) -> bool {
    authority.eq_ignore_ascii_case("github.com")
        || authority.eq_ignore_ascii_case("www.github.com")
        || authority.eq_ignore_ascii_case("github.com:443")
        || authority.eq_ignore_ascii_case("www.github.com:443")
}

fn matches_github_ssh_authority(authority: &str) -> bool {
    authority.eq_ignore_ascii_case("git@github.com")
        || authority.eq_ignore_ascii_case("git@github.com:22")
        || authority.eq_ignore_ascii_case("git@ssh.github.com:443")
}

fn repository_from_path(
    path: &str,
    transport: CloneTransport,
    allow_checkout: bool,
) -> Result<GitHubRepository, String> {
    let path = path.trim_matches('/');
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.len() < 2 || parts.iter().any(|part| part.is_empty()) {
        return Err("GitHub repository URL must identify an owner and repository".to_string());
    }

    let owner = parts[0];
    let mut name = parts[1];
    if name
        .get(name.len().saturating_sub(4)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".git"))
    {
        name = &name[..name.len() - 4];
    }
    validate_component(owner, "owner")?;
    validate_component(name, "repository")?;

    let checkout = match &parts[2..] {
        [] => GitHubCheckout::DefaultBranch,
        ["tree", branch @ ..] if allow_checkout && !branch.is_empty() => {
            let branch = percent_decode(&branch.join("/"))?;
            validate_branch_name(&branch)?;
            GitHubCheckout::Branch(branch)
        }
        ["pull", number] if allow_checkout => {
            let number = number
                .parse::<u64>()
                .ok()
                .filter(|number| *number > 0)
                .ok_or_else(|| "GitHub pull-request URL has an invalid PR number".to_string())?;
            GitHubCheckout::PullRequest(number)
        }
        ["commit", commit] if allow_checkout => {
            GitHubCheckout::Commit(normalize_commit_id(commit)?)
        }
        _ => {
            return Err(
                "Supported GitHub URLs identify a repository root, a branch with /tree/<branch>, a pull request with /pull/<number>, or a commit with /commit/<sha>"
                    .to_string(),
            );
        }
    };

    let identity = format!(
        "{}/{}",
        owner.to_ascii_lowercase(),
        name.to_ascii_lowercase()
    );
    let repository_web_url = format!("https://github.com/{owner}/{name}");
    let web_url = match &checkout {
        GitHubCheckout::DefaultBranch => repository_web_url,
        GitHubCheckout::Branch(branch) => {
            format!(
                "{repository_web_url}/tree/{}",
                percent_encode_branch(branch)
            )
        }
        GitHubCheckout::PullRequest(number) => {
            format!("{repository_web_url}/pull/{number}")
        }
        GitHubCheckout::Commit(commit) => {
            format!("{repository_web_url}/commit/{commit}")
        }
    };
    let clone_url = match transport {
        CloneTransport::Https => format!("https://github.com/{owner}/{name}.git"),
        CloneTransport::Ssh => format!("git@github.com:{owner}/{name}.git"),
    };

    Ok(GitHubRepository {
        owner: owner.to_string(),
        name: name.to_string(),
        identity,
        checkout,
        web_url,
        clone_url,
    })
}

fn percent_decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() {
            return Err("GitHub branch URL contains an incomplete percent escape".to_string());
        }
        let high = hex_value(bytes[index + 1])
            .ok_or_else(|| "GitHub branch URL contains an invalid percent escape".to_string())?;
        let low = hex_value(bytes[index + 2])
            .ok_or_else(|| "GitHub branch URL contains an invalid percent escape".to_string())?;
        decoded.push((high << 4) | low);
        index += 3;
    }
    String::from_utf8(decoded)
        .map_err(|_| "GitHub branch URL is not valid UTF-8 after decoding".to_string())
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn percent_encode_branch(branch: &str) -> String {
    let mut encoded = String::with_capacity(branch.len());
    for byte in branch.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn validate_branch_name(branch: &str) -> Result<(), String> {
    let invalid_character = branch.chars().any(|character| {
        character.is_control()
            || character == ' '
            || matches!(character, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    });
    let invalid_component = branch
        .split('/')
        .any(|component| component.starts_with('.') || component.ends_with(".lock"));
    let valid = !branch.is_empty()
        && branch.len() <= 1_024
        && branch != "@"
        && !branch.starts_with(['-', '/'])
        && !branch.ends_with(['/', '.'])
        && !branch.contains("//")
        && !branch.contains("..")
        && !branch.contains("@{")
        && !invalid_character
        && !invalid_component;
    if valid {
        Ok(())
    } else {
        Err("GitHub branch URL contains an invalid Git branch name".to_string())
    }
}

fn normalize_commit_id(commit: &str) -> Result<String, String> {
    if commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(commit.to_ascii_lowercase())
    } else {
        Err("GitHub commit URL must contain a full 40-character hexadecimal commit ID".to_string())
    }
}

fn validate_component(value: &str, label: &str) -> Result<(), String> {
    let valid = !value.is_empty()
        && value.len() <= 100
        && !matches!(value, "." | "..")
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        });
    if valid {
        Ok(())
    } else {
        Err(format!("GitHub {label} name is invalid"))
    }
}

fn canonical_clone_dir(config: &AppConfig, access_root: &Path) -> Result<PathBuf, String> {
    let clone_dir = fs::canonicalize(&config.project_clone_dir).map_err(|error| {
        format!(
            "Could not resolve the configured project clone directory {}: {error}",
            config.project_clone_dir.display()
        )
    })?;
    if !clone_dir.is_dir() {
        return Err(format!(
            "Configured project clone directory is not a directory: {}",
            clone_dir.display()
        ));
    }
    if clone_dir != access_root && !clone_dir.starts_with(access_root) {
        return Err(format!(
            "Configured project clone directory resolves outside the project access root: {}",
            clone_dir.display()
        ));
    }
    Ok(clone_dir)
}

async fn find_existing_repositories(
    config: &AppConfig,
    access_root: &Path,
    clone_dir: &Path,
    target: &Path,
    repository: &GitHubRepository,
) -> Result<(BTreeSet<PathBuf>, Vec<String>), String> {
    let mut candidates = BTreeSet::new();
    let mut warnings = Vec::new();

    candidates.insert(access_root.to_path_buf());
    candidates.insert(clone_dir.to_path_buf());
    if target.is_dir() {
        candidates.insert(target.to_path_buf());
    }

    match discover_project_catalog(config) {
        Ok(catalog) => {
            candidates.extend(
                catalog
                    .projects
                    .into_iter()
                    .map(|project| project.canonical_path),
            );
        }
        Err(error) => warnings.push(format!(
            "Project catalogue lookup failed while searching for an existing checkout: {error}"
        )),
    }

    let entries = fs::read_dir(clone_dir).map_err(|error| {
        format!(
            "Could not inspect the project clone directory {}: {error}",
            clone_dir.display()
        )
    })?;
    for entry in entries.flatten() {
        let path = entry.path();
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".codexify-clone-")
        {
            continue;
        }
        if path.is_dir() {
            candidates.insert(path);
        }
    }

    let mut matches = BTreeSet::new();
    for candidate in candidates {
        if let Some(project_root) =
            matching_repository_at(config, access_root, &candidate, repository).await?
        {
            matches.insert(project_root);
        }
    }

    Ok((matches, warnings))
}

async fn matching_repository_at(
    config: &AppConfig,
    access_root: &Path,
    candidate: &Path,
    repository: &GitHubRepository,
) -> Result<Option<PathBuf>, String> {
    let Ok(candidate) = fs::canonicalize(candidate) else {
        return Ok(None);
    };
    if !candidate.is_dir() || (candidate != access_root && !candidate.starts_with(access_root)) {
        return Ok(None);
    }
    let Some(git_root) = git_top_level(config, &candidate).await? else {
        return Ok(None);
    };
    let git_root = fs::canonicalize(&git_root).map_err(|error| {
        format!(
            "Could not resolve Git root {} while matching {}: {error}",
            git_root.display(),
            repository.web_url()
        )
    })?;
    if git_root != access_root && !git_root.starts_with(access_root) {
        return Ok(None);
    }
    if remote_matches(config, &git_root, repository).await? {
        Ok(Some(git_root))
    } else {
        Ok(None)
    }
}

fn unique_existing_match(
    matches: BTreeSet<PathBuf>,
    repository: &GitHubRepository,
) -> Result<Option<PathBuf>, String> {
    if matches.len() <= 1 {
        return Ok(matches.into_iter().next());
    }

    let paths = matches
        .iter()
        .map(|path| format!("`{}`", path.display()))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "Multiple local checkouts match {}: {paths}. Pass the intended project path instead because a conversation cannot switch projects after binding.",
        repository.web_url()
    ))
}

fn destination_collision(target: &Path, repository: &GitHubRepository) -> String {
    format!(
        "Cannot clone {} because the destination `{}` already exists and is not a checkout of that repository. Move it, configure another projectCloneDir/--project-clone-dir, or pass an existing matching project path.",
        repository.web_url(),
        target.display()
    )
}

async fn materialize_existing_repository(
    config: &AppConfig,
    source_project_root: PathBuf,
    repository: &GitHubRepository,
) -> Result<MaterializedRepository, String> {
    let checkout_commit = if repository.has_explicit_checkout() {
        let fetch_source = matching_remote_source(config, &source_project_root, repository)
            .await?
            .ok_or_else(|| {
                format!(
                    "Could not find a matching Git remote in {} for {}",
                    source_project_root.display(),
                    repository.web_url()
                )
            })?;
        Some(fetch_checkout_commit(config, &source_project_root, repository, &fetch_source).await?)
    } else {
        None
    };
    let source_matches_checkout = match checkout_commit.as_deref() {
        Some(commit) => head_commit(config, &source_project_root).await? == commit,
        None => true,
    };

    Ok(MaterializedRepository {
        source_project_root,
        checkout_commit,
        source_matches_checkout,
        cloned: false,
    })
}

async fn clone_repository(
    config: &AppConfig,
    access_root: &Path,
    clone_dir: &Path,
    target: &Path,
    repository: &GitHubRepository,
) -> Result<MaterializedRepository, String> {
    clone_repository_from(
        config,
        access_root,
        clone_dir,
        target,
        repository,
        repository.clone_url(),
    )
    .await
}

async fn clone_repository_from(
    config: &AppConfig,
    access_root: &Path,
    clone_dir: &Path,
    target: &Path,
    repository: &GitHubRepository,
    clone_source: &str,
) -> Result<MaterializedRepository, String> {
    let staging = temporary_clone_path(clone_dir, repository)?;
    let temporary = staging.join("repository");

    let mut command = Command::new("git");
    command.args(["clone", "--no-recurse-submodules"]);
    if let GitHubCheckout::Branch(branch) = &repository.checkout {
        command.args(["--branch", branch]);
    }
    command
        .arg("--")
        .arg(clone_source)
        .arg(&temporary)
        .current_dir(clone_dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .kill_on_drop(true);
    scrub_untrusted_child_env(&mut command, config);

    let output = match timeout(GIT_CLONE_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(format!(
                "Could not run git clone for {}: {error}",
                repository.web_url()
            ));
        }
        Err(_) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(format!(
                "git clone for {} timed out after {} seconds",
                repository.web_url(),
                GIT_CLONE_TIMEOUT.as_secs()
            ));
        }
    };
    if !output.status.success() {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!(
            "git clone for {} failed: {}",
            repository.web_url(),
            bounded_output(&output)
        ));
    }

    let checkout_commit =
        match prepare_cloned_checkout(config, &temporary, repository, clone_source).await {
            Ok(commit) => commit,
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };

    if clone_source != repository.clone_url() {
        let output = match git_output(
            config,
            &temporary,
            &[
                OsString::from("remote"),
                OsString::from("set-url"),
                OsString::from("origin"),
                OsString::from(repository.clone_url()),
            ],
            GIT_INSPECTION_TIMEOUT,
        )
        .await
        {
            Ok(output) => output,
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };
        if !output.status.success() {
            let _ = fs::remove_dir_all(&staging);
            return Err(format!(
                "Could not record the requested GitHub remote after cloning: {}",
                bounded_output(&output)
            ));
        }
    }

    let matches = match project_matches_repository(config, &temporary, repository).await {
        Ok(matches) => matches,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    if !matches {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!(
            "The completed clone did not report the requested GitHub repository as a remote: {}",
            repository.web_url()
        ));
    }

    if fs::symlink_metadata(target).is_ok() {
        let _ = fs::remove_dir_all(&staging);
        if let Some(project_root) =
            matching_repository_at(config, access_root, target, repository).await?
        {
            return materialize_existing_repository(config, project_root, repository).await;
        }
        return Err(destination_collision(target, repository));
    }

    if let Err(error) = publish_clone_no_replace(&temporary, target) {
        let _ = fs::remove_dir_all(&staging);
        if let Some(project_root) =
            matching_repository_at(config, access_root, target, repository).await?
        {
            return materialize_existing_repository(config, project_root, repository).await;
        }
        if fs::symlink_metadata(target).is_ok() {
            return Err(destination_collision(target, repository));
        }
        return Err(format!(
            "Could not publish the cloned repository at {}: {error}",
            target.display()
        ));
    }
    let _ = fs::remove_dir(&staging);

    let project_root = fs::canonicalize(target).map_err(|error| {
        format!(
            "Could not resolve cloned repository {}: {error}",
            target.display()
        )
    })?;
    if project_root != access_root && !project_root.starts_with(access_root) {
        return Err(format!(
            "Cloned repository resolves outside the project access root: {}",
            project_root.display()
        ));
    }
    if !project_matches_repository(config, &project_root, repository).await? {
        return Err(format!(
            "Published clone at {} no longer matches {}",
            project_root.display(),
            repository.web_url()
        ));
    }
    Ok(MaterializedRepository {
        source_project_root: project_root,
        checkout_commit,
        source_matches_checkout: true,
        cloned: true,
    })
}

#[cfg(target_os = "linux")]
fn publish_clone_no_replace(source: &Path, target: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let target = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target path contains NUL"))?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn publish_clone_no_replace(source: &Path, target: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let target = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target path contains NUL"))?;
    let result = unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn publish_clone_no_replace(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0) };
    if result != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn publish_clone_no_replace(_source: &Path, _target: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace directory publication is unsupported on this platform",
    ))
}

async fn prepare_cloned_checkout(
    config: &AppConfig,
    git_root: &Path,
    repository: &GitHubRepository,
    clone_source: &str,
) -> Result<Option<String>, String> {
    match &repository.checkout {
        GitHubCheckout::DefaultBranch => Ok(None),
        GitHubCheckout::Branch(branch) => {
            let head = head_commit(config, git_root).await?;
            let branch_commit = required_commit(
                config,
                git_root,
                &format!("refs/heads/{branch}"),
                "resolve the cloned branch",
            )
            .await?;
            if head != branch_commit {
                return Err(format!(
                    "git clone completed for {}, but HEAD does not match the requested branch `{branch}`",
                    repository.web_url()
                ));
            }
            Ok(Some(head))
        }
        GitHubCheckout::PullRequest(_) | GitHubCheckout::Commit(_) => {
            let commit = fetch_checkout_commit(config, git_root, repository, clone_source).await?;
            let output = git_output(
                config,
                git_root,
                &[
                    OsString::from("checkout"),
                    OsString::from("--detach"),
                    OsString::from("--force"),
                    OsString::from(&commit),
                ],
                GIT_INSPECTION_TIMEOUT,
            )
            .await?;
            if !output.status.success() {
                return Err(format!(
                    "Could not check out the requested GitHub target for {}: {}",
                    repository.web_url(),
                    bounded_output(&output)
                ));
            }
            Ok(Some(commit))
        }
    }
}

async fn fetch_checkout_commit(
    config: &AppConfig,
    git_root: &Path,
    repository: &GitHubRepository,
    fetch_source: &str,
) -> Result<String, String> {
    let source_spec = match &repository.checkout {
        GitHubCheckout::DefaultBranch => {
            return Err("Internal error: default-branch selection has no explicit ref".to_string());
        }
        GitHubCheckout::Branch(branch) => format!("refs/heads/{branch}"),
        GitHubCheckout::PullRequest(number) => format!("refs/pull/{number}/head"),
        GitHubCheckout::Commit(commit) => commit.clone(),
    };
    let destination_ref = checkout_storage_ref(repository);
    let refspec = format!("+{source_spec}:{destination_ref}");
    let output = git_output(
        config,
        git_root,
        &[
            OsString::from("fetch"),
            OsString::from("--force"),
            OsString::from("--no-tags"),
            OsString::from("--no-recurse-submodules"),
            OsString::from("--"),
            OsString::from(fetch_source),
            OsString::from(refspec),
        ],
        GIT_FETCH_TIMEOUT,
    )
    .await?;
    if !output.status.success() {
        return Err(format!(
            "Could not fetch the requested checkout for {}: {}",
            repository.web_url(),
            bounded_output(&output).replace(fetch_source, "<repository-remote>")
        ));
    }

    required_commit(
        config,
        git_root,
        &destination_ref,
        "resolve the fetched checkout",
    )
    .await
}

fn checkout_storage_ref(repository: &GitHubRepository) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"codexify/github-checkout/v1\0");
    hasher.update(repository.identity.as_bytes());
    hasher.update(b"\0");
    hasher.update(repository.web_url().as_bytes());
    format!(
        "refs/codexify/project-selection/{}",
        encode_hex(&hasher.finalize(), 40)
    )
}

async fn head_commit(config: &AppConfig, git_root: &Path) -> Result<String, String> {
    required_commit(config, git_root, "HEAD", "resolve the current checkout").await
}

async fn required_commit(
    config: &AppConfig,
    git_root: &Path,
    reference: &str,
    action: &str,
) -> Result<String, String> {
    let output = git_output(
        config,
        git_root,
        &[
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from(format!("{reference}^{{commit}}")),
        ],
        GIT_INSPECTION_TIMEOUT,
    )
    .await?;
    if !output.status.success() {
        return Err(format!(
            "Could not {action} in {}: {}",
            git_root.display(),
            bounded_output(&output)
        ));
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if commit.is_empty() {
        return Err(format!(
            "Could not {action} in {}: Git returned an empty commit ID",
            git_root.display()
        ));
    }
    Ok(commit)
}

async fn git_top_level(config: &AppConfig, path: &Path) -> Result<Option<PathBuf>, String> {
    let output = git_output(
        config,
        path,
        &[
            OsString::from("rev-parse"),
            OsString::from("--show-toplevel"),
        ],
        GIT_INSPECTION_TIMEOUT,
    )
    .await?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(text)))
}

async fn remote_matches(
    config: &AppConfig,
    git_root: &Path,
    repository: &GitHubRepository,
) -> Result<bool, String> {
    Ok(!matching_remote_sources(config, git_root, repository)
        .await?
        .is_empty())
}

async fn matching_remote_source(
    config: &AppConfig,
    git_root: &Path,
    repository: &GitHubRepository,
) -> Result<Option<String>, String> {
    Ok(matching_remote_sources(config, git_root, repository)
        .await?
        .into_iter()
        .next()
        .map(|(_, url)| url))
}

async fn matching_remote_sources(
    config: &AppConfig,
    git_root: &Path,
    repository: &GitHubRepository,
) -> Result<Vec<(String, String)>, String> {
    let output = git_output(
        config,
        git_root,
        &[
            OsString::from("config"),
            OsString::from("--local"),
            OsString::from("--get-regexp"),
            OsString::from(r"^remote\..*\.url$"),
        ],
        GIT_INSPECTION_TIMEOUT,
    )
    .await?;
    if !output.status.success() {
        if matches!(output.status.code(), Some(1 | 128)) {
            return Ok(Vec::new());
        }
        return Err(format!(
            "Could not inspect Git remotes in {}: {}",
            git_root.display(),
            bounded_output(&output)
        ));
    }

    let mut matches = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(2, char::is_whitespace);
            let key = fields.next()?;
            let url = fields.next()?.trim();
            let name = key.strip_prefix("remote.")?.strip_suffix(".url")?;
            remote_url_matches(url, repository).then(|| (name.to_string(), url.to_string()))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        let left_origin = left.0 != "origin";
        let right_origin = right.0 != "origin";
        left_origin.cmp(&right_origin).then_with(|| left.cmp(right))
    });
    Ok(matches)
}

async fn git_output(
    config: &AppConfig,
    cwd: &Path,
    args: &[OsString],
    duration: Duration,
) -> Result<Output, String> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_OPTIONAL_LOCKS", "0")
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
    let mut bytes = Vec::with_capacity(output.stdout.len() + output.stderr.len() + 1);
    bytes.extend_from_slice(&output.stdout);
    if !output.stdout.is_empty() && !output.stderr.is_empty() {
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(&output.stderr);
    if bytes.len() > MAX_GIT_OUTPUT_BYTES {
        bytes.truncate(MAX_GIT_OUTPUT_BYTES);
        bytes.extend_from_slice(b"\n[output truncated]");
    }
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    if text.is_empty() {
        format!("exit status {}", output.status)
    } else {
        text
    }
}

fn temporary_clone_path(
    clone_dir: &Path,
    repository: &GitHubRepository,
) -> Result<PathBuf, String> {
    loop {
        let counter = CLONE_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = clone_dir.join(format!(
            ".codexify-clone-{}.tmp.{}.{}",
            repository_key(repository),
            std::process::id(),
            counter
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Err(error) =
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                    {
                        let _ = fs::remove_dir(&path);
                        return Err(format!(
                            "Could not restrict temporary clone directory {}: {error}",
                            path.display()
                        ));
                    }
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "Could not create a temporary clone directory in {}: {error}",
                    clone_dir.display()
                ));
            }
        }
    }
}

fn repository_key(repository: &GitHubRepository) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"codexify/github-repository/v1\0");
    hasher.update(repository.identity.as_bytes());
    encode_hex(&hasher.finalize(), 24)
}

struct CloneLock {
    path: PathBuf,
    token: String,
}

impl Drop for CloneLock {
    fn drop(&mut self) {
        if fs::read_to_string(&self.path).is_ok_and(|current| current == self.token) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

async fn acquire_clone_lock(
    clone_dir: &Path,
    repository: &GitHubRepository,
) -> Result<CloneLock, String> {
    let lock_path = clone_dir.join(format!(
        ".codexify-clone-{}.lock",
        repository_key(repository)
    ));
    let deadline = now_ms() + CLONE_LOCK_TIMEOUT_MS;

    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                let token = format!(
                    "{}:{}:{}",
                    std::process::id(),
                    now_ms(),
                    CLONE_COUNTER.fetch_add(1, Ordering::SeqCst)
                );
                if let Err(error) = write!(file, "{token}") {
                    let _ = fs::remove_file(&lock_path);
                    return Err(format!(
                        "Could not initialize project clone lock {}: {error}",
                        lock_path.display()
                    ));
                }
                return Ok(CloneLock {
                    path: lock_path,
                    token,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                match fs::symlink_metadata(&lock_path) {
                    Ok(metadata) if metadata.file_type().is_file() => {
                        let modified = metadata.modified().map_err(|error| {
                            format!(
                                "Could not inspect project clone lock {}: {error}",
                                lock_path.display()
                            )
                        })?;
                        let age = SystemTime::now()
                            .duration_since(modified)
                            .map(|duration| duration.as_millis())
                            .unwrap_or(0);
                        if age > CLONE_LOCK_STALE_MS {
                            let _ = fs::remove_file(&lock_path);
                            continue;
                        }
                    }
                    Ok(_) => {
                        return Err(format!(
                            "Project clone lock path is not a regular file: {}",
                            lock_path.display()
                        ));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(format!(
                            "Could not inspect project clone lock {}: {error}",
                            lock_path.display()
                        ));
                    }
                }
                if now_ms() >= deadline {
                    return Err(format!(
                        "Timed out waiting for another clone of {} to finish",
                        repository.web_url()
                    ));
                }
                tokio::time::sleep(Duration::from_millis(CLONE_LOCK_RETRY_MS)).await;
            }
            Err(error) => {
                return Err(format!(
                    "Could not lock project clone destination {}: {error}",
                    lock_path.display()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_config;
    use tempfile::TempDir;

    #[test]
    fn parses_and_normalizes_common_github_urls() {
        let https = ProjectReference::parse("https://github.com/OpenAI/codex.git/").unwrap();
        let ProjectReference::GitHub(https) = https else {
            panic!("expected GitHub reference");
        };
        assert_eq!(https.web_url(), "https://github.com/OpenAI/codex");
        assert_eq!(https.clone_url(), "https://github.com/OpenAI/codex.git");

        let ssh = ProjectReference::parse("git@github.com:openai/CODEX.git").unwrap();
        let ProjectReference::GitHub(ssh) = ssh else {
            panic!("expected GitHub reference");
        };
        assert!(https.same_identity(&ssh));
        assert_eq!(ssh.clone_url(), "git@github.com:openai/CODEX.git");

        let alternate =
            ProjectReference::parse("ssh://git@ssh.github.com:443/OpenAI/codex.git").unwrap();
        let ProjectReference::GitHub(alternate) = alternate else {
            panic!("expected GitHub reference");
        };
        assert!(https.same_identity(&alternate));
        assert_eq!(
            alternate.clone_url(),
            "ssh://git@ssh.github.com:443/OpenAI/codex.git"
        );

        let branch = ProjectReference::parse(
            "https://github.com/OpenAI/codex/tree/feature%2Fproject-selection",
        )
        .unwrap();
        let ProjectReference::GitHub(branch) = branch else {
            panic!("expected GitHub reference");
        };
        assert_eq!(
            branch.web_url(),
            "https://github.com/OpenAI/codex/tree/feature/project-selection"
        );
        assert!(branch.has_explicit_checkout());
        assert!(https.same_identity(&branch));
        assert!(!https.same_selection(&branch));

        let pull = ProjectReference::parse("https://github.com/OpenAI/codex/pull/886/").unwrap();
        let ProjectReference::GitHub(pull) = pull else {
            panic!("expected GitHub reference");
        };
        assert_eq!(pull.web_url(), "https://github.com/OpenAI/codex/pull/886");
        assert!(pull.has_explicit_checkout());

        let commit = ProjectReference::parse(
            "https://github.com/OpenAI/codex/commit/C8CAE44BF004A6AC6BFC267C5DFE503D57652103/",
        )
        .unwrap();
        let ProjectReference::GitHub(commit) = commit else {
            panic!("expected GitHub reference");
        };
        assert_eq!(
            commit.web_url(),
            "https://github.com/OpenAI/codex/commit/c8cae44bf004a6ac6bfc267c5dfe503d57652103"
        );
        assert!(commit.has_explicit_checkout());
        assert!(https.same_identity(&commit));
        assert!(!https.same_selection(&commit));
    }

    #[test]
    fn rejects_non_github_credentials_and_unsupported_subpages() {
        for input in [
            "https://gitlab.com/openai/codex",
            "https://token@github.com/openai/codex",
            "http://github.com/openai/codex",
            "git://github.com/openai/codex",
            "https://github.com/openai/codex/tree/..",
            "https://github.com/openai/codex/pull/0",
            "https://github.com/openai/codex/pull/886/files",
            "https://github.com/openai/codex/commit/deadbeef",
            "https://github.com/openai/codex/commit/000000000000000000000000000000000000000g",
            "https://github.com/openai/codex/commit/c8cae44bf004a6ac6bfc267c5dfe503d57652103/checks",
            "https://github.com/openai/codex/issues/1",
            "https://github.com/openai/codex?tab=readme",
        ] {
            assert!(ProjectReference::parse(input).is_err(), "{input}");
        }
    }

    #[test]
    fn ordinary_paths_remain_paths() {
        assert_eq!(
            ProjectReference::parse("nested/project").unwrap(),
            ProjectReference::Path("nested/project".to_string())
        );
    }

    #[tokio::test]
    async fn clones_into_the_configured_directory_and_verifies_the_requested_remote() {
        let root = TempDir::new().unwrap();
        let access_root = root.path().join("projects");
        let clone_dir = access_root.join("cloned");
        let source = root.path().join("source");
        fs::create_dir_all(&clone_dir).unwrap();
        fs::create_dir_all(&source).unwrap();

        git(&source, &["init", "--quiet"]);
        fs::write(source.join("README.md"), "fixture").unwrap();
        git(&source, &["add", "README.md"]);
        git(
            &source,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );

        let repository = parse_github_repository("https://github.com/acme/widget").unwrap();
        let mut config = default_config(access_root.clone());
        config.multi_project = true;
        config.project_clone_dir = clone_dir.clone();
        config.project_catalog.codex_config.enabled = false;

        let target = clone_dir.join("widget");
        let cloned = clone_repository_from(
            &config,
            &fs::canonicalize(&access_root).unwrap(),
            &fs::canonicalize(&clone_dir).unwrap(),
            &target,
            &repository,
            source.to_str().unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(
            cloned.source_project_root,
            fs::canonicalize(&target).unwrap()
        );
        assert!(cloned.cloned);
        assert_eq!(cloned.checkout_commit, None);
        assert!(cloned.source_matches_checkout);
        assert_eq!(
            fs::read_to_string(target.join("README.md")).unwrap(),
            "fixture"
        );
        assert!(
            project_matches_repository(&config, &target, &repository)
                .await
                .unwrap()
        );
        assert_eq!(
            fs::read_dir(&clone_dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            [std::ffi::OsString::from("widget")]
        );
    }

    #[tokio::test]
    async fn clones_and_checks_out_the_requested_branch() {
        let root = TempDir::new().unwrap();
        let access_root = root.path().join("projects");
        let clone_dir = access_root.join("cloned");
        let source = root.path().join("source");
        fs::create_dir_all(&clone_dir).unwrap();
        initialize_repository(&source);
        commit_file(&source, "base.txt", "base", "base");
        git(&source, &["checkout", "--quiet", "-b", "split_db"]);
        let branch_commit = commit_file(&source, "branch.txt", "split", "branch");

        let repository =
            parse_github_repository("https://github.com/acme/widget/tree/split_db").unwrap();
        let mut config = default_config(access_root.clone());
        config.multi_project = true;
        config.project_clone_dir = clone_dir.clone();
        config.project_catalog.codex_config.enabled = false;

        let target = clone_dir.join("widget");
        let cloned = clone_repository_from(
            &config,
            &fs::canonicalize(&access_root).unwrap(),
            &fs::canonicalize(&clone_dir).unwrap(),
            &target,
            &repository,
            source.to_str().unwrap(),
        )
        .await
        .unwrap();

        assert!(cloned.cloned);
        assert!(cloned.source_matches_checkout);
        assert_eq!(
            cloned.checkout_commit.as_deref(),
            Some(branch_commit.as_str())
        );
        assert_eq!(git_text(&target, &["branch", "--show-current"]), "split_db");
        assert_eq!(git_text(&target, &["rev-parse", "HEAD"]), branch_commit);
        assert_eq!(
            fs::read_to_string(target.join("branch.txt")).unwrap(),
            "split"
        );
    }

    #[tokio::test]
    async fn clones_fetches_and_checks_out_the_requested_pull_request() {
        let root = TempDir::new().unwrap();
        let access_root = root.path().join("projects");
        let clone_dir = access_root.join("cloned");
        let source = root.path().join("source");
        fs::create_dir_all(&clone_dir).unwrap();
        initialize_repository(&source);
        commit_file(&source, "base.txt", "base", "base");
        git(&source, &["checkout", "--quiet", "-b", "pull-head"]);
        let pull_commit = commit_file(&source, "pull.txt", "pull", "pull request");
        git(&source, &["update-ref", "refs/pull/886/head", &pull_commit]);
        git(&source, &["checkout", "--quiet", "main"]);

        let repository =
            parse_github_repository("https://github.com/acme/widget/pull/886").unwrap();
        let mut config = default_config(access_root.clone());
        config.multi_project = true;
        config.project_clone_dir = clone_dir.clone();
        config.project_catalog.codex_config.enabled = false;

        let target = clone_dir.join("widget");
        let cloned = clone_repository_from(
            &config,
            &fs::canonicalize(&access_root).unwrap(),
            &fs::canonicalize(&clone_dir).unwrap(),
            &target,
            &repository,
            source.to_str().unwrap(),
        )
        .await
        .unwrap();

        assert!(cloned.cloned);
        assert!(cloned.source_matches_checkout);
        assert_eq!(
            cloned.checkout_commit.as_deref(),
            Some(pull_commit.as_str())
        );
        assert_eq!(git_text(&target, &["branch", "--show-current"]), "");
        assert_eq!(git_text(&target, &["rev-parse", "HEAD"]), pull_commit);
        assert_eq!(fs::read_to_string(target.join("pull.txt")).unwrap(), "pull");
    }

    #[tokio::test]
    async fn clones_fetches_and_checks_out_the_requested_commit() {
        let root = TempDir::new().unwrap();
        let access_root = root.path().join("projects");
        let clone_dir = access_root.join("cloned");
        let source = root.path().join("source");
        fs::create_dir_all(&clone_dir).unwrap();
        initialize_repository(&source);
        commit_file(&source, "base.txt", "base", "base");
        git(&source, &["checkout", "--quiet", "-b", "feature"]);
        let requested_commit = commit_file(&source, "commit.txt", "commit", "requested commit");
        git(&source, &["checkout", "--quiet", "main"]);

        let repository = parse_github_repository(&format!(
            "https://github.com/acme/widget/commit/{requested_commit}"
        ))
        .unwrap();
        let mut config = default_config(access_root.clone());
        config.multi_project = true;
        config.project_clone_dir = clone_dir.clone();
        config.project_catalog.codex_config.enabled = false;

        let target = clone_dir.join("widget");
        let cloned = clone_repository_from(
            &config,
            &fs::canonicalize(&access_root).unwrap(),
            &fs::canonicalize(&clone_dir).unwrap(),
            &target,
            &repository,
            source.to_str().unwrap(),
        )
        .await
        .unwrap();

        assert!(cloned.cloned);
        assert!(cloned.source_matches_checkout);
        assert_eq!(
            cloned.checkout_commit.as_deref(),
            Some(requested_commit.as_str())
        );
        assert_eq!(git_text(&target, &["branch", "--show-current"]), "");
        assert_eq!(git_text(&target, &["rev-parse", "HEAD"]), requested_commit);
        assert_eq!(
            fs::read_to_string(target.join("commit.txt")).unwrap(),
            "commit"
        );
    }

    #[tokio::test]
    async fn fetching_a_target_for_an_existing_checkout_does_not_move_head() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("source");
        let checkout = root.path().join("checkout");
        initialize_repository(&source);
        let base_commit = commit_file(&source, "base.txt", "base", "base");
        git(&source, &["checkout", "--quiet", "-b", "split_db"]);
        let branch_commit = commit_file(&source, "branch.txt", "split", "branch");
        git(&source, &["checkout", "--quiet", "main"]);
        git(
            root.path(),
            &[
                "clone",
                "--quiet",
                source.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ],
        );

        let repository =
            parse_github_repository("https://github.com/acme/widget/tree/split_db").unwrap();
        let config = default_config(root.path().to_path_buf());
        let fetched =
            fetch_checkout_commit(&config, &checkout, &repository, source.to_str().unwrap())
                .await
                .unwrap();

        assert_eq!(fetched, branch_commit);
        assert_eq!(git_text(&checkout, &["rev-parse", "HEAD"]), base_commit);
        assert!(!checkout.join("branch.txt").exists());
    }

    #[tokio::test]
    async fn fetching_a_commit_for_an_existing_checkout_does_not_move_head() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("source");
        let checkout = root.path().join("checkout");
        initialize_repository(&source);
        let base_commit = commit_file(&source, "base.txt", "base", "base");
        git(&source, &["checkout", "--quiet", "-b", "feature"]);
        let requested_commit = commit_file(&source, "commit.txt", "commit", "requested commit");
        git(&source, &["checkout", "--quiet", "main"]);
        git(
            root.path(),
            &[
                "clone",
                "--quiet",
                source.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ],
        );

        let repository = parse_github_repository(&format!(
            "https://github.com/acme/widget/commit/{requested_commit}"
        ))
        .unwrap();
        let config = default_config(root.path().to_path_buf());
        let fetched =
            fetch_checkout_commit(&config, &checkout, &repository, source.to_str().unwrap())
                .await
                .unwrap();

        assert_eq!(fetched, requested_commit);
        assert_eq!(git_text(&checkout, &["rev-parse", "HEAD"]), base_commit);
        assert!(!checkout.join("commit.txt").exists());
    }

    #[tokio::test]
    async fn rejects_ambiguous_matching_checkouts() {
        let root = TempDir::new().unwrap();
        let access_root = root.path().join("projects");
        let first = access_root.join("widget");
        let second = access_root.join("widget-copy");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        for checkout in [&first, &second] {
            git(checkout, &["init", "--quiet"]);
            git(
                checkout,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/acme/widget.git",
                ],
            );
        }

        let mut config = default_config(access_root.clone());
        config.multi_project = true;
        config.project_catalog.codex_config.enabled = false;
        let repository = parse_github_repository("https://github.com/acme/widget").unwrap();

        let error = resolve_github_project(
            &config,
            &fs::canonicalize(&access_root).unwrap(),
            &repository,
        )
        .await
        .unwrap_err();
        assert!(error.contains("Multiple local checkouts match"), "{error}");
        assert!(error.contains(first.to_string_lossy().as_ref()), "{error}");
        assert!(error.contains(second.to_string_lossy().as_ref()), "{error}");
    }

    #[tokio::test]
    async fn refuses_an_unrelated_clone_destination_without_fetching() {
        let root = TempDir::new().unwrap();
        let access_root = root.path().join("projects");
        let target = access_root.join("widget");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep.txt"), "do not replace").unwrap();

        let mut config = default_config(access_root.clone());
        config.multi_project = true;
        config.project_catalog.codex_config.enabled = false;
        let repository = parse_github_repository("https://github.com/acme/widget").unwrap();

        let error = resolve_github_project(
            &config,
            &fs::canonicalize(&access_root).unwrap(),
            &repository,
        )
        .await
        .unwrap_err();
        assert!(error.contains("destination"), "{error}");
        assert_eq!(
            fs::read_to_string(target.join("keep.txt")).unwrap(),
            "do not replace"
        );
    }

    #[test]
    fn clone_publication_never_replaces_an_existing_directory() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("source.txt"), "source").unwrap();
        fs::write(target.join("target.txt"), "target").unwrap();

        let error = publish_clone_no_replace(&source, &target).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(source.join("source.txt")).unwrap(),
            "source"
        );
        assert_eq!(
            fs::read_to_string(target.join("target.txt")).unwrap(),
            "target"
        );
    }

    fn git(cwd: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git must be installed for tests");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_text(cwd: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git must be installed for tests");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn initialize_repository(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git(path, &["init", "--quiet"]);
        git(path, &["config", "user.name", "Test"]);
        git(path, &["config", "user.email", "test@example.com"]);
        git(path, &["checkout", "--quiet", "-b", "main"]);
    }

    fn commit_file(path: &Path, name: &str, contents: &str, message: &str) -> String {
        fs::write(path.join(name), contents).unwrap();
        git(path, &["add", name]);
        git(path, &["commit", "--quiet", "-m", message]);
        git_text(path, &["rev-parse", "HEAD"])
    }
}
