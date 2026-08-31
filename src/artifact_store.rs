use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use getrandom::getrandom;
use serde::{Deserialize, Serialize};

pub const ARTIFACT_RECORD_VERSION: u32 = 1;
pub const ARTIFACT_TOKEN_LENGTH: usize = 43;
const TOKEN_BYTES: usize = 32;
const TOKEN_ATTEMPTS: usize = 8;
const MAX_RECORD_BYTES: u64 = 64 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_NAME_BYTES: usize = 1024;
const MAX_MIME_BYTES: usize = 1024;
const LOCK_STALE_MS: u128 = 10 * 60 * 1_000;
const LOCK_TIMEOUT_MS: u128 = 5 * 60 * 1_000;
const LOCK_RETRY_MS: u64 = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRecord {
    pub version: u32,
    pub token: String,
    pub source_root: String,
    pub source_path: String,
    pub source_root_identity: Option<SourceRootIdentity>,
    pub name: String,
    pub mime_type: String,
    pub original_bytes: u64,
    pub original_sha256: String,
    pub created_at_unix_ms: u64,
    pub last_accessed_at_unix_ms: u64,
    pub snapshot_stored: bool,
}

#[derive(Debug, Clone)]
pub struct NewArtifactRecord {
    pub source_root: String,
    pub source_path: String,
    pub source_root_identity: Option<SourceRootIdentity>,
    pub name: String,
    pub mime_type: String,
    pub original_bytes: u64,
    pub original_sha256: String,
    pub created_at_unix_ms: u64,
    pub last_accessed_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SourceRootIdentity {
    Unix { device: u64, inode: u64 },
}

#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
}

pub struct StagedSnapshot {
    temporary: tempfile::NamedTempFile,
    byte_count: u64,
}

pub enum StoredPayload {
    Snapshot { file: File, record: ArtifactRecord },
    SourceOnly { record: ArtifactRecord },
}

impl StagedSnapshot {
    pub fn writer(&mut self) -> &mut File {
        self.temporary.as_file_mut()
    }

    pub fn set_byte_count(&mut self, byte_count: u64) {
        self.byte_count = byte_count;
    }
}

struct StoreLock {
    path: PathBuf,
}

struct SnapshotCandidate {
    token: String,
    path: PathBuf,
    byte_count: u64,
    record: ArtifactRecord,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl ArtifactStore {
    pub fn for_current_user() -> Result<Self, String> {
        let home = crate::util::home_dir()
            .ok_or_else(|| "Could not determine the current user's home directory".to_string())?;
        let store = Self::new_at(home.join(".codexify").join("artifacts"));
        store.ensure_layout()?;
        Ok(store)
    }

    pub fn new_at(root: PathBuf) -> Self {
        Self { root }
    }

    fn records_dir(&self) -> PathBuf {
        self.root.join("records")
    }

    fn snapshots_dir(&self) -> PathBuf {
        self.root.join("snapshots")
    }

    fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }

    fn record_path(&self, token: &str) -> PathBuf {
        self.records_dir().join(format!("{token}.json"))
    }

    fn snapshot_path(&self, token: &str) -> PathBuf {
        self.snapshots_dir().join(format!("{token}.blob"))
    }

    pub fn stage_snapshot(&self) -> Result<StagedSnapshot, String> {
        self.ensure_layout()?;
        let temporary = tempfile::NamedTempFile::new_in(self.snapshots_dir())
            .map_err(|error| format!("Could not stage an immutable artifact snapshot: {error}"))?;
        make_private_file(temporary.path()).map_err(|error| {
            format!("Could not restrict an immutable artifact snapshot: {error}")
        })?;
        Ok(StagedSnapshot {
            temporary,
            byte_count: 0,
        })
    }

