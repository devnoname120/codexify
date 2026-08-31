//! Verified, out-of-process Codexify self-updates.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::time::Duration;

use anyhow::{Context, bail};
use flate2::read::GzDecoder;
use reqwest::{Client, redirect::Policy};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::Archive;
use tokio::process::Command;
use zip::ZipArchive;

use crate::process_env::SERVICE_SUPERVISED_ENV;
use crate::service;
use crate::util::home_dir;

const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/devnoname120/codexify/releases/latest";
const RELEASE_ROOT: &str = "https://github.com/devnoname120/codexify/releases/download";
const MAX_RELEASE_METADATA_BYTES: usize = 1024 * 1024;
const MAX_ARCHIVE_BYTES: usize = 128 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CHANGELOG_FILE_BYTES: u64 = 1024 * 1024;
const MAX_SELECTED_CHANGELOG_BYTES: usize = 256 * 1024;
const MAX_STATUS_FILE_BYTES: u64 = 8 * 1024;
const MAX_STATUS_RECORDS: usize = 32;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);
const LATEST_VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const BINARY_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const UPDATE_DELAY_SECONDS: u64 = 10;
const UPDATE_LOCK_FILE: &str = "update.lock";
const UPDATE_STATUS_DIR: &str = "status";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfUpdateStatus {
    Scheduled,
    UpToDate,
    AheadOfLatest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    Scheduled,
    Installing,
    Validating,
    Restarting,
    Succeeded,
    Failed,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatusRecord {
    pub update_id: String,
    pub from_version: String,
    pub target_version: String,
    pub state: UpdatePhase,
    pub updated_at: String,
    pub failure_code: Option<String>,
    pub failure_detail: Option<String>,
}

impl UpdateStatusRecord {
    pub fn scheduled(update_id: &str, from_version: &str, target_version: &str) -> Self {
        Self {
            update_id: update_id.to_string(),
            from_version: from_version.to_string(),
            target_version: target_version.to_string(),
            state: UpdatePhase::Scheduled,
            updated_at: update_timestamp(),
            failure_code: None,
            failure_detail: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelfUpdateReceipt {
    pub status: SelfUpdateStatus,
    pub current_version: String,
    pub target_version: String,
    pub update_id: Option<String>,
    pub service_restart: bool,
    pub log_path: String,
    #[serde(skip_serializing)]
    pub changelog: Option<String>,
}

#[derive(Debug, Clone)]
struct ReleaseSource {
    latest_release_url: String,
    release_root: String,
}

impl Default for ReleaseSource {
    fn default() -> Self {
        Self {
            latest_release_url: LATEST_RELEASE_URL.to_string(),
            release_root: RELEASE_ROOT.to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    TarGz,
    Zip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseAsset {
    tag: String,
    version: Version,
    archive_name: String,
    binary_name: &'static str,
    kind: ArchiveKind,
}

#[derive(Debug)]
struct UpdateWorkspace {
    id: String,
    lock_path: PathBuf,
    staged_path: PathBuf,
    backup_path: PathBuf,
    worker_path: PathBuf,
    status_path: PathBuf,
    clean_on_drop: bool,
    preserve_status: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateLockInspection {
    pub path: PathBuf,
    pub update_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatestVersionStatus {
    UpdateAvailable,
    UpToDate,
    AheadOfLatest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestVersionInspection {
    pub status: LatestVersionStatus,
    pub current: Version,
    pub latest: Version,
}

impl UpdateWorkspace {
    fn create(root: &Path, target: &Path) -> anyhow::Result<Self> {
        let update_dir = root.join("update");
        make_private_dir(&update_dir)?;
        let status_dir = status_dir_for_root(root);
        ensure_private_status_dir(&status_dir)?;
        let target_dir = target
            .parent()
            .context("installed Codexify executable has no parent directory")?;
        make_private_dir(target_dir)?;

        let id = random_id()?;
        let lock_path = update_dir.join(UPDATE_LOCK_FILE);
        let mut lock = match private_new_file(&lock_path, false) {
            Ok(lock) => lock,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let owner = fs::read_to_string(&lock_path)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let detail = if owner.is_empty() {
                    String::new()
                } else {
                    format!(" (update {owner})")
                };
                bail!(
                    "a Codexify self-update is already in progress{detail}; inspect `codexify service logs -f` and remove {} only if the earlier update is no longer running",
                    lock_path.display()
                )
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create update lock {}", lock_path.display()));
            }
        };
        if let Err(error) = writeln!(lock, "{id}").and_then(|_| lock.sync_all()) {
            drop(lock);
            let _ = fs::remove_file(&lock_path);
            return Err(error)
                .with_context(|| format!("initialize update lock {}", lock_path.display()));
        }

        let executable_suffix = std::env::consts::EXE_SUFFIX;
        let staged_path = target_dir.join(format!(".codexify-update-{id}{executable_suffix}"));
        let backup_path = target_dir.join(format!(".codexify-backup-{id}{executable_suffix}"));
        let worker_extension = if cfg!(windows) { "ps1" } else { "sh" };
        let worker_path = update_dir.join(format!("worker-{id}.{worker_extension}"));
        let status_path = status_record_path(&status_dir, &id);

        Ok(Self {
            id,
            lock_path,
            staged_path,
            backup_path,
            worker_path,
            status_path,
            clean_on_drop: true,
            preserve_status: false,
        })
    }

    fn hand_off(&mut self) {
        self.clean_on_drop = false;
    }

    fn preserve_status(&mut self) {
        self.preserve_status = true;
    }
}

impl Drop for UpdateWorkspace {
    fn drop(&mut self) {
        if !self.clean_on_drop {
            return;
        }
        for path in [
            &self.staged_path,
            &self.backup_path,
            &self.worker_path,
            &self.lock_path,
        ] {
            let _ = fs::remove_file(path);
        }
        if !self.preserve_status {
            let _ = fs::remove_file(&self.status_path);
        }
    }
}

pub fn inspect_update_lock() -> anyhow::Result<Option<UpdateLockInspection>> {
    let home = home_dir().context("locate the user's home directory")?;
    inspect_update_lock_from(&home)
}

pub async fn inspect_latest_version() -> anyhow::Result<LatestVersionInspection> {
    inspect_latest_version_from(
        ReleaseSource::default(),
        env!("CARGO_PKG_VERSION"),
        LATEST_VERSION_CHECK_TIMEOUT,
    )
    .await
}

async fn inspect_latest_version_from(
    source: ReleaseSource,
    current_version_text: &str,
    request_timeout: Duration,
) -> anyhow::Result<LatestVersionInspection> {
    let current =
        Version::parse(current_version_text).context("parse the running Codexify version")?;
    let client = release_metadata_client(request_timeout)?;
    let latest = latest_release(&client, &source).await?.version;
    let status = if latest > current {
        LatestVersionStatus::UpdateAvailable
    } else if latest == current {
        LatestVersionStatus::UpToDate
    } else {
        LatestVersionStatus::AheadOfLatest
    };
    Ok(LatestVersionInspection {
        status,
        current,
        latest,
    })
}

fn inspect_update_lock_from(home: &Path) -> anyhow::Result<Option<UpdateLockInspection>> {
    const MAX_LOCK_BYTES: u64 = 4096;
    const MAX_UPDATE_ID_BYTES: usize = 128;

    let path = home.join(".codexify").join("update").join(UPDATE_LOCK_FILE);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("open update lock {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect update lock {}", path.display()))?;
    if !metadata.is_file() {
        bail!(
            "Codexify update lock is not a regular file: {}",
            path.display()
        );
    }

    let update_id = if metadata.len() > MAX_LOCK_BYTES {
        None
    } else {
        let mut raw = String::new();
        file.take(MAX_LOCK_BYTES + 1)
            .read_to_string(&mut raw)
            .with_context(|| format!("read update lock {}", path.display()))?;
        if raw.len() as u64 > MAX_LOCK_BYTES {
            return Ok(Some(UpdateLockInspection {
                path,
                update_id: None,
            }));
        }
        let candidate = raw.trim();
        (candidate.len() <= MAX_UPDATE_ID_BYTES
            && !candidate.is_empty()
            && candidate
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
        .then(|| candidate.to_string())
    };

    Ok(Some(UpdateLockInspection { path, update_id }))
}

fn update_timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub(crate) fn valid_update_id(value: &str) -> bool {
    value.len() == 24
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn status_record_path(status_dir: &Path, update_id: &str) -> PathBuf {
    status_dir.join(format!("{update_id}.json"))
}

fn status_dir_for_root(root: &Path) -> PathBuf {
    root.join("update").join(UPDATE_STATUS_DIR)
}

fn ensure_private_status_dir(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() {
                bail!("update status path is not a private directory");
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => make_private_dir(path)?,
        Err(error) => return Err(error).context("inspect update status directory"),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn validate_status_record(record: &UpdateStatusRecord) -> anyhow::Result<()> {
    if !valid_update_id(&record.update_id) {
        bail!("update status record has an invalid update id");
    }
    Version::parse(&record.from_version).context("parse update source version")?;
    Version::parse(&record.target_version).context("parse update target version")?;
    chrono::DateTime::parse_from_rfc3339(&record.updated_at)
        .context("parse update status timestamp")?;

    if record.failure_code.as_ref().is_some_and(|value| {
        value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    }) {
        bail!("update status record has an invalid failure code");
    }
    if record.failure_detail.as_ref().is_some_and(|value| {
        value.is_empty() || value.len() > 512 || value.chars().any(char::is_control)
    }) {
        bail!("update status record has an invalid failure detail");
    }

    let failed = matches!(record.state, UpdatePhase::Failed | UpdatePhase::RolledBack);
    if failed != (record.failure_code.is_some() && record.failure_detail.is_some()) {
        bail!("update status record has inconsistent failure fields");
    }
    Ok(())
}

fn replace_file_atomically(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };

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
        let result = unsafe {
            MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(not(windows))]
    {
        fs::rename(source, target)
    }
}

fn write_status_record_to(status_dir: &Path, record: &UpdateStatusRecord) -> anyhow::Result<()> {
    ensure_private_status_dir(status_dir)?;
    validate_status_record(record)?;
    let bytes = serde_json::to_vec(record).context("serialize update status record")?;
    if bytes.len() as u64 > MAX_STATUS_FILE_BYTES {
        bail!("update status record exceeds its size limit");
    }

    let temporary = status_dir.join(format!(".{}.tmp-{}", record.update_id, random_id()?));
    write_private_bytes(&temporary, &bytes, false)?;
    let destination = status_record_path(status_dir, &record.update_id);
    if let Err(error) = replace_file_atomically(&temporary, &destination) {
        let _ = fs::remove_file(&temporary);
        return Err(error).context("publish update status record");
    }
    #[cfg(unix)]
    File::open(status_dir)?.sync_all()?;
    Ok(())
}

fn read_status_record_from(
    status_dir: &Path,
    update_id: &str,
) -> anyhow::Result<UpdateStatusRecord> {
    if !valid_update_id(update_id) {
        bail!("invalid update id");
    }
    let directory = fs::symlink_metadata(status_dir).context("open update status directory")?;
    if !directory.file_type().is_dir() {
        bail!("update status directory is unavailable");
    }

    let path = status_record_path(status_dir, update_id);
    let metadata = fs::symlink_metadata(&path).context("locate update status record")?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_STATUS_FILE_BYTES {
        bail!("update status record is unavailable");
    }
    let bytes = fs::read(&path).context("read update status record")?;
    if bytes.len() as u64 > MAX_STATUS_FILE_BYTES {
        bail!("update status record exceeds its size limit");
    }
    let record: UpdateStatusRecord =
        serde_json::from_slice(&bytes).context("parse update status record")?;
    validate_status_record(&record)?;
    if record.update_id != update_id {
        bail!("update status record id does not match its filename");
    }
    Ok(record)
}

pub fn read_update_status(update_id: &str) -> anyhow::Result<UpdateStatusRecord> {
    let home = home_dir().context("locate the user's home directory")?;
    read_status_record_from(&status_dir_for_root(&home.join(".codexify")), update_id)
}

fn cleanup_status_records(status_dir: &Path, active_update_id: &str) -> anyhow::Result<()> {
    ensure_private_status_dir(status_dir)?;
    let mut records = Vec::new();
    for entry in fs::read_dir(status_dir).context("list update status records")? {
        let entry = entry.context("read update status directory entry")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(update_id) = name.strip_suffix(".json") else {
            continue;
        };
        if !valid_update_id(update_id) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.file_type().is_file() {
            continue;
        }
        records.push((
            metadata
                .modified()
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            update_id.to_string(),
            entry.path(),
        ));
    }
    records.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

    let mut remaining = records.len();
    for (_, update_id, path) in records {
        if remaining <= MAX_STATUS_RECORDS {
            break;
        }
        if update_id == active_update_id {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => remaining -= 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => remaining -= 1,
            Err(error) => return Err(error).context("remove old update status record"),
        }
    }
    Ok(())
}

fn select_changelog_sections(
    changelog: &str,
    current_version: &Version,
    target_version: &Version,
) -> Option<String> {
    if target_version <= current_version {
        return None;
    }

    let mut headings = Vec::new();
    let mut offset = 0;
    for line in changelog.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("## [") {
            let version = rest
                .split_once(']')
                .and_then(|(value, _)| Version::parse(value).ok());
            headings.push((offset, version));
        }
        offset += line.len();
    }
    if offset < changelog.len() {
        let line = &changelog[offset..];
        if let Some(rest) = line.strip_prefix("## [") {
            let version = rest
                .split_once(']')
                .and_then(|(value, _)| Version::parse(value).ok());
            headings.push((offset, version));
        }
    }

    let mut selected = String::new();
    for (index, (start, version)) in headings.iter().enumerate() {
        let Some(version) = version else {
            continue;
        };
        if version <= current_version || version > target_version {
            continue;
        }
        let end = headings
            .get(index + 1)
            .map_or(changelog.len(), |(offset, _)| *offset);
        let section = &changelog[*start..end];
        if selected.len().saturating_add(section.len()) > MAX_SELECTED_CHANGELOG_BYTES {
            return None;
        }
        selected.push_str(section);
    }

    let selected = selected.trim_end();
    (!selected.is_empty()).then(|| format!("{selected}\n"))
}

pub async fn trigger() -> anyhow::Result<SelfUpdateReceipt> {
    trigger_from(ReleaseSource::default(), env!("CARGO_PKG_VERSION")).await
}

async fn trigger_from(
    source: ReleaseSource,
    current_version_text: &str,
) -> anyhow::Result<SelfUpdateReceipt> {
    let current_version =
        Version::parse(current_version_text).context("parse the running Codexify version")?;
    let home = home_dir().context("locate the user's home directory")?;
    if !home.is_absolute() {
        bail!("the user's home directory must be absolute for self-update");
    }
    let root = home.join(".codexify");
    let target = installed_executable(&home);
    ensure_running_installed_binary(&target)?;

    let service_restart = service_supervised();
    if service_restart && !service::is_installed().context("query the Codexify service")? {
        bail!("the server is service-supervised, but its service definition is missing");
    }
    #[cfg(windows)]
    if !service_restart {
        bail!(
            "Windows self-update requires Codexify to be running under its background service so the executable can be unlocked and restarted; run the installer manually or install the service first"
        );
    }

    let log_path = service::log_path()?;
    let mut workspace = UpdateWorkspace::create(&root, &target)?;
    let client = update_client()?;
    let latest = latest_release(&client, &source).await?;

    if latest.version <= current_version {
        let status = if latest.version == current_version {
            SelfUpdateStatus::UpToDate
        } else {
            SelfUpdateStatus::AheadOfLatest
        };
        return Ok(SelfUpdateReceipt {
            status,
            current_version: current_version.to_string(),
            target_version: latest.version.to_string(),
            update_id: None,
            service_restart: false,
            log_path: log_path.to_string_lossy().into_owned(),
            changelog: None,
        });
    }

    let release_url = format!(
        "{}/{}/{}",
        source.release_root.trim_end_matches('/'),
        latest.tag,
        latest.archive_name
    );
    let checksums_url = format!(
        "{}/{}/checksums.txt",
        source.release_root.trim_end_matches('/'),
        latest.tag
    );
    let checksums = fetch_bytes(
        &client,
        &checksums_url,
        MAX_RELEASE_METADATA_BYTES,
        "release checksums",
    )
    .await?;
    let expected = checksum_for(&checksums, &latest.archive_name)?;
    let archive = fetch_bytes(&client, &release_url, MAX_ARCHIVE_BYTES, "release archive").await?;
    let actual = sha256_hex(&archive);
    if actual != expected {
        bail!(
            "release archive {} failed SHA-256 verification",
            latest.archive_name
        );
    }

    let extracted = extract_release(&archive, &latest)?;
    let changelog = extracted.changelog.as_deref().and_then(|contents| {
        select_changelog_sections(contents, &current_version, &latest.version)
    });
    write_staged_binary(&workspace.staged_path, &extracted.binary)?;
    validate_staged_binary(&workspace.staged_path).await?;
    make_log_available(&log_path)?;

    let worker = worker_script(
        &workspace,
        &target,
        &log_path,
        &current_version,
        &latest.version,
        service_restart,
    )?;
    write_private_bytes(&workspace.worker_path, worker.as_bytes(), false)?;

    let status_dir = workspace
        .status_path
        .parent()
        .context("update status record has no parent directory")?;
    let scheduled = UpdateStatusRecord::scheduled(
        &workspace.id,
        &current_version.to_string(),
        &latest.version.to_string(),
    );
    write_status_record_to(status_dir, &scheduled)?;
    if let Err(error) = cleanup_status_records(status_dir, &workspace.id) {
        let _ = append_update_event(
            &log_path,
            &format!(
                "could not remove old update status records before update {}: {error:#}",
                workspace.id
            ),
        );
    }
    append_update_event(
        &log_path,
        &format!(
            "verified Codexify {} and scheduled update {}",
            latest.version, workspace.id
        ),
    )?;

    if let Err(error) = schedule_worker(&workspace.worker_path, &workspace.id, service_restart) {
        let failed = UpdateStatusRecord {
            state: UpdatePhase::Failed,
            updated_at: update_timestamp(),
            failure_code: Some("schedule_failed".to_string()),
            failure_detail: Some("The detached updater could not be scheduled.".to_string()),
            ..scheduled
        };
        let _ = write_status_record_to(status_dir, &failed);
        workspace.preserve_status();
        let _ = append_update_event(
            &log_path,
            &format!("could not schedule update {}: {error:#}", workspace.id),
        );
        return Err(error);
    }

    let update_id = workspace.id.clone();
    workspace.hand_off();
    Ok(SelfUpdateReceipt {
        status: SelfUpdateStatus::Scheduled,
        current_version: current_version.to_string(),
        target_version: latest.version.to_string(),
        update_id: Some(update_id),
        service_restart,
        log_path: log_path.to_string_lossy().into_owned(),
        changelog,
    })
}

fn installed_executable(home: &Path) -> PathBuf {
    home.join(".codexify")
        .join("bin")
        .join(format!("codexify{}", std::env::consts::EXE_SUFFIX))
}

fn ensure_running_installed_binary(target: &Path) -> anyhow::Result<()> {
    let current = std::env::current_exe().context("locate the running Codexify executable")?;
    let metadata = fs::symlink_metadata(target).with_context(|| {
        format!(
            "self_update requires the standard Codexify installation at {}; install Codexify with the release installer first",
            target.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        bail!(
            "self_update requires a regular installed executable rather than a symlink or directory at {}",
            target.display()
        );
    }
    if !same_file::is_same_file(&current, target).with_context(|| {
        format!(
            "compare the running executable {} with {}",
            current.display(),
            target.display()
        )
    })? {
        bail!(
            "self_update refuses to replace a different installation: the running executable is {}, but the managed executable is {}",
            current.display(),
            target.display()
        );
    }
    Ok(())
}

fn service_supervised() -> bool {
    std::env::var_os(SERVICE_SUPERVISED_ENV).as_deref() == Some(OsStr::new("1"))
}

fn update_client() -> anyhow::Result<Client> {
    release_client(DOWNLOAD_TIMEOUT, "codexify-self-update")
        .context("build Codexify self-update client")
}

fn release_metadata_client(timeout: Duration) -> anyhow::Result<Client> {
    release_client(timeout, "codexify-doctor").context("build Codexify release metadata client")
}

fn release_client(timeout: Duration, user_agent: &str) -> anyhow::Result<Client> {
    Ok(crate::tls::client_builder()
        .redirect(Policy::limited(5))
        .timeout(timeout)
        .user_agent(format!("{user_agent}/{}", env!("CARGO_PKG_VERSION")))
        .build()?)
}

async fn latest_release(client: &Client, source: &ReleaseSource) -> anyhow::Result<ReleaseAsset> {
    let bytes = fetch_bytes(
        client,
        &source.latest_release_url,
        MAX_RELEASE_METADATA_BYTES,
        "latest-release metadata",
    )
    .await?;
    let release: LatestRelease =
        serde_json::from_slice(&bytes).context("parse latest Codexify release metadata")?;
    release_asset(&release.tag_name)
}

async fn fetch_bytes(
    client: &Client,
    url: &str,
    maximum: usize,
    label: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("download {label}"))?
        .error_for_status()
        .with_context(|| format!("download {label}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        bail!("{label} exceeds the {maximum}-byte limit");
    }
    let initial_capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0)
        .min(maximum);
    let mut bytes = Vec::with_capacity(initial_capacity);
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("read {label}"))?
    {
        if bytes.len().saturating_add(chunk.len()) > maximum {
            bail!("{label} exceeds the {maximum}-byte limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn release_asset(tag: &str) -> anyhow::Result<ReleaseAsset> {
    if tag.trim() != tag {
        bail!("latest release tag contains surrounding whitespace");
    }
    let version_text = tag
        .strip_prefix('v')
        .context("latest release tag must start with `v`")?;
    let version = Version::parse(version_text)
        .with_context(|| format!("latest release tag {tag:?} is not semantic versioning"))?;

    let (platform, kind, binary_name) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => ("linux-x64", ArchiveKind::TarGz, "codexify"),
        ("linux", "aarch64") => ("linux-arm64", ArchiveKind::TarGz, "codexify"),
        ("macos", "x86_64") => ("darwin-x64", ArchiveKind::TarGz, "codexify"),
        ("macos", "aarch64") => ("darwin-arm64", ArchiveKind::TarGz, "codexify"),
        ("windows", "x86_64" | "aarch64") => ("windows-x64", ArchiveKind::Zip, "codexify.exe"),
        (os, arch) => bail!("Codexify has no release asset for {os}-{arch}"),
    };
    let extension = match kind {
        ArchiveKind::TarGz => "tar.gz",
        ArchiveKind::Zip => "zip",
    };
    Ok(ReleaseAsset {
        tag: tag.to_string(),
        version,
        archive_name: format!("codexify-{tag}-{platform}.{extension}"),
        binary_name,
        kind,
    })
}

fn checksum_for(checksums: &[u8], archive_name: &str) -> anyhow::Result<String> {
    let text = std::str::from_utf8(checksums).context("release checksums are not UTF-8")?;
    let mut matches = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(hash) = fields.next() else {
            continue;
        };
        let Some(name) = fields.next() else {
            continue;
        };
        if fields.next().is_some() || name.trim_start_matches('*') != archive_name {
            continue;
        }
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("checksums.txt contains an invalid SHA-256 for {archive_name}");
        }
        matches.push(hash.to_ascii_lowercase());
    }
    match matches.as_slice() {
        [hash] => Ok(hash.clone()),
        [] => bail!("checksums.txt does not contain {archive_name}"),
        _ => bail!("checksums.txt contains duplicate entries for {archive_name}"),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ExtractedRelease {
    binary: Vec<u8>,
    changelog: Option<String>,
}

fn extract_release(archive: &[u8], asset: &ReleaseAsset) -> anyhow::Result<ExtractedRelease> {
    match asset.kind {
        ArchiveKind::TarGz => extract_tar_release(archive, asset.binary_name),
        ArchiveKind::Zip => extract_zip_release(archive, asset.binary_name),
    }
}

fn extract_tar_release(archive: &[u8], binary_name: &str) -> anyhow::Result<ExtractedRelease> {
    let decoder = GzDecoder::new(Cursor::new(archive));
    let mut archive = Archive::new(decoder);
    let mut binary = None;
    let mut changelog = None;
    let mut changelog_seen = false;
    for entry in archive.entries().context("read release tar archive")? {
        let mut entry = entry.context("read release tar entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .context("read release tar entry path")?
            .into_owned();
        let Some(file_name) = path.file_name() else {
            continue;
        };
        if file_name == OsStr::new(binary_name) {
            if binary.is_some() {
                bail!("release tar archive contains more than one {binary_name}");
            }
            if entry.size() > MAX_BINARY_BYTES {
                bail!("release executable exceeds the {MAX_BINARY_BYTES}-byte limit");
            }
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry
                .by_ref()
                .take(MAX_BINARY_BYTES + 1)
                .read_to_end(&mut bytes)
                .context("read release executable from tar archive")?;
            if bytes.len() as u64 > MAX_BINARY_BYTES {
                bail!("release executable exceeds the {MAX_BINARY_BYTES}-byte limit");
            }
            binary = Some(bytes);
        } else if file_name == OsStr::new("CHANGELOG.md") {
            if changelog_seen {
                changelog = None;
                continue;
            }
            changelog_seen = true;
            if entry.size() > MAX_CHANGELOG_FILE_BYTES {
                continue;
            }
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry
                .by_ref()
                .take(MAX_CHANGELOG_FILE_BYTES + 1)
                .read_to_end(&mut bytes)
                .context("read release changelog from tar archive")?;
            if bytes.len() as u64 <= MAX_CHANGELOG_FILE_BYTES {
                changelog = String::from_utf8(bytes).ok();
            }
        }
    }
    Ok(ExtractedRelease {
        binary: binary
            .with_context(|| format!("release tar archive does not contain {binary_name}"))?,
        changelog,
    })
}

fn extract_zip_release(archive: &[u8], binary_name: &str) -> anyhow::Result<ExtractedRelease> {
    let mut archive = ZipArchive::new(Cursor::new(archive)).context("open release ZIP")?;
    let mut binary = None;
    let mut changelog = None;
    let mut changelog_seen = false;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("read release ZIP entry {index}"))?;
        if !entry.is_file() {
            continue;
        }
        let file_name = Path::new(entry.name()).file_name().map(OsStr::to_owned);
        if file_name.as_deref() == Some(OsStr::new(binary_name)) {
            if binary.is_some() {
                bail!("release ZIP contains more than one {binary_name}");
            }
            if entry.size() > MAX_BINARY_BYTES {
                bail!("release executable exceeds the {MAX_BINARY_BYTES}-byte limit");
            }
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry
                .by_ref()
                .take(MAX_BINARY_BYTES + 1)
                .read_to_end(&mut bytes)
                .context("read release executable from ZIP")?;
            if bytes.len() as u64 > MAX_BINARY_BYTES {
                bail!("release executable exceeds the {MAX_BINARY_BYTES}-byte limit");
            }
            binary = Some(bytes);
        } else if file_name.as_deref() == Some(OsStr::new("CHANGELOG.md")) {
            if changelog_seen {
                changelog = None;
                continue;
            }
            changelog_seen = true;
            if entry.size() > MAX_CHANGELOG_FILE_BYTES {
                continue;
            }
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry
                .by_ref()
                .take(MAX_CHANGELOG_FILE_BYTES + 1)
                .read_to_end(&mut bytes)
                .context("read release changelog from ZIP")?;
            if bytes.len() as u64 <= MAX_CHANGELOG_FILE_BYTES {
                changelog = String::from_utf8(bytes).ok();
            }
        }
    }
    Ok(ExtractedRelease {
        binary: binary.with_context(|| format!("release ZIP does not contain {binary_name}"))?,
        changelog,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn random_id() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 12];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| anyhow::anyhow!("generate self-update identifier: {error}"))?;
    Ok(hex(&bytes))
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

fn make_private_dir(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_new_file(path: &Path, executable: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    #[cfg(not(unix))]
    let _ = executable;
    options.open(path)
}

fn write_private_bytes(path: &Path, bytes: &[u8], executable: bool) -> anyhow::Result<()> {
    let mut file = private_new_file(path, executable)
        .with_context(|| format!("create staged update file {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }),
        )?;
    }
    Ok(())
}

fn write_staged_binary(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if bytes.is_empty() {
        bail!("release executable is empty");
    }
    write_private_bytes(path, bytes, true)
}

async fn validate_staged_binary(path: &Path) -> anyhow::Result<()> {
    let status = tokio::time::timeout(
        BINARY_PROBE_TIMEOUT,
        Command::new(path)
            .arg("--help")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await
    .context("staged Codexify executable did not finish its validation probe")?
    .with_context(|| format!("start staged Codexify executable {}", path.display()))?;
    if !status.success() {
        bail!("staged Codexify executable failed its validation probe with {status}");
    }
    Ok(())
}

fn make_log_available(path: &Path) -> anyhow::Result<()> {
    let parent = path.parent().context("service log path has no parent")?;
    make_private_dir(parent)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("open service log {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    file.sync_all()?;
    Ok(())
}

fn append_update_event(path: &Path, message: &str) -> anyhow::Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("open service log {}", path.display()))?;
    writeln!(
        file,
        "\n[{}] [self-update] {message}",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )?;
    file.flush()?;
    Ok(())
}

fn worker_script(
    workspace: &UpdateWorkspace,
    target: &Path,
    log_path: &Path,
    from_version: &Version,
    version: &Version,
    restart_service: bool,
) -> anyhow::Result<String> {
    #[cfg(unix)]
    {
        let launchd_label = if cfg!(target_os = "macos") {
            format!("dev.codexify.update.{}", workspace.id)
        } else {
            String::new()
        };
        unix_worker_script(
            workspace,
            target,
            log_path,
            from_version,
            version,
            restart_service,
            &launchd_label,
        )
    }
    #[cfg(windows)]
    {
        windows_worker_script(
            workspace,
            target,
            log_path,
            from_version,
            version,
            restart_service,
            &windows_task_name(&workspace.id),
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (
            workspace,
            target,
            log_path,
            from_version,
            version,
            restart_service,
        );
        bail!("Codexify self-update is supported only on Linux, macOS, and Windows")
    }
}

#[cfg(unix)]
fn unix_worker_script(
    workspace: &UpdateWorkspace,
    target: &Path,
    log_path: &Path,
    from_version: &Version,
    version: &Version,
    restart_service: bool,
    launchd_label: &str,
) -> anyhow::Result<String> {
    let target = sh_path(target)?;
    let staged = sh_path(&workspace.staged_path)?;
    let backup = sh_path(&workspace.backup_path)?;
    let lock = sh_path(&workspace.lock_path)?;
    let script = sh_path(&workspace.worker_path)?;
    let status = sh_path(&workspace.status_path)?;
    let log = sh_path(log_path)?;
    let update_id = sh_quote(&workspace.id);
    let from_version = sh_quote(&from_version.to_string());
    let version = sh_quote(&version.to_string());
    let launchd_label = sh_quote(launchd_label);
    let restart_service = if restart_service { 1 } else { 0 };

    Ok(format!(
        r#"#!/bin/sh
set -eu
umask 077
PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
export PATH
TARGET={target}
STAGED={staged}
BACKUP={backup}
LOCK={lock}
SCRIPT={script}
STATUS={status}
LOG={log}
UPDATE_ID={update_id}
FROM_VERSION={from_version}
VERSION={version}
RESTART_SERVICE={restart_service}
LAUNCHD_LABEL={launchd_label}
STOPPED=0
UPDATED=0
ROLLED_BACK=0
FAILURE_CODE=worker_failed
FAILURE_DETAIL='The detached updater stopped unexpectedly.'

log() {{
    printf '\n[%s] [self-update] %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*" >> "$LOG"
}}

write_status() {{
    state=$1
    failure_code=$2
    failure_detail=$3
    timestamp=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
    temporary="$STATUS.tmp.$$"
    if [ -n "$failure_code" ]; then
        persisted=$(printf '{{"updateId":"%s","fromVersion":"%s","targetVersion":"%s","state":"%s","updatedAt":"%s","failureCode":"%s","failureDetail":"%s"}}' \
            "$UPDATE_ID" "$FROM_VERSION" "$VERSION" "$state" "$timestamp" "$failure_code" "$failure_detail")
    else
        persisted=$(printf '{{"updateId":"%s","fromVersion":"%s","targetVersion":"%s","state":"%s","updatedAt":"%s","failureCode":null,"failureDetail":null}}' \
            "$UPDATE_ID" "$FROM_VERSION" "$VERSION" "$state" "$timestamp")
    fi
    if printf '%s' "$persisted" > "$temporary" && chmod 600 "$temporary" && mv -f "$temporary" "$STATUS"; then
        :
    else
        rm -f "$temporary"
        log "could not persist update status $state"
    fi
    return 0
}}

finish() {{
    status=$?
    trap - 0 HUP INT TERM
    set +e

    if [ "$UPDATED" -ne 1 ] && [ -e "$BACKUP" ]; then
        rm -f "$TARGET"
        if mv -f "$BACKUP" "$TARGET"; then
            ROLLED_BACK=1
            log 'restored the previous Codexify executable'
        else
            FAILURE_CODE=rollback_failed
            FAILURE_DETAIL='The previous Codexify executable could not be restored.'
            log 'failed to restore the previous Codexify executable'
            status=1
        fi
    fi

    if [ "$RESTART_SERVICE" -eq 1 ] && [ "$STOPPED" -eq 1 ]; then
        write_status restarting '' ''
        service_executable=$TARGET
        if [ ! -x "$service_executable" ] && [ -x "$BACKUP" ]; then
            service_executable=$BACKUP
        fi
        if [ -x "$service_executable" ]; then
            if "$service_executable" service enable >> "$LOG" 2>&1; then
                log 'restarted the Codexify service'
            else
                FAILURE_CODE=service_restart_failed
                FAILURE_DETAIL='The Codexify service could not be restarted.'
                log 'failed to restart the Codexify service; run `codexify service enable` manually'
                status=1
            fi
        else
            FAILURE_CODE=executable_missing
            FAILURE_DETAIL='No Codexify executable was available to restart the service.'
            log 'cannot restart the Codexify service because no executable is available'
            status=1
        fi
    fi

    if [ "$UPDATED" -eq 1 ] && [ "$status" -eq 0 ]; then
        rm -f "$BACKUP"
    elif [ "$UPDATED" -eq 1 ] && [ -e "$BACKUP" ]; then
        log "previous executable retained at $BACKUP"
    fi
    rm -f "$STAGED" "$LOCK" "$SCRIPT"
    if [ "$status" -eq 0 ]; then
        write_status succeeded '' ''
        log "Codexify $VERSION self-update completed"
    elif [ "$ROLLED_BACK" -eq 1 ] && [ "$FAILURE_CODE" != service_restart_failed ]; then
        write_status rolled_back "$FAILURE_CODE" "$FAILURE_DETAIL"
        log "Codexify $VERSION self-update rolled back"
    else
        write_status failed "$FAILURE_CODE" "$FAILURE_DETAIL"
        log "Codexify $VERSION self-update failed"
    fi

    if [ -n "$LAUNCHD_LABEL" ]; then
        launchctl remove "$LAUNCHD_LABEL" >/dev/null 2>&1
    fi
    exit "$status"
}}
trap finish 0 HUP INT TERM

log "update worker started for Codexify $VERSION"
sleep {UPDATE_DELAY_SECONDS}
write_status installing '' ''

if [ "$RESTART_SERVICE" -eq 1 ]; then
    STOPPED=1
    if ! "$TARGET" service disable >> "$LOG" 2>&1; then
        FAILURE_CODE=service_stop_failed
        FAILURE_DETAIL='The Codexify service could not be stopped.'
        log 'failed to stop the Codexify service'
        exit 1
    fi
    log 'stopped the Codexify service'
fi

if ! rm -f "$BACKUP"; then
    FAILURE_CODE=backup_failed
    FAILURE_DETAIL='The previous Codexify executable backup could not be prepared.'
    exit 1
fi
if ! ln "$TARGET" "$BACKUP" 2>/dev/null; then
    if ! cp -p "$TARGET" "$BACKUP"; then
        FAILURE_CODE=backup_failed
        FAILURE_DETAIL='The previous Codexify executable could not be backed up.'
        exit 1
    fi
fi
if ! mv -f "$STAGED" "$TARGET"; then
    FAILURE_CODE=replacement_failed
    FAILURE_DETAIL='The installed Codexify executable could not be replaced.'
    exit 1
fi
if ! chmod 755 "$TARGET"; then
    FAILURE_CODE=install_failed
    FAILURE_DETAIL='The installed Codexify executable permissions could not be set.'
    exit 1
fi
if [ "$(uname -s)" = Darwin ]; then
    xattr -d com.apple.quarantine "$TARGET" >/dev/null 2>&1 || true
fi
write_status validating '' ''
if ! "$TARGET" --help >/dev/null 2>&1; then
    FAILURE_CODE=validation_failed
    FAILURE_DETAIL='The replacement Codexify executable failed validation.'
    log 'the replacement executable failed its validation probe'
    exit 1
fi
UPDATED=1
log "installed Codexify $VERSION"
"#
    ))
}

#[cfg(unix)]
fn sh_path(path: &Path) -> anyhow::Result<String> {
    let value = path
        .to_str()
        .with_context(|| format!("self-update path must be valid UTF-8: {}", path.display()))?;
    Ok(sh_quote(value))
}

#[cfg(unix)]
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(any(windows, test))]
fn windows_worker_script(
    workspace: &UpdateWorkspace,
    target: &Path,
    log_path: &Path,
    from_version: &Version,
    version: &Version,
    restart_service: bool,
    task_name: &str,
) -> anyhow::Result<String> {
    let target = powershell_path(target)?;
    let staged = powershell_path(&workspace.staged_path)?;
    let backup = powershell_path(&workspace.backup_path)?;
    let lock = powershell_path(&workspace.lock_path)?;
    let script = powershell_path(&workspace.worker_path)?;
    let status = powershell_path(&workspace.status_path)?;
    let log = powershell_path(log_path)?;
    let update_id = powershell_quote(&workspace.id);
    let from_version = powershell_quote(&from_version.to_string());
    let version = powershell_quote(&version.to_string());
    let task_name = powershell_quote(task_name);
    let restart_service = if restart_service { "$true" } else { "$false" };

    Ok(format!(
        r#"Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Target = {target}
$Staged = {staged}
$Backup = {backup}
$Lock = {lock}
$ScriptPath = {script}
$Status = {status}
$Log = {log}
$UpdateId = {update_id}
$FromVersion = {from_version}
$Version = {version}
$TaskName = {task_name}
$RestartService = {restart_service}
$Stopped = $false
$Updated = $false
$RolledBack = $false
$ExitCode = 0
$FailedReplacement = "$Target.failed.$PID"
$FailureCode = 'worker_failed'
$FailureDetail = 'The detached updater stopped unexpectedly.'

function Write-UpdateLog([string]$Message) {{
    try {{
        $Timestamp = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
        Add-Content -LiteralPath $Log -Value "`n[$Timestamp] [self-update] $Message" -Encoding UTF8
    }} catch {{}}
}}

function Write-UpdateStatus([string]$State, [string]$Code, [string]$Detail) {{
    $Temporary = "$Status.tmp.$PID"
    try {{
        $Timestamp = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
        $Record = [ordered]@{{
            updateId = $UpdateId
            fromVersion = $FromVersion
            targetVersion = $Version
            state = $State
            updatedAt = $Timestamp
            failureCode = $(if ($Code) {{ $Code }} else {{ $null }})
            failureDetail = $(if ($Detail) {{ $Detail }} else {{ $null }})
        }}
        $Json = $Record | ConvertTo-Json -Compress
        $Encoding = New-Object System.Text.UTF8Encoding($false)
        [IO.File]::WriteAllText($Temporary, $Json, $Encoding)
        if (Test-Path -LiteralPath $Status) {{
            [IO.File]::Replace($Temporary, $Status, $null, $true)
        }} else {{
            [IO.File]::Move($Temporary, $Status)
        }}
    }} catch {{
        Remove-Item -LiteralPath $Temporary -Force -ErrorAction SilentlyContinue
        Write-UpdateLog "could not persist update status $State"
    }}
}}

try {{
    Write-UpdateLog "update worker started for Codexify $Version"
    Start-Sleep -Seconds {UPDATE_DELAY_SECONDS}
    Write-UpdateStatus 'installing' '' ''

    if ($RestartService) {{
        $Stopped = $true
        $FailureCode = 'service_stop_failed'
        $FailureDetail = 'The Codexify service could not be stopped.'
        & $Target service disable *>> $Log
        if ($LASTEXITCODE -ne 0) {{ throw 'failed to stop the Codexify service' }}
        Write-UpdateLog 'stopped the Codexify service'
    }}

    $FailureCode = 'backup_failed'
    $FailureDetail = 'The previous Codexify executable could not be backed up.'
    Remove-Item -LiteralPath $Backup, $FailedReplacement -Force -ErrorAction SilentlyContinue
    $FailureCode = 'replacement_failed'
    $FailureDetail = 'The installed Codexify executable could not be replaced.'
    $ReplaceError = $null
    for ($Attempt = 0; $Attempt -lt 60; $Attempt++) {{
        try {{
            [IO.File]::Replace($Staged, $Target, $Backup, $true)
            $ReplaceError = $null
            break
        }} catch {{
            $ReplaceError = $_
            Start-Sleep -Milliseconds 250
        }}
    }}
    if ($ReplaceError) {{ throw "could not atomically replace the installed executable: $($ReplaceError.Exception.Message)" }}

    $FailureCode = 'validation_failed'
    $FailureDetail = 'The replacement Codexify executable failed validation.'
    Write-UpdateStatus 'validating' '' ''
    & $Target --help *> $null
    if ($LASTEXITCODE -ne 0) {{ throw 'the replacement executable failed its validation probe' }}
    $Updated = $true
    Write-UpdateLog "installed Codexify $Version"
}} catch {{
    $ExitCode = 1
    Write-UpdateLog "self-update failed: $($_.Exception.Message)"
    if (-not $Updated -and (Test-Path -LiteralPath $Backup)) {{
        try {{
            [IO.File]::Replace($Backup, $Target, $FailedReplacement, $true)
            Remove-Item -LiteralPath $FailedReplacement -Force -ErrorAction SilentlyContinue
            $RolledBack = $true
            Write-UpdateLog 'restored the previous Codexify executable'
        }} catch {{
            try {{
                Remove-Item -LiteralPath $Target -Force -ErrorAction SilentlyContinue
                Move-Item -LiteralPath $Backup -Destination $Target -Force
                $RolledBack = $true
                Write-UpdateLog 'restored the previous Codexify executable'
            }} catch {{
                $FailureCode = 'rollback_failed'
                $FailureDetail = 'The previous Codexify executable could not be restored.'
                Write-UpdateLog "failed to restore the previous executable: $($_.Exception.Message)"
            }}
        }}
    }}
}} finally {{
    if ($RestartService -and $Stopped) {{
        Write-UpdateStatus 'restarting' '' ''
        $ServiceExecutable = if (Test-Path -LiteralPath $Target) {{ $Target }} elseif (Test-Path -LiteralPath $Backup) {{ $Backup }} else {{ $null }}
        if ($ServiceExecutable) {{
            try {{
                & $ServiceExecutable service enable *>> $Log
                if ($LASTEXITCODE -ne 0) {{ throw 'service enable returned a failure status' }}
                Write-UpdateLog 'restarted the Codexify service'
            }} catch {{
                $FailureCode = 'service_restart_failed'
                $FailureDetail = 'The Codexify service could not be restarted.'
                Write-UpdateLog 'failed to restart the Codexify service; run `codexify service enable` manually'
                $ExitCode = 1
            }}
        }} else {{
            $FailureCode = 'executable_missing'
            $FailureDetail = 'No Codexify executable was available to restart the service.'
            Write-UpdateLog 'cannot restart the Codexify service because no executable is available'
            $ExitCode = 1
        }}
    }}

    if ($Updated -and $ExitCode -eq 0) {{
        Remove-Item -LiteralPath $Backup -Force -ErrorAction SilentlyContinue
    }} elseif ($Updated -and (Test-Path -LiteralPath $Backup)) {{
        Write-UpdateLog "previous executable retained at $Backup"
    }}
    Remove-Item -LiteralPath $Staged -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $FailedReplacement -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $Lock -Force -ErrorAction SilentlyContinue
    if ($ExitCode -eq 0) {{
        Write-UpdateStatus 'succeeded' '' ''
        Write-UpdateLog "Codexify $Version self-update completed"
    }} elseif ($RolledBack -and $FailureCode -ne 'service_restart_failed') {{
        Write-UpdateStatus 'rolled_back' $FailureCode $FailureDetail
        Write-UpdateLog "Codexify $Version self-update rolled back"
    }} else {{
        Write-UpdateStatus 'failed' $FailureCode $FailureDetail
        Write-UpdateLog "Codexify $Version self-update failed"
    }}
    Remove-Item -LiteralPath $ScriptPath -Force -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
}}
exit $ExitCode
"#
    ))
}

#[cfg(any(windows, test))]
fn powershell_path(path: &Path) -> anyhow::Result<String> {
    let value = path
        .to_str()
        .with_context(|| format!("self-update path must be valid UTF-8: {}", path.display()))?;
    Ok(powershell_quote(value))
}

#[cfg(any(windows, test))]
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn schedule_worker(worker: &Path, id: &str, supervised: bool) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        match schedule_systemd(worker, id) {
            Ok(()) => Ok(()),
            Err(error) if !supervised => spawn_detached_unix(worker)
                .with_context(|| format!("systemd transient unit was unavailable ({error:#})")),
            Err(error) => Err(error),
        }
    }
    #[cfg(target_os = "macos")]
    {
        match schedule_launchd(worker, id) {
            Ok(()) => Ok(()),
            Err(error) if !supervised => spawn_detached_unix(worker)
                .with_context(|| format!("launchd submitted job was unavailable ({error:#})")),
            Err(error) => Err(error),
        }
    }
    #[cfg(windows)]
    {
        if !supervised {
            bail!("Windows self-update requires the Codexify background service");
        }
        schedule_windows(worker, id)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (worker, id, supervised);
        bail!("Codexify self-update is supported only on Linux, macOS, and Windows")
    }
}

#[cfg(any(target_os = "linux", test))]
fn systemd_arguments(worker: &Path, id: &str) -> Vec<String> {
    let worker = worker
        .to_string_lossy()
        .replace('%', "%%")
        .replace('$', "$$");
    vec![
        "--user".to_string(),
        "--quiet".to_string(),
        "--collect".to_string(),
        format!("--unit=codexify-update-{id}"),
        "--description=Codexify self-update".to_string(),
        "--property=Type=oneshot".to_string(),
        "--property=TimeoutStartSec=30min".to_string(),
        "--working-directory=/".to_string(),
        "/bin/sh".to_string(),
        worker,
    ]
}

#[cfg(target_os = "linux")]
fn schedule_systemd(worker: &Path, id: &str) -> anyhow::Result<()> {
    let mut command = StdCommand::new("systemd-run");
    command.args(systemd_arguments(worker, id));
    run_checked(
        command,
        "schedule the Codexify update in a transient systemd unit",
    )
}

#[cfg(any(target_os = "macos", test))]
fn launchd_label(id: &str) -> String {
    format!("dev.codexify.update.{id}")
}

#[cfg(target_os = "macos")]
fn schedule_launchd(worker: &Path, id: &str) -> anyhow::Result<()> {
    let mut command = StdCommand::new("launchctl");
    command
        .args(["submit", "-l", &launchd_label(id), "--", "/bin/sh"])
        .arg(worker);
    run_checked(command, "schedule the Codexify update as a launchd job")
}

#[cfg(windows)]
fn windows_task_name(id: &str) -> String {
    format!("Codexify Update {id}")
}

#[cfg(windows)]
fn schedule_windows(worker: &Path, id: &str) -> anyhow::Result<()> {
    let script = windows_registration_script(worker, &windows_task_name(id))?;
    run_checked(
        powershell_command(&script),
        "schedule the Codexify update as a Windows task",
    )
}

#[cfg(any(windows, test))]
fn windows_registration_script(worker: &Path, task_name: &str) -> anyhow::Result<String> {
    let worker = worker
        .to_str()
        .with_context(|| format!("self-update path must be valid UTF-8: {}", worker.display()))?;
    if worker.contains(['\n', '\r', '"']) {
        bail!("Windows self-update script path contains an unsupported character");
    }
    let task_name = powershell_quote(task_name);
    let arguments = powershell_quote(&format!(
        "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{worker}\""
    ));
    let working_dir = powershell_quote(
        worker
            .rsplit_once(['\\', '/'])
            .map_or(".", |(directory, _)| directory),
    );
    Ok(format!(
        "$ErrorActionPreference = 'Stop'\n$TaskName = {task_name}\n$Existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue\nif ($Existing) {{ Stop-ScheduledTask -InputObject $Existing -ErrorAction SilentlyContinue; Unregister-ScheduledTask -InputObject $Existing -Confirm:$false }}\n$Identity = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name\n$Engine = (Get-Process -Id $PID).Path\n$Action = New-ScheduledTaskAction -Execute $Engine -Argument {arguments} -WorkingDirectory {working_dir}\n$Settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 30) -MultipleInstances IgnoreNew\n$Principal = New-ScheduledTaskPrincipal -UserId $Identity -LogonType Interactive -RunLevel Limited\nRegister-ScheduledTask -TaskName $TaskName -Action $Action -Settings $Settings -Principal $Principal -Force | Out-Null\nStart-ScheduledTask -TaskName $TaskName\n"
    ))
}

#[cfg(windows)]
fn powershell_command(script: &str) -> StdCommand {
    use base64::Engine;

    let mut bytes = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let program = if program_exists("powershell.exe") {
        "powershell.exe"
    } else {
        "pwsh.exe"
    };
    let mut command = StdCommand::new(program);
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-EncodedCommand",
        &encoded,
    ]);
    command
}

#[cfg(windows)]
fn program_exists(program: &str) -> bool {
    StdCommand::new("where.exe")
        .arg(program)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn spawn_detached_unix(worker: &Path) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let mut command = StdCommand::new("/bin/sh");
    command
        .arg(worker)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .with_context(|| format!("start detached update worker {}", worker.display()))?;
    Ok(())
}

fn run_checked(mut command: StdCommand, action: &str) -> anyhow::Result<()> {
    let output = command.output().with_context(|| {
        format!(
            "{action}: start {}",
            command.get_program().to_string_lossy()
        )
    })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = bounded_command_output(&output.stderr);
    let stdout = bounded_command_output(&output.stdout);
    let detail = if !stderr.is_empty() { stderr } else { stdout };
    bail!("{action} failed with {}: {detail}", output.status)
}

fn bounded_command_output(bytes: &[u8]) -> String {
    const MAX: usize = 4096;
    let bytes = if bytes.len() > MAX {
        &bytes[bytes.len() - MAX..]
    } else {
        bytes
    };
    String::from_utf8_lossy(bytes).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use zip::write::SimpleFileOptions;

    async fn release_server(body: &'static str, delay: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{address}/latest")
    }

    fn test_asset(kind: ArchiveKind, binary_name: &'static str) -> ReleaseAsset {
        ReleaseAsset {
            tag: "v9.8.7".to_string(),
            version: Version::new(9, 8, 7),
            archive_name: match kind {
                ArchiveKind::TarGz => "codexify-v9.8.7-test.tar.gz",
                ArchiveKind::Zip => "codexify-v9.8.7-test.zip",
            }
            .to_string(),
            binary_name,
            kind,
        }
    }

    fn workspace(root: &Path) -> UpdateWorkspace {
        UpdateWorkspace {
            id: "0123456789abcdef01234567".to_string(),
            lock_path: root.join("update.lock"),
            staged_path: root.join("codexify.new"),
            backup_path: root.join("codexify.old"),
            worker_path: root.join("worker"),
            status_path: root.join("status/0123456789abcdef01234567.json"),
            clean_on_drop: false,
            preserve_status: false,
        }
    }

    #[test]
    fn inspect_update_lock_reports_only_bounded_safe_identifiers() {
        let root = tempfile::tempdir().unwrap();
        assert!(inspect_update_lock_from(root.path()).unwrap().is_none());

        let update_dir = root.path().join(".codexify/update");
        fs::create_dir_all(&update_dir).unwrap();
        let lock_path = update_dir.join(UPDATE_LOCK_FILE);
        fs::write(&lock_path, "0123456789abcdef01234567\n").unwrap();
        let lock = inspect_update_lock_from(root.path()).unwrap().unwrap();
        assert_eq!(lock.path, lock_path);
        assert_eq!(lock.update_id.as_deref(), Some("0123456789abcdef01234567"));

        fs::write(&lock.path, "unsafe owner text with spaces\n").unwrap();
        let lock = inspect_update_lock_from(root.path()).unwrap().unwrap();
        assert_eq!(lock.update_id, None);

        fs::write(&lock.path, vec![b'a'; 5000]).unwrap();
        let lock = inspect_update_lock_from(root.path()).unwrap().unwrap();
        assert_eq!(lock.update_id, None);
    }

    #[test]
    fn release_tag_selects_the_current_platform_asset() {
        let asset = release_asset("v1.2.3").unwrap();
        assert_eq!(asset.version, Version::new(1, 2, 3));
        assert!(asset.archive_name.starts_with("codexify-v1.2.3-"));
        assert!(asset.archive_name.ends_with(match asset.kind {
            ArchiveKind::TarGz => ".tar.gz",
            ArchiveKind::Zip => ".zip",
        }));
        assert!(release_asset("1.2.3").is_err());
        assert!(release_asset("vnot-a-version").is_err());
    }

    #[tokio::test]
    async fn doctor_latest_version_inspection_compares_current_and_published_versions() {
        for (current, expected) in [
            ("1.2.2", LatestVersionStatus::UpdateAvailable),
            ("1.2.3", LatestVersionStatus::UpToDate),
            ("1.2.4", LatestVersionStatus::AheadOfLatest),
        ] {
            let source = ReleaseSource {
                latest_release_url: release_server(r#"{"tag_name":"v1.2.3"}"#, Duration::ZERO)
                    .await,
                release_root: "http://unused.invalid".to_string(),
            };
            let inspection = inspect_latest_version_from(source, current, Duration::from_secs(1))
                .await
                .unwrap();
            assert_eq!(inspection.status, expected);
            assert_eq!(inspection.current, Version::parse(current).unwrap());
            assert_eq!(inspection.latest, Version::new(1, 2, 3));
        }
    }

    #[tokio::test]
    async fn doctor_latest_version_inspection_is_bounded_and_rejects_bad_metadata() {
        let malformed = ReleaseSource {
            latest_release_url: release_server(r#"{"tag_name":7}"#, Duration::ZERO).await,
            release_root: "http://unused.invalid".to_string(),
        };
        let error = inspect_latest_version_from(malformed, "1.2.3", Duration::from_secs(1))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("parse latest Codexify release metadata"));

        let slow = ReleaseSource {
            latest_release_url: release_server(
                r#"{"tag_name":"v1.2.3"}"#,
                Duration::from_millis(250),
            )
            .await,
            release_root: "http://unused.invalid".to_string(),
        };
        let error = inspect_latest_version_from(slow, "1.2.3", Duration::from_millis(50))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("download latest-release metadata"));
    }

    #[test]
    fn checksum_parser_requires_one_exact_valid_entry() {
        let name = "codexify-v1.2.3-linux-x64.tar.gz";
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let text = format!("{hash}  other\n{hash} *{name}\n");
        assert_eq!(checksum_for(text.as_bytes(), name).unwrap(), hash);
        assert!(checksum_for(format!("{hash}  other\n").as_bytes(), name).is_err());
        assert!(
            checksum_for(format!("{hash}  {name}\n{hash}  {name}\n").as_bytes(), name).is_err()
        );
    }

    #[test]
    fn changelog_selection_includes_every_release_in_the_upgrade_interval() {
        let changelog = "# Changelog\n\n## [1.3.0] - 2026-08-31\n\n- Three.\n\n## [1.2.0] - 2026-08-30\n\n- Two.\n\n## [1.1.0] - 2026-08-29\n\n- One.\n\n## [1.0.0] - 2026-08-28\n\n- Initial.\n";
        let selected =
            select_changelog_sections(changelog, &Version::new(1, 0, 0), &Version::new(1, 2, 0))
                .unwrap();

        assert!(selected.starts_with("## [1.2.0]"));
        assert!(selected.contains("## [1.1.0]"));
        assert!(!selected.contains("## [1.3.0]"));
        assert!(!selected.contains("## [1.0.0]"));
    }

    #[test]
    fn changelog_selection_returns_none_for_missing_or_oversized_notes() {
        assert!(
            select_changelog_sections(
                "# Changelog\n\n## [1.0.0]\n\n- Initial.\n",
                &Version::new(1, 0, 0),
                &Version::new(1, 1, 0),
            )
            .is_none()
        );

        let oversized = format!(
            "# Changelog\n\n## [2.0.0]\n\n{}",
            "x".repeat(MAX_SELECTED_CHANGELOG_BYTES + 1)
        );
        assert!(
            select_changelog_sections(&oversized, &Version::new(1, 0, 0), &Version::new(2, 0, 0),)
                .is_none()
        );
    }

    #[test]
    fn status_record_round_trips_through_a_private_atomic_file() {
        let root = tempfile::tempdir().unwrap();
        let status_dir = root.path().join("status");
        let record = UpdateStatusRecord::scheduled("0123456789abcdef01234567", "1.0.0", "2.0.0");

        write_status_record_to(&status_dir, &record).unwrap();
        assert_eq!(
            read_status_record_from(&status_dir, &record.update_id).unwrap(),
            record
        );
        assert_eq!(
            fs::read_dir(&status_dir).unwrap().count(),
            1,
            "the atomic temporary file must not remain"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&status_dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(status_record_path(&status_dir, &record.update_id))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn status_record_reader_rejects_malformed_ids_and_corrupt_records() {
        let root = tempfile::tempdir().unwrap();
        let status_dir = root.path().join("status");
        make_private_dir(&status_dir).unwrap();

        assert!(read_status_record_from(&status_dir, "../update").is_err());

        let id = "0123456789abcdef01234567";
        fs::write(status_record_path(&status_dir, id), b"not json").unwrap();
        assert!(read_status_record_from(&status_dir, id).is_err());
    }

    #[test]
    fn status_record_cleanup_keeps_the_active_and_newest_records() {
        let root = tempfile::tempdir().unwrap();
        let status_dir = root.path().join("status");
        for index in 0..(MAX_STATUS_RECORDS + 3) {
            let id = format!("{index:024x}");
            let record = UpdateStatusRecord::scheduled(&id, "1.0.0", "2.0.0");
            write_status_record_to(&status_dir, &record).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        let active = format!("{:024x}", MAX_STATUS_RECORDS + 2);

        cleanup_status_records(&status_dir, &active).unwrap();

        assert!(status_record_path(&status_dir, &active).is_file());
        assert!(fs::read_dir(&status_dir).unwrap().count() <= MAX_STATUS_RECORDS);
        assert!(!status_record_path(&status_dir, &format!("{:024x}", 0)).exists());
    }

    #[test]
    fn extracts_one_bounded_binary_from_tar_gzip() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut builder = tar::Builder::new(&mut encoder);
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o755);
            header.set_size(4);
            header.set_cksum();
            builder
                .append_data(&mut header, "codexify-v9/bin/codexify", &b"test"[..])
                .unwrap();
            let changelog = b"# Changelog\n\n## [9.8.7]\n\n- Test release.\n";
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(changelog.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, "codexify-v9/CHANGELOG.md", &changelog[..])
                .unwrap();
            builder.finish().unwrap();
        }
        let archive = encoder.finish().unwrap();
        let release =
            extract_release(&archive, &test_asset(ArchiveKind::TarGz, "codexify")).unwrap();
        assert_eq!(release.binary, b"test");
        assert_eq!(
            release.changelog.as_deref(),
            Some("# Changelog\n\n## [9.8.7]\n\n- Test release.\n")
        );
    }

    #[test]
    fn extracts_one_bounded_binary_from_zip() {
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        writer
            .start_file("codexify-v9/bin/codexify.exe", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"test").unwrap();
        writer
            .start_file("codexify-v9/CHANGELOG.md", SimpleFileOptions::default())
            .unwrap();
        writer
            .write_all(b"# Changelog\n\n## [9.8.7]\n\n- Test release.\n")
            .unwrap();
        let archive = writer.finish().unwrap().into_inner();
        let release =
            extract_release(&archive, &test_asset(ArchiveKind::Zip, "codexify.exe")).unwrap();
        assert_eq!(release.binary, b"test");
        assert_eq!(
            release.changelog.as_deref(),
            Some("# Changelog\n\n## [9.8.7]\n\n- Test release.\n")
        );
    }

    #[test]
    fn duplicate_or_invalid_changelog_does_not_block_binary_extraction() {
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        writer
            .start_file("codexify-v9/codexify.exe", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"test").unwrap();
        writer
            .start_file("codexify-v9/CHANGELOG.md", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"first").unwrap();
        writer
            .start_file(
                "codexify-v9/docs/CHANGELOG.md",
                SimpleFileOptions::default(),
            )
            .unwrap();
        writer.write_all(b"second").unwrap();
        let archive = writer.finish().unwrap().into_inner();

        let release =
            extract_release(&archive, &test_asset(ArchiveKind::Zip, "codexify.exe")).unwrap();
        assert_eq!(release.binary, b"test");
        assert!(release.changelog.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn unix_worker_stops_swaps_rolls_back_and_restarts() {
        let root = Path::new("/tmp/codexify update/Paul's test");
        let workspace = workspace(root);
        let script = unix_worker_script(
            &workspace,
            &root.join("codexify"),
            &root.join("codexify.log"),
            &Version::new(1, 0, 0),
            &Version::new(2, 0, 0),
            true,
            "dev.codexify.update.test",
        )
        .unwrap();
        assert!(script.contains("service disable"));
        assert!(script.contains("service enable"));
        assert!(script.contains("mv -f \"$STAGED\" \"$TARGET\""));
        assert!(script.contains("restored the previous Codexify executable"));
        assert!(script.contains(&format!("sleep {UPDATE_DELAY_SECONDS}")));
        assert!(script.contains("xattr -d com.apple.quarantine"));
        assert!(script.contains("launchctl remove \"$LAUNCHD_LABEL\""));
        assert!(script.contains("write_status installing"));
        assert!(script.contains("write_status validating"));
        assert!(script.contains("write_status restarting"));
        assert!(script.contains("write_status succeeded"));
        assert!(script.contains("write_status rolled_back"));
        assert!(!script.contains("codex-free"));
    }

    #[test]
    fn windows_worker_persists_restart_safe_status_transitions() {
        let root = Path::new(r"C:\Users\Paul\Codexify Update");
        let workspace = workspace(root);
        let script = windows_worker_script(
            &workspace,
            &root.join("codexify.exe"),
            &root.join("codexify.log"),
            &Version::new(1, 0, 0),
            &Version::new(2, 0, 0),
            true,
            "Codexify Update test",
        )
        .unwrap();

        assert!(script.contains("Write-UpdateStatus 'installing'"));
        assert!(script.contains("Write-UpdateStatus 'validating'"));
        assert!(script.contains("Write-UpdateStatus 'restarting'"));
        assert!(script.contains("Write-UpdateStatus 'succeeded'"));
        assert!(script.contains("Write-UpdateStatus 'rolled_back'"));
    }

    #[test]
    fn service_manager_launches_are_external_and_transient() {
        let arguments = systemd_arguments(Path::new("/tmp/worker.sh"), "abc123");
        assert!(arguments.contains(&"--collect".to_string()));
        assert!(arguments.contains(&"--unit=codexify-update-abc123".to_string()));
        assert_eq!(launchd_label("abc123"), "dev.codexify.update.abc123");
    }

    #[test]
    fn windows_task_is_on_demand_and_self_removing() {
        let script = windows_registration_script(
            Path::new(r"C:\Users\Paul\update worker.ps1"),
            "Codexify Update abc123",
        )
        .unwrap();
        assert!(script.contains("Register-ScheduledTask"));
        assert!(script.contains("Start-ScheduledTask"));
        assert!(!script.contains("New-ScheduledTaskTrigger"));
    }

    #[cfg(windows)]
    #[test]
    fn generated_windows_scripts_parse() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path());
        let task_name = windows_task_name(&workspace.id);
        let worker = windows_worker_script(
            &workspace,
            &root.path().join("codexify.exe"),
            &root.path().join("codexify.log"),
            &Version::new(1, 0, 0),
            &Version::new(2, 0, 0),
            true,
            &task_name,
        )
        .unwrap();
        let registration = windows_registration_script(&workspace.worker_path, &task_name).unwrap();
        for (name, contents) in [("worker.ps1", worker), ("register.ps1", registration)] {
            let path = root.path().join(name);
            fs::write(&path, contents).unwrap();
            let path = powershell_quote(&path.to_string_lossy());
            let parser = format!(
                "$tokens = $null\n$errors = $null\n[System.Management.Automation.Language.Parser]::ParseFile({path}, [ref]$tokens, [ref]$errors) | Out-Null\nif ($errors.Count) {{ $errors | ForEach-Object {{ Write-Error $_ }}; exit 1 }}\n"
            );
            let output = powershell_command(&parser).output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
