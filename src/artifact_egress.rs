use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD};
use cap_std::{ambient_authority, fs::Dir};
use getrandom::getrandom;
use rmcp::model::{Resource, ResourceContents};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::types::ArtifactEgressConfig;

pub const ARTIFACT_RESOURCE_URI_PREFIX: &str = "codexify://artifact/";
const TOKEN_BYTES: usize = 32;
const TOKEN_LENGTH: usize = 43;
const TOKEN_ATTEMPTS: usize = 8;
const MAX_SOURCE_PATH_BYTES: usize = 4096;

#[derive(Debug)]
pub struct ArtifactEgressError {
    code: &'static str,
    message: String,
}

impl ArtifactEgressError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for ArtifactEgressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ArtifactEgressError {}

#[derive(Debug)]
pub struct ArtifactSnapshot {
    name: String,
    mime_type: String,
    bytes: Arc<[u8]>,
    sha256: String,
}

impl ArtifactSnapshot {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    pub fn byte_count(&self) -> u64 {
        self.bytes.len() as u64
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

#[derive(Debug)]
pub struct RegisteredArtifact {
    pub resource: Resource,
    pub sha256: String,
    pub byte_count: u64,
    pub mime_type: String,
    pub name: String,
    pub expires_in_ms: u64,
}

#[derive(Debug)]
struct StoredArtifact {
    snapshot: Arc<ArtifactSnapshot>,
    expires_at: Instant,
}

#[derive(Debug, Default)]
struct StoreState {
    entries: HashMap<String, StoredArtifact>,
    order: VecDeque<String>,
    cached_bytes: u64,
}

impl StoreState {
    fn remove(&mut self, token: &str) {
        if let Some(entry) = self.entries.remove(token) {
            self.cached_bytes = self
                .cached_bytes
                .saturating_sub(entry.snapshot.byte_count());
        }
    }

    fn prune_expired(&mut self, now: Instant) {
        while let Some(token) = self.order.front().cloned() {
            match self.entries.get(&token) {
                Some(entry) if entry.expires_at > now => break,
                Some(_) => {
                    self.order.pop_front();
                    self.remove(&token);
                }
                None => {
                    self.order.pop_front();
                }
            }
        }
    }

    fn evict_oldest(&mut self) -> bool {
        while let Some(token) = self.order.pop_front() {
            if self.entries.contains_key(&token) {
                self.remove(&token);
                return true;
            }
        }
        false
    }
}

#[derive(Debug)]
pub struct ArtifactEgressStore {
    config: ArtifactEgressConfig,
    state: Mutex<StoreState>,
}

impl ArtifactEgressStore {
    pub fn new(config: ArtifactEgressConfig) -> Self {
        Self {
            config,
            state: Mutex::new(StoreState::default()),
        }
    }