    pub fn publish_export(
        &self,
        new: NewArtifactRecord,
        mut staged: Option<StagedSnapshot>,
        max_snapshot_bytes: u64,
    ) -> Result<ArtifactRecord, String> {
        validate_new_record(&new)?;
        self.ensure_layout()?;
        let _lock = self.acquire_store_lock()?;
        let token = (0..TOKEN_ATTEMPTS)
            .find_map(|_| {
                let token = generate_token().ok()?;
                (!self.record_path(&token).exists() && !self.snapshot_path(&token).exists())
                    .then_some(token)
            })
            .ok_or_else(|| "Could not allocate a unique artifact capability".to_string())?;

        if staged.as_ref().is_some_and(|snapshot| {
            snapshot.byte_count != new.original_bytes || snapshot.byte_count > max_snapshot_bytes
        }) {
            staged = None;
        }

        if let Some(snapshot) = staged.as_ref()
            && !self.make_snapshot_room(snapshot.byte_count, max_snapshot_bytes)?
        {
            staged = None;
        }

        let snapshot_path = self.snapshot_path(&token);
        let mut snapshot_stored = false;
        if let Some(mut snapshot) = staged {
            let durable = snapshot
                .temporary
                .as_file_mut()
                .flush()
                .and_then(|_| snapshot.temporary.as_file().sync_all())
                .is_ok();
            if durable {
                match snapshot.temporary.persist_noclobber(&snapshot_path) {
                    Ok(_) => {
                        snapshot_stored = true;
                        sync_directory(&self.snapshots_dir());
                    }
                    Err(error) => {
                        tracing::warn!(
                            error = %error.error,
                            "immutable artifact snapshot publication failed; using source fallback"
                        );
                    }
                }
            }
        }

        let record = ArtifactRecord {
            version: ARTIFACT_RECORD_VERSION,
            token,
            source_root: new.source_root,
            source_path: new.source_path,
            source_root_identity: new.source_root_identity,
            name: new.name,
            mime_type: new.mime_type,
            original_bytes: new.original_bytes,
            original_sha256: new.original_sha256,
            created_at_unix_ms: new.created_at_unix_ms,
            last_accessed_at_unix_ms: new.last_accessed_at_unix_ms,
            snapshot_stored,
        };
        if let Err(error) = self.persist_record(&record) {
            if snapshot_stored {
                let _ = std::fs::remove_file(&snapshot_path);
            }
            return Err(error);
        }
        Ok(record)
    }

    pub fn persist_record(&self, record: &ArtifactRecord) -> Result<(), String> {
        validate_record(record, &record.token)?;
        self.ensure_layout()?;
        let records = self.records_dir();
        let target = self.record_path(&record.token);
        let json = serde_json::to_vec_pretty(record)
            .map_err(|error| format!("Could not serialize artifact record: {error}"))?;
        if json.len() as u64 > MAX_RECORD_BYTES {
            return Err("Artifact record exceeds the supported size".to_string());
        }

        let mut temporary = tempfile::NamedTempFile::new_in(&records).map_err(|error| {
            format!(
                "Could not create temporary artifact record in {}: {error}",
                records.display()
            )
        })?;
        make_private_file(temporary.path()).map_err(|error| {
            format!(
                "Could not restrict temporary artifact record {}: {error}",
                temporary.path().display()
            )
        })?;
        temporary
            .write_all(&json)
            .and_then(|_| temporary.write_all(b"\n"))
            .and_then(|_| temporary.as_file().sync_all())
            .map_err(|error| format!("Could not write artifact record: {error}"))?;
        temporary.persist(&target).map_err(|error| {
            format!(
                "Could not publish artifact record {}: {}",
                target.display(),
                error.error
            )
        })?;
        sync_directory(&records);
        Ok(())
    }

    pub fn load_record(&self, token: &str) -> Result<Option<ArtifactRecord>, String> {
        validate_token(token)?;
        self.ensure_layout()?;
        let path = self.record_path(token);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "Could not inspect artifact record {}: {error}",
                    path.display()
                ));
            }
        };
        validate_private_regular_file(&path, &metadata, MAX_RECORD_BYTES)?;

        let file = open_file_no_follow(&path)?;
        let opened = file.metadata().map_err(|error| {
            format!(
                "Could not inspect opened artifact record {}: {error}",
                path.display()
            )
        })?;
        validate_private_regular_file(&path, &opened, MAX_RECORD_BYTES)?;

        let mut bytes = Vec::with_capacity(opened.len().min(MAX_RECORD_BYTES) as usize);
        file.take(MAX_RECORD_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| {
                format!("Could not read artifact record {}: {error}", path.display())
            })?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(format!(
                "Artifact record {} exceeds the supported size",
                path.display()
            ));
        }
        let record: ArtifactRecord = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Artifact record {} is invalid: {error}", path.display()))?;
        validate_record(&record, token)?;
        Ok(Some(record))
    }

    pub fn resolve_payload(&self, token: &str) -> Result<Option<StoredPayload>, String> {
        self.ensure_layout()?;
        let _lock = self.acquire_store_lock()?;
        let Some(mut record) = self.load_record(token)? else {
            return Ok(None);
        };
        let path = self.snapshot_path(token);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.mark_snapshot_missing(&mut record);
                return Ok(Some(StoredPayload::SourceOnly { record }));
            }
            Err(error) => {
                return Err(format!(
                    "Could not inspect immutable artifact snapshot {}: {error}",
                    path.display()
                ));
            }
        };
        if validate_private_regular_file(&path, &metadata, record.original_bytes).is_err()
            || metadata.len() != record.original_bytes
        {
            self.mark_snapshot_missing(&mut record);
            return Ok(Some(StoredPayload::SourceOnly { record }));
        }
        let file = match open_file_no_follow(&path) {
            Ok(file) => file,
            Err(_error) if std::fs::symlink_metadata(&path).is_err() => {
                self.mark_snapshot_missing(&mut record);
                return Ok(Some(StoredPayload::SourceOnly { record }));
            }
            Err(error) => return Err(error),
        };
        let opened = file.metadata().map_err(|error| {
            format!(
                "Could not inspect opened immutable artifact snapshot {}: {error}",
                path.display()
            )
        })?;
        if validate_private_regular_file(&path, &opened, record.original_bytes).is_err()
            || opened.len() != record.original_bytes
        {
            self.mark_snapshot_missing(&mut record);
            return Ok(Some(StoredPayload::SourceOnly { record }));
        }
        record.snapshot_stored = true;
        record.last_accessed_at_unix_ms =
            now_unix_ms_u64().max(record.last_accessed_at_unix_ms.saturating_add(1));
        if let Err(error) = self.persist_record(&record) {
            tracing::warn!(
                %error,
                "could not refresh immutable artifact snapshot recency"
            );
        }
        Ok(Some(StoredPayload::Snapshot { file, record }))
    }

    fn make_snapshot_room(
        &self,
        incoming_bytes: u64,
        max_snapshot_bytes: u64,
    ) -> Result<bool, String> {
        if incoming_bytes == 0 {
            return Ok(true);
        }
        if max_snapshot_bytes == 0 || incoming_bytes > max_snapshot_bytes {
            return Ok(false);
        }

        let (mut occupied, mut candidates) = self.snapshot_inventory()?;
        if occupied.saturating_add(incoming_bytes) <= max_snapshot_bytes {
            return Ok(true);
        }
        candidates.sort_by(|left, right| {
            left.record
                .last_accessed_at_unix_ms
                .cmp(&right.record.last_accessed_at_unix_ms)
                .then_with(|| {
                    left.record
                        .created_at_unix_ms
                        .cmp(&right.record.created_at_unix_ms)
                })
                .then_with(|| left.token.cmp(&right.token))
        });

        for mut candidate in candidates {
            if occupied.saturating_add(incoming_bytes) <= max_snapshot_bytes {
                break;
            }
            match std::fs::remove_file(&candidate.path) {
                Ok(()) => {
                    occupied = occupied.saturating_sub(candidate.byte_count);
                    candidate.record.snapshot_stored = false;
                    if let Err(error) = self.persist_record(&candidate.record) {
                        tracing::warn!(
                            %error,
                            "could not persist immutable artifact snapshot eviction state"
                        );
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    occupied = occupied.saturating_sub(candidate.byte_count);
                }
                Err(error) => {
                    tracing::debug!(
                        %error,
                        "immutable artifact snapshot could not be evicted"
                    );
                }
            }
        }
        sync_directory(&self.snapshots_dir());
        Ok(occupied.saturating_add(incoming_bytes) <= max_snapshot_bytes)
    }

    fn snapshot_inventory(&self) -> Result<(u64, Vec<SnapshotCandidate>), String> {
        let mut occupied = 0_u64;
        let mut candidates = Vec::new();
        let directory = self.snapshots_dir();
        for entry in std::fs::read_dir(&directory).map_err(|error| {
            format!(
                "Could not inspect immutable artifact snapshot directory {}: {error}",
                directory.display()
            )
        })? {
            let entry = entry.map_err(|error| {
                format!("Could not inspect an immutable artifact snapshot entry: {error}")
            })?;
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            let Some(token) = file_name.strip_suffix(".blob") else {
                continue;
            };
            if validate_token(token).is_err() {
                continue;
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                format!(
                    "Could not inspect immutable artifact snapshot {}: {error}",
                    path.display()
                )
            })?;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || !private_file_permissions(&metadata)
            {
                return Err(format!(
                    "Immutable artifact snapshot entry is unsafe: {}",
                    path.display()
                ));
            }
            let record = match self.load_record(token) {
                Ok(Some(record)) => record,
                Ok(None) => {
                    if let Err(error) = std::fs::remove_file(&path)
                        && error.kind() != std::io::ErrorKind::NotFound
                    {
                        tracing::warn!(
                            %error,
                            "could not reclaim orphan immutable artifact snapshot"
                        );
                    }
                    continue;
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "discarding immutable artifact snapshot with invalid record"
                    );
                    if let Err(remove_error) = std::fs::remove_file(&path)
                        && remove_error.kind() != std::io::ErrorKind::NotFound
                    {
                        tracing::warn!(
                            error = %remove_error,
                            "could not reclaim immutable artifact snapshot with invalid record"
                        );
                    }
                    continue;
                }
            };
            let byte_count = metadata.len();
            occupied = occupied.saturating_add(byte_count);
            candidates.push(SnapshotCandidate {
                token: token.to_string(),
                path,
                byte_count,
                record,
            });
        }
        Ok((occupied, candidates))
    }

    fn mark_snapshot_missing(&self, record: &mut ArtifactRecord) {
        if !record.snapshot_stored {
            return;
        }
        record.snapshot_stored = false;
        if let Err(error) = self.persist_record(record) {
            tracing::warn!(
                %error,
                "could not persist missing immutable artifact snapshot state"
            );
        }
    }

    fn ensure_layout(&self) -> Result<(), String> {
        ensure_private_directory(&self.root)?;
        ensure_private_directory(&self.records_dir())?;
        ensure_private_directory(&self.snapshots_dir())?;
        ensure_private_directory(&self.locks_dir())?;
        Ok(())
    }

    fn acquire_store_lock(&self) -> Result<StoreLock, String> {
        self.ensure_layout()?;
        let path = self.locks_dir().join("store.lock");
        let deadline = now_unix_ms().saturating_add(LOCK_TIMEOUT_MS);
        loop {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
                options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
            }
            match options.open(&path) {
                Ok(mut file) => {
                    make_private_file(&path).map_err(|error| {
                        format!("Could not restrict artifact-store lock: {error}")
                    })?;
                    writeln!(file, "{} {}", std::process::id(), now_unix_ms()).map_err(
                        |error| format!("Could not initialize artifact-store lock: {error}"),
                    )?;
                    return Ok(StoreLock { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                        format!("Could not inspect artifact-store lock: {error}")
                    })?;
                    if !metadata.is_file() || metadata.file_type().is_symlink() {
                        return Err("Artifact-store lock path is unsafe".to_string());
                    }
                    let age = metadata
                        .modified()
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                        .map(|duration| duration.as_millis())
                        .unwrap_or(0);
                    if age > LOCK_STALE_MS {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if now_unix_ms() >= deadline {
                        return Err("Timed out waiting for the artifact-store lock".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(LOCK_RETRY_MS));
                }
                Err(error) => {
                    return Err(format!("Could not acquire artifact-store lock: {error}"));
                }
            }
        }
    }

    #[cfg(test)]
    fn record_path_for_test(&self, token: &str) -> PathBuf {
        self.record_path(token)
    }

    #[cfg(test)]
    fn records_dir_for_test(&self) -> PathBuf {
        self.records_dir()
    }

    #[cfg(test)]
    fn snapshots_dir_for_test(&self) -> PathBuf {
        self.snapshots_dir()
    }
}