    pub fn register(
        &self,
        snapshot: ArtifactSnapshot,
    ) -> Result<RegisteredArtifact, ArtifactEgressError> {
        let byte_count = snapshot.byte_count();
        if byte_count > self.config.max_file_bytes || byte_count > self.config.max_cached_bytes {
            return Err(ArtifactEgressError::new(
                "artifact_too_large",
                "The exported file exceeds the configured artifact-egress limit.",
            ));
        }

        let now = Instant::now();
        let expires_at = now
            .checked_add(Duration::from_millis(self.config.reference_ttl_ms))
            .ok_or_else(|| {
                ArtifactEgressError::new(
                    "artifact_egress_invalid",
                    "artifactEgress.referenceTtlMs is too large for this platform.",
                )
            })?;
        let snapshot = Arc::new(snapshot);
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.prune_expired(now);

        let token = (0..TOKEN_ATTEMPTS)
            .find_map(|_| {
                let token = generate_token().ok()?;
                (!state.entries.contains_key(&token)).then_some(token)
            })
            .ok_or_else(|| {
                ArtifactEgressError::new(
                    "artifact_reference_failed",
                    "A unique artifact reference could not be generated.",
                )
            })?;

        while state.entries.len() >= self.config.max_references
            || state.cached_bytes.saturating_add(byte_count) > self.config.max_cached_bytes
        {
            if !state.evict_oldest() {
                return Err(ArtifactEgressError::new(
                    "artifact_cache_full",
                    "The artifact-egress cache could not make room for this file.",
                ));
            }
        }

        let uri = format!("{ARTIFACT_RESOURCE_URI_PREFIX}{token}");

        state.cached_bytes = state.cached_bytes.saturating_add(byte_count);
        state.order.push_back(token.clone());
        state.entries.insert(
            token,
            StoredArtifact {
                snapshot: Arc::clone(&snapshot),
                expires_at,
            },
        );

        let resource = Resource::new(uri, snapshot.name.clone())
            .with_title(snapshot.name.clone())
            .with_description("Immutable file snapshot exported from the active Codexify project")
            .with_mime_type(snapshot.mime_type.clone())
            .with_size(byte_count);

        Ok(RegisteredArtifact {
            resource,
            sha256: snapshot.sha256.clone(),
            byte_count,
            mime_type: snapshot.mime_type.clone(),
            name: snapshot.name.clone(),
            expires_in_ms: self.config.reference_ttl_ms,
        })
    }