fn validate_token(token: &str) -> Result<(), String> {
    if token.len() != ARTIFACT_TOKEN_LENGTH
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err("Artifact capability token is malformed".to_string());
    }
    Ok(())
}

fn validate_record(record: &ArtifactRecord, expected_token: &str) -> Result<(), String> {
    if record.version != ARTIFACT_RECORD_VERSION {
        return Err(format!(
            "Unsupported artifact record version {}",
            record.version
        ));
    }
    validate_token(&record.token)?;
    if record.token != expected_token {
        return Err("Artifact record token does not match its capability".to_string());
    }
    validate_bounded_text(&record.source_root, MAX_PATH_BYTES, "source root")?;
    if !Path::new(&record.source_root).is_absolute() {
        return Err("Artifact record source root must be absolute".to_string());
    }
    validate_relative_path(&record.source_path)?;
    validate_bounded_text(&record.name, MAX_NAME_BYTES, "name")?;
    validate_bounded_text(&record.mime_type, MAX_MIME_BYTES, "MIME type")?;
    if record.original_sha256.len() != 64
        || !record
            .original_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err("Artifact record SHA-256 is malformed".to_string());
    }
    Ok(())
}

fn validate_new_record(record: &NewArtifactRecord) -> Result<(), String> {
    let placeholder = ArtifactRecord {
        version: ARTIFACT_RECORD_VERSION,
        token: "A".repeat(ARTIFACT_TOKEN_LENGTH),
        source_root: record.source_root.clone(),
        source_path: record.source_path.clone(),
        source_root_identity: record.source_root_identity.clone(),
        name: record.name.clone(),
        mime_type: record.mime_type.clone(),
        original_bytes: record.original_bytes,
        original_sha256: record.original_sha256.clone(),
        created_at_unix_ms: record.created_at_unix_ms,
        last_accessed_at_unix_ms: record.last_accessed_at_unix_ms,
        snapshot_stored: false,
    };
    validate_record(&placeholder, &placeholder.token)
}