    pub async fn read_resource(
        &self,
        uri: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<ResourceContents>, ArtifactEgressError> {
        let Some(token) = parse_token(uri) else {
            return Ok(None);
        };
        let snapshot = {
            let now = Instant::now();
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.prune_expired(now);
            let Some(entry) = state.entries.get(token) else {
                return Ok(None);
            };
            Arc::clone(&entry.snapshot)
        };
        let uri = uri.to_string();
        let mut task = tokio::task::spawn_blocking(move || {
            ResourceContents::blob(STANDARD.encode(snapshot.bytes.as_ref()), uri)
                .with_mime_type(snapshot.mime_type.clone())
        });
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(ArtifactEgressError::new(
                "resource_read_cancelled",
                "Exported-file resource retrieval was cancelled by the MCP client.",
            )),
            result = &mut task => result
                .map(Some)
                .map_err(|_| ArtifactEgressError::new(
                    "resource_read_failed",
                    "The exported-file resource could not be encoded.",
                )),
        }
    }
}

fn generate_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn parse_token(uri: &str) -> Option<&str> {
    let token = uri.strip_prefix(ARTIFACT_RESOURCE_URI_PREFIX)?;
    (token.len() == TOKEN_LENGTH
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'))
    .then_some(token)
}

#[derive(Debug)]
struct SourcePath {
    relative: PathBuf,
    display: String,
    name: String,
}

impl SourcePath {
    fn parse(value: &str) -> Result<Self, ArtifactEgressError> {
        if value.is_empty()
            || value.len() > MAX_SOURCE_PATH_BYTES
            || value.chars().any(char::is_control)
            || value.ends_with('/')
            || value.ends_with('\\')
        {
            return Err(invalid_source());
        }

        let mut relative = PathBuf::new();
        for component in Path::new(value).components() {
            match component {
                Component::Normal(part) => {
                    validate_platform_component(part)?;
                    relative.push(part);
                }
                Component::CurDir => {}
                Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                    return Err(invalid_source());
                }
            }
        }
        let name = relative
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or_else(invalid_source)?
            .to_string_lossy()
            .into_owned();
        let display = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        Ok(Self {
            relative,
            display,
            name,
        })
    }
}

fn validate_platform_component(component: &std::ffi::OsStr) -> Result<(), ArtifactEgressError> {
    #[cfg(windows)]
    {
        let value = component.to_string_lossy();
        if value.ends_with(' ')
            || value.ends_with('.')
            || value
                .chars()
                .any(|character| character.is_control() || "<>:\"|?*".contains(character))
        {
            return Err(invalid_source());
        }
        let stem = value
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        let numbered_device = |prefix| {
            stem.strip_prefix(prefix).is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
        };
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || numbered_device("COM")
            || numbered_device("LPT")
        {
            return Err(invalid_source());
        }
    }

    #[cfg(not(windows))]
    let _ = component;

    Ok(())
}

fn invalid_source() -> ArtifactEgressError {
    ArtifactEgressError::new(
        "source_invalid",
        "The source must be a non-empty relative file path inside the active project.",
    )
}

fn mime_type_for_path(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some(
            "txt" | "md" | "markdown" | "rst" | "log" | "rs" | "c" | "h" | "cc" | "cpp" | "cxx"
            | "hpp" | "hh" | "java" | "kt" | "kts" | "swift" | "go" | "py" | "rb" | "php" | "sh"
            | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd" | "toml" | "ini" | "cfg" | "conf"
            | "diff" | "patch",
        ) => "text/plain",
        Some("csv") => "text/csv",
        Some("tsv") => "text/tab-separated-values",
        Some("html" | "htm") => "text/html",
        Some("css") => "text/css",
        Some("js" | "mjs" | "cjs") => "text/javascript",
        Some("xml") => "application/xml",
        Some("json" | "map") => "application/json",
        Some("yaml" | "yml") => "application/yaml",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("gz" | "tgz") => "application/gzip",
        Some("bz2" | "tbz" | "tbz2") => "application/x-bzip2",
        Some("xz" | "txz") => "application/x-xz",
        Some("tar") => "application/x-tar",
        Some("7z") => "application/x-7z-compressed",
        Some("rar") => "application/vnd.rar",
        Some("wasm") => "application/wasm",
        Some("doc") => "application/msword",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("ppt") => "application/vnd.ms-powerpoint",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("odt") => "application/vnd.oasis.opendocument.text",
        Some("ods") => "application/vnd.oasis.opendocument.spreadsheet",
        Some("odp") => "application/vnd.oasis.opendocument.presentation",
        Some("png") => "image/png",
        Some("jpg" | "jpeg" | "jpe") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("svg" | "svgz") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("tif" | "tiff") => "image/tiff",
        Some("avif") => "image/avif",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("flac") => "audio/flac",
        Some("ogg" | "oga") => "audio/ogg",
        Some("m4a") => "audio/mp4",
        Some("mp4" | "m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        Some("avi") => "video/x-msvideo",
        _ => "application/octet-stream",
    }
}

fn cancelled() -> ArtifactEgressError {
    ArtifactEgressError::new(
        "file_export_cancelled",
        "File export was cancelled by the MCP client.",
    )
}

fn absolute_root(work_dir: &Path) -> Result<PathBuf, ArtifactEgressError> {
    if work_dir.is_absolute() {
        return Ok(work_dir.to_path_buf());
    }
    std::env::current_dir()
        .map(|directory| directory.join(work_dir))
        .map_err(|_| {
            ArtifactEgressError::new(
                "source_unsafe",
                "The active project root could not be made absolute safely.",
            )
        })
}

fn open_project_file(
    work_dir: &Path,
    source: &SourcePath,
    max_file_bytes: u64,
) -> Result<(std::fs::File, u64), ArtifactEgressError> {
    let root = absolute_root(work_dir)?;
    let canonical_root = std::fs::canonicalize(root).map_err(|_| {
        ArtifactEgressError::new(
            "source_unsafe",
            "The active project root could not be resolved safely.",
        )
    })?;
    let directory = Dir::open_ambient_dir(canonical_root, ambient_authority()).map_err(|_| {
        ArtifactEgressError::new(
            "source_unsafe",
            "The active project root could not be opened safely.",
        )
    })?;
    let file = directory.open(&source.relative).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ArtifactEgressError::new(
                "source_not_found",
                format!("File not found: {}", source.display),
            )
        } else {
            ArtifactEgressError::new(
                "source_unsafe",
                format!(
                    "The source `{}` could not be opened safely.",
                    source.display
                ),
            )
        }
    })?;
    let metadata = file.metadata().map_err(|_| {
        ArtifactEgressError::new(
            "source_read_failed",
            format!("The source `{}` could not be inspected.", source.display),
        )
    })?;
    if !metadata.is_file() {
        return Err(ArtifactEgressError::new(
            "source_not_file",
            format!("The source `{}` is not a regular file.", source.display),
        ));
    }
    if metadata.len() > max_file_bytes {
        return Err(ArtifactEgressError::new(
            "artifact_too_large",
            format!(
                "The source `{}` is {} bytes; artifactEgress.maxFileBytes is {}.",
                source.display,
                metadata.len(),
                max_file_bytes
            ),
        ));
    }
    Ok((file.into_std(), metadata.len()))
}

pub async fn snapshot_project_file(
    work_dir: &Path,
    input_path: &str,
    config: &ArtifactEgressConfig,
    cancellation: &CancellationToken,
) -> Result<ArtifactSnapshot, ArtifactEgressError> {
    if !config.enabled {
        return Err(ArtifactEgressError::new(
            "artifact_egress_disabled",
            "File export is disabled by configuration.",
        ));
    }
    let source = SourcePath::parse(input_path)?;
    let source_for_open = SourcePath {
        relative: source.relative.clone(),
        display: source.display.clone(),
        name: source.name.clone(),
    };
    let work_dir = work_dir.to_path_buf();
    let max_file_bytes = config.max_file_bytes;
    let mut open_task = tokio::task::spawn_blocking(move || {
        open_project_file(&work_dir, &source_for_open, max_file_bytes)
    });
    let (file, declared_size) = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(cancelled()),
        result = &mut open_task => result.map_err(|_| {
            ArtifactEgressError::new(
                "source_read_failed",
                "The file-open operation terminated unexpectedly.",
            )
        })??,
    };

    let mut bytes = Vec::with_capacity(
        usize::try_from(declared_size.min(config.max_file_bytes)).unwrap_or_default(),
    );
    let file = tokio::fs::File::from_std(file);
    let mut limited = file.take(config.max_file_bytes.saturating_add(1));
    let read_result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(cancelled()),
        result = limited.read_to_end(&mut bytes) => result,
    };
    read_result.map_err(|_| {
        ArtifactEgressError::new(
            "source_read_failed",
            format!("The source `{}` could not be read.", source.display),
        )
    })?;
    if bytes.len() as u64 > config.max_file_bytes {
        return Err(ArtifactEgressError::new(
            "artifact_too_large",
            format!(
                "The source `{}` grew beyond artifactEgress.maxFileBytes while it was read.",
                source.display
            ),
        ));
    }

    let mut hash_task = tokio::task::spawn_blocking(move || {
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        (bytes, sha256)
    });
    let (bytes, sha256) = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(cancelled()),
        result = &mut hash_task => result.map_err(|_| ArtifactEgressError::new(
            "source_read_failed",
            "The exported file could not be hashed.",
        ))?,
    };
    let mime_type = mime_type_for_path(&source.relative).to_string();
    Ok(ArtifactSnapshot {
        name: source.name,
        mime_type,
        bytes: Arc::from(bytes),
        sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ArtifactEgressConfig {
        ArtifactEgressConfig {
            max_file_bytes: 1024,
            max_cached_bytes: 2048,
            max_references: 2,
            reference_ttl_ms: 60_000,
            ..ArtifactEgressConfig::default()
        }
    }

    #[tokio::test]
    async fn snapshots_exact_bytes_and_serves_an_opaque_resource() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("payload.bin"), [0_u8, 1, 2, 255]).unwrap();
        let snapshot = snapshot_project_file(
            root.path(),
            "payload.bin",
            &config(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(snapshot.byte_count(), 4);
        assert_eq!(snapshot.mime_type(), "application/octet-stream");

        let store = ArtifactEgressStore::new(config());
        let registered = store.register(snapshot).unwrap();
        assert!(
            registered
                .resource
                .uri
                .starts_with(ARTIFACT_RESOURCE_URI_PREFIX)
        );
        assert!(!registered.resource.uri.contains("payload"));
        assert_eq!(registered.resource.name, "payload.bin");
        assert_eq!(registered.resource.size, Some(4));

        std::fs::write(root.path().join("payload.bin"), b"replacement").unwrap();

        let contents = store
            .read_resource(&registered.resource.uri, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        match contents {
            ResourceContents::BlobResourceContents {
                blob, mime_type, ..
            } => {
                assert_eq!(STANDARD.decode(blob).unwrap(), [0_u8, 1, 2, 255]);
                assert_eq!(mime_type.as_deref(), Some("application/octet-stream"));
            }
            _ => panic!("expected blob resource"),
        }
    }

    #[tokio::test]
    async fn rejects_traversal_and_files_over_the_limit() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("large.bin"), vec![0_u8; 1025]).unwrap();
        let cancellation = CancellationToken::new();

        let traversal = snapshot_project_file(root.path(), "../secret", &config(), &cancellation)
            .await
            .unwrap_err();
        assert_eq!(traversal.code(), "source_invalid");

        let too_large = snapshot_project_file(root.path(), "large.bin", &config(), &cancellation)
            .await
            .unwrap_err();
        assert_eq!(too_large.code(), "artifact_too_large");

        let control = snapshot_project_file(root.path(), "bad\nname", &config(), &cancellation)
            .await
            .unwrap_err();
        assert_eq!(control.code(), "source_invalid");
    }

    #[tokio::test]
    async fn evicts_oldest_snapshots_to_stay_within_both_bounds() {
        let root = tempfile::tempdir().unwrap();
        for name in ["one.bin", "two.bin", "three.bin"] {
            std::fs::write(root.path().join(name), vec![name.as_bytes()[0]; 800]).unwrap();
        }
        let store = ArtifactEgressStore::new(config());
        let mut uris = Vec::new();
        for name in ["one.bin", "two.bin", "three.bin"] {
            let snapshot =
                snapshot_project_file(root.path(), name, &config(), &CancellationToken::new())
                    .await
                    .unwrap();
            uris.push(store.register(snapshot).unwrap().resource.uri);
        }
        let cancellation = CancellationToken::new();
        assert!(
            store
                .read_resource(&uris[0], &cancellation)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .read_resource(&uris[1], &cancellation)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .read_resource(&uris[2], &cancellation)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn expires_resource_capabilities() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("short.txt"), b"short").unwrap();
        let mut short_lived = config();
        short_lived.reference_ttl_ms = 1;
        let snapshot = snapshot_project_file(
            root.path(),
            "short.txt",
            &short_lived,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let store = ArtifactEgressStore::new(short_lived);
        let uri = store.register(snapshot).unwrap().resource.uri;
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(
            store
                .read_resource(&uri, &CancellationToken::new())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn honors_cancellation_before_snapshot_and_resource_read() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("cancel.bin"), b"cancel").unwrap();

        let cancelled_snapshot = CancellationToken::new();
        cancelled_snapshot.cancel();
        let error =
            snapshot_project_file(root.path(), "cancel.bin", &config(), &cancelled_snapshot)
                .await
                .unwrap_err();
        assert_eq!(error.code(), "file_export_cancelled");

        let snapshot = snapshot_project_file(
            root.path(),
            "cancel.bin",
            &config(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let store = ArtifactEgressStore::new(config());
        let uri = store.register(snapshot).unwrap().resource.uri;
        let cancelled_read = CancellationToken::new();
        cancelled_read.cancel();
        let error = store
            .read_resource(&uri, &cancelled_read)
            .await
            .unwrap_err();
        assert_eq!(error.code(), "resource_read_cancelled");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn capability_open_rejects_a_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.bin"), b"secret").unwrap();
        symlink(
            outside.path().join("secret.bin"),
            root.path().join("linked.bin"),
        )
        .unwrap();

        let error = snapshot_project_file(
            root.path(),
            "linked.bin",
            &config(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "source_unsafe");
    }
}