fn generate_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn now_unix_ms_u64() -> u64 {
    now_unix_ms().min(u64::MAX as u128) as u64
}

fn validate_bounded_text(value: &str, max_bytes: usize, label: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(format!("Artifact record {label} is invalid"));
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> Result<(), String> {
    validate_bounded_text(value, MAX_PATH_BYTES, "source path")?;
    if value.ends_with('/') || value.ends_with('\\') {
        return Err("Artifact record source path is invalid".to_string());
    }
    let mut saw_normal = false;
    for component in Path::new(value).components() {
        match component {
            Component::Normal(_) => saw_normal = true,
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                return Err("Artifact record source path is invalid".to_string());
            }
        }
    }
    if !saw_normal {
        return Err("Artifact record source path is invalid".to_string());
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_directory(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path).map_err(|error| {
                format!(
                    "Could not create artifact-store directory {}: {error}",
                    path.display()
                )
            })?;
            make_private_directory(path).map_err(|error| {
                format!(
                    "Could not restrict artifact-store directory {}: {error}",
                    path.display()
                )
            })?;
            let metadata = std::fs::symlink_metadata(path).map_err(|error| {
                format!(
                    "Could not inspect artifact-store directory {}: {error}",
                    path.display()
                )
            })?;
            validate_private_directory(path, &metadata)
        }
        Err(error) => Err(format!(
            "Could not inspect artifact-store directory {}: {error}",
            path.display()
        )),
    }
}

fn validate_private_directory(path: &Path, metadata: &std::fs::Metadata) -> Result<(), String> {
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || !private_directory_permissions(metadata)
    {
        return Err(format!(
            "Artifact-store directory is not a private regular directory: {}",
            path.display()
        ));
    }
    Ok(())
}

fn validate_private_regular_file(
    path: &Path,
    metadata: &std::fs::Metadata,
    max_bytes: u64,
) -> Result<(), String> {
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || !private_file_permissions(metadata)
        || metadata.len() > max_bytes
    {
        return Err(format!(
            "Artifact-store file is invalid or not private: {}",
            path.display()
        ));
    }
    Ok(())
}

fn open_file_no_follow(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path).map_err(|error| {
        format!(
            "Could not open artifact-store file {}: {error}",
            path.display()
        )
    })
}

fn make_private_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn make_private_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn private_directory_permissions(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o077 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

fn private_file_permissions(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o077 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

fn sync_directory(path: &Path) {
    #[cfg(unix)]
    {
        if let Ok(directory) = File::open(path) {
            let _ = directory.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn fixture_record(
        token: &str,
        source_root: &str,
        source_path: &str,
        bytes: u64,
    ) -> ArtifactRecord {
        ArtifactRecord {
            version: ARTIFACT_RECORD_VERSION,
            token: token.to_string(),
            source_root: source_root.to_string(),
            source_path: source_path.to_string(),
            source_root_identity: None,
            name: Path::new(source_path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            mime_type: "application/octet-stream".to_string(),
            original_bytes: bytes,
            original_sha256: "0".repeat(64),
            created_at_unix_ms: 1,
            last_accessed_at_unix_ms: 1,
            snapshot_stored: false,
        }
    }

    fn new_record(source_root: &str, source_path: &str, bytes: u64) -> NewArtifactRecord {
        NewArtifactRecord {
            source_root: source_root.to_string(),
            source_path: source_path.to_string(),
            source_root_identity: None,
            name: Path::new(source_path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            mime_type: "application/octet-stream".to_string(),
            original_bytes: bytes,
            original_sha256: "0".repeat(64),
            created_at_unix_ms: 1,
            last_accessed_at_unix_ms: 1,
        }
    }

    fn write_private_test_file(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn record_survives_store_reconstruction() {
        let state = tempfile::tempdir().unwrap();
        let source_root = state.path().to_string_lossy().into_owned();
        let store = ArtifactStore::new_at(state.path().join("artifacts"));
        let token = "A".repeat(ARTIFACT_TOKEN_LENGTH);
        let record = fixture_record(&token, &source_root, "report.txt", 13);
        store.persist_record(&record).unwrap();
        drop(store);

        let reopened = ArtifactStore::new_at(state.path().join("artifacts"));
        let loaded = reopened.load_record(&token).unwrap().unwrap();
        assert_eq!(loaded.token, token);
        assert_eq!(loaded.source_path, "report.txt");
        assert_eq!(loaded.original_bytes, 13);
    }

    #[test]
    fn rejects_malformed_mismatched_and_oversized_records() {
        let state = tempfile::tempdir().unwrap();
        let source_root = state.path().to_string_lossy().into_owned();
        let store = ArtifactStore::new_at(state.path().join("artifacts"));
        store.ensure_layout().unwrap();

        let malformed = "B".repeat(ARTIFACT_TOKEN_LENGTH);
        write_private_test_file(&store.record_path_for_test(&malformed), b"not-json");
        assert!(store.load_record(&malformed).is_err());

        let mismatched = "C".repeat(ARTIFACT_TOKEN_LENGTH);
        let mut record = fixture_record(
            &"D".repeat(ARTIFACT_TOKEN_LENGTH),
            &source_root,
            "report.txt",
            13,
        );
        write_private_test_file(
            &store.record_path_for_test(&mismatched),
            &serde_json::to_vec(&record).unwrap(),
        );
        assert!(store.load_record(&mismatched).is_err());

        record.token = "E".repeat(ARTIFACT_TOKEN_LENGTH);
        let mut padded = serde_json::to_vec(&record).unwrap();
        padded.resize((MAX_RECORD_BYTES + 1) as usize, b' ');
        write_private_test_file(&store.record_path_for_test(&record.token), &padded);
        assert!(store.load_record(&record.token).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_or_non_private_record_state() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let state = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let source_root = state.path().to_string_lossy().into_owned();
        let store = ArtifactStore::new_at(state.path().join("artifacts"));
        store.ensure_layout().unwrap();

        let token = "F".repeat(ARTIFACT_TOKEN_LENGTH);
        let record = fixture_record(&token, &source_root, "report.txt", 13);
        let outside_record = outside.path().join("record.json");
        std::fs::write(&outside_record, serde_json::to_vec(&record).unwrap()).unwrap();
        let record_path = store.record_path_for_test(&token);
        symlink(&outside_record, &record_path).unwrap();
        assert!(store.load_record(&token).is_err());

        std::fs::remove_file(&record_path).unwrap();
        write_private_test_file(&record_path, &serde_json::to_vec(&record).unwrap());
        let records = store.records_dir_for_test();
        std::fs::set_permissions(&records, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(store.load_record(&token).is_err());
    }

    #[test]
    fn publishes_an_immutable_snapshot_with_its_durable_record() {
        let state = tempfile::tempdir().unwrap();
        let source_root = state.path().to_string_lossy().into_owned();
        let store = ArtifactStore::new_at(state.path().join("artifacts"));
        let mut staged = store.stage_snapshot().unwrap();
        staged.writer().write_all(b"abc").unwrap();
        staged.set_byte_count(3);

        let record = store
            .publish_export(
                new_record(&source_root, "report.bin", 3),
                Some(staged),
                1024,
            )
            .unwrap();
        assert!(record.snapshot_stored);
        assert_eq!(
            std::fs::read(
                store
                    .snapshots_dir_for_test()
                    .join(format!("{}.blob", record.token))
            )
            .unwrap(),
            b"abc"
        );
        assert_eq!(
            store.load_record(&record.token).unwrap().unwrap().token,
            record.token
        );
    }

    #[test]
    fn failed_publication_leaves_no_snapshot_or_temporary_file() {
        let state = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new_at(state.path().join("artifacts"));
        store.ensure_layout().unwrap();
        let mut staged = store.stage_snapshot().unwrap();
        staged.writer().write_all(b"abc").unwrap();
        staged.set_byte_count(3);

        assert!(
            store
                .publish_export(
                    new_record(&state.path().to_string_lossy(), "bad\npath", 3,),
                    Some(staged),
                    1024,
                )
                .is_err()
        );
        let entries = std::fs::read_dir(store.snapshots_dir_for_test())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(entries.is_empty());
    }
}
