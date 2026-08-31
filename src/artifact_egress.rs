use std::fmt;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use base64::engine::general_purpose::STANDARD;
use cap_std::{ambient_authority, fs::Dir};
use rmcp::model::{Resource, ResourceContents};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::artifact_store::{
    ARTIFACT_TOKEN_LENGTH, ArtifactStore, NewArtifactRecord, SourceRootIdentity, StoredPayload,
};
use crate::types::ArtifactEgressConfig;

pub const ARTIFACT_RESOURCE_URI_PREFIX: &str = "codexify://artifact/";
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
pub struct RegisteredArtifact {
    pub resource: Resource,
    pub sha256: String,
    pub byte_count: u64,
    pub mime_type: String,
    pub name: String,
    pub snapshot_stored: bool,
    pub fallback_to_source: bool,
}

#[derive(Debug)]
pub struct ArtifactEgressStore {
    config: ArtifactEgressConfig,
    store: ArtifactStore,
}

impl ArtifactEgressStore {
    pub fn new(config: ArtifactEgressConfig) -> Result<Self, ArtifactEgressError> {
        let store = ArtifactStore::for_current_user()
            .map_err(|message| ArtifactEgressError::new("artifact_store_unavailable", message))?;
        Ok(Self { config, store })
    }

    #[doc(hidden)]
    pub fn new_at(config: ArtifactEgressConfig, root: PathBuf) -> Self {
        Self {
            config,
            store: ArtifactStore::new_at(root),
        }
    }

    pub async fn export_project_file(
        &self,
        work_dir: &Path,
        input_path: &str,
        cancellation: &CancellationToken,
    ) -> Result<RegisteredArtifact, ArtifactEgressError> {
        if !self.config.enabled {
            return Err(ArtifactEgressError::new(
                "artifact_egress_disabled",
                "File export is disabled by configuration.",
            ));
        }
        let source = SourcePath::parse(input_path)?;
        let work_dir = work_dir.to_path_buf();
        let config = self.config.clone();
        let store = self.store.clone();
        let cancellation = cancellation.clone();
        tokio::task::spawn_blocking(move || {
            export_project_file_blocking(&store, &config, &work_dir, source, &cancellation)
        })
        .await
        .map_err(|_| {
            ArtifactEgressError::new(
                "source_read_failed",
                "The file-export operation terminated unexpectedly.",
            )
        })?
    }

    pub async fn read_resource(
        &self,
        uri: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<ResourceContents>, ArtifactEgressError> {
        let Some(token) = parse_token(uri) else {
            return Ok(None);
        };
        let store = self.store.clone();
        let token_owned = token.to_string();
        let uri_owned = uri.to_string();
        let max_file_bytes = self.config.max_file_bytes;
        let fallback_to_source = self.config.fallback_to_source;
        let cancellation_owned = cancellation.clone();
        let durable = tokio::task::spawn_blocking(move || {
            let Some(payload) = store
                .resolve_payload(&token_owned)
                .map_err(|message| ArtifactEgressError::new("resource_read_failed", message))?
            else {
                return Ok(None);
            };
            let contents = match payload {
                StoredPayload::Snapshot { file, record } => {
                    let encoded = encode_file_as_blob(file, max_file_bytes, &cancellation_owned)?;
                    if encoded.byte_count == record.original_bytes
                        && encoded.sha256 == record.original_sha256
                    {
                        Some(
                            ResourceContents::blob(encoded.blob, uri_owned.clone())
                                .with_mime_type(record.mime_type),
                        )
                    } else {
                        encode_source_fallback(
                            &record,
                            fallback_to_source,
                            max_file_bytes,
                            &cancellation_owned,
                            &uri_owned,
                        )?
                    }
                }
                StoredPayload::SourceOnly { record } => encode_source_fallback(
                    &record,
                    fallback_to_source,
                    max_file_bytes,
                    &cancellation_owned,
                    &uri_owned,
                )?,
            };
            Ok(Some(contents))
        })
        .await
        .map_err(|_| {
            ArtifactEgressError::new(
                "resource_read_failed",
                "The exported-file resource could not be encoded.",
            )
        })??;
        Ok(durable.flatten())
    }
}

struct EncodedBlob {
    blob: String,
    byte_count: u64,
    sha256: String,
}

fn encode_file_as_blob(
    mut file: std::fs::File,
    max_file_bytes: u64,
    cancellation: &CancellationToken,
) -> Result<EncodedBlob, ArtifactEgressError> {
    if cancellation.is_cancelled() {
        return Err(ArtifactEgressError::new(
            "resource_read_cancelled",
            "Exported-file resource retrieval was cancelled by the MCP client.",
        ));
    }
    let mut encoder = base64::write::EncoderWriter::new(Vec::new(), &STANDARD);
    let mut hasher = Sha256::new();
    let mut byte_count = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if cancellation.is_cancelled() {
            return Err(ArtifactEgressError::new(
                "resource_read_cancelled",
                "Exported-file resource retrieval was cancelled by the MCP client.",
            ));
        }
        let read = file.read(&mut buffer).map_err(|_| {
            ArtifactEgressError::new(
                "resource_read_failed",
                "The exported-file resource could not be read.",
            )
        })?;
        if read == 0 {
            break;
        }
        byte_count = byte_count.saturating_add(read as u64);
        if byte_count > max_file_bytes {
            return Err(ArtifactEgressError::new(
                "artifact_too_large",
                "The exported-file resource exceeds artifactEgress.maxFileBytes.",
            ));
        }
        hasher.update(&buffer[..read]);
        encoder.write_all(&buffer[..read]).map_err(|_| {
            ArtifactEgressError::new(
                "resource_read_failed",
                "The exported-file resource could not be encoded.",
            )
        })?;
    }
    let encoded = encoder.finish().map_err(|_| {
        ArtifactEgressError::new(
            "resource_read_failed",
            "The exported-file resource could not be encoded.",
        )
    })?;
    let blob = String::from_utf8(encoded).map_err(|_| {
        ArtifactEgressError::new(
            "resource_read_failed",
            "The exported-file resource encoding was invalid.",
        )
    })?;
    Ok(EncodedBlob {
        blob,
        byte_count,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

fn encode_source_fallback(
    record: &crate::artifact_store::ArtifactRecord,
    fallback_to_source: bool,
    max_file_bytes: u64,
    cancellation: &CancellationToken,
    uri: &str,
) -> Result<Option<ResourceContents>, ArtifactEgressError> {
    if !fallback_to_source {
        return Ok(None);
    }
    let Some(file) = open_recorded_source(record, max_file_bytes)? else {
        return Ok(None);
    };
    let encoded = encode_file_as_blob(file, max_file_bytes, cancellation)?;
    if encoded.byte_count != record.original_bytes || encoded.sha256 != record.original_sha256 {
        tracing::debug!(
            original_bytes = record.original_bytes,
            current_bytes = encoded.byte_count,
            "artifact source fallback served a newer file version"
        );
    }
    Ok(Some(
        ResourceContents::blob(encoded.blob, uri.to_string())
            .with_mime_type(record.mime_type.clone()),
    ))
}

fn open_recorded_source(
    record: &crate::artifact_store::ArtifactRecord,
    max_file_bytes: u64,
) -> Result<Option<std::fs::File>, ArtifactEgressError> {
    let source = match SourcePath::parse(&record.source_path) {
        Ok(source) => source,
        Err(_) => return Ok(None),
    };
    let stored_root = PathBuf::from(&record.source_root);
    if !stored_root.is_absolute() {
        return Ok(None);
    }
    let canonical_root = match std::fs::canonicalize(&stored_root) {
        Ok(root) if root == stored_root => root,
        Ok(_) => return Ok(None),
        Err(_) => return Ok(None),
    };
    let metadata = match std::fs::metadata(&canonical_root) {
        Ok(metadata) if metadata.is_dir() => metadata,
        Ok(_) | Err(_) => return Ok(None),
    };
    let path_identity = source_root_identity(&metadata);
    if path_identity.as_ref() != record.source_root_identity.as_ref() {
        return Ok(None);
    }
    let directory = match Dir::open_ambient_dir(&canonical_root, ambient_authority()) {
        Ok(directory) => directory,
        Err(_) => return Ok(None),
    };
    let opened_root = match directory.try_clone() {
        Ok(directory) => directory.into_std_file(),
        Err(_) => return Ok(None),
    };
    let opened_metadata = match opened_root.metadata() {
        Ok(metadata) if metadata.is_dir() => metadata,
        Ok(_) | Err(_) => return Ok(None),
    };
    if source_root_identity(&opened_metadata).as_ref() != record.source_root_identity.as_ref() {
        return Ok(None);
    }
    let file = match open_cap_file_no_follow(&directory, &source.relative) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let metadata = match file.metadata() {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) | Err(_) => return Ok(None),
    };
    if metadata.len() > max_file_bytes {
        return Err(ArtifactEgressError::new(
            "artifact_too_large",
            "The current source file exceeds artifactEgress.maxFileBytes.",
        ));
    }
    Ok(Some(file.into_std()))
}

fn export_project_file_blocking(
    store: &ArtifactStore,
    config: &ArtifactEgressConfig,
    work_dir: &Path,
    source: SourcePath,
    cancellation: &CancellationToken,
) -> Result<RegisteredArtifact, ArtifactEgressError> {
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    let opened = open_project_file_for_export(work_dir, &source, config.max_file_bytes)?;
    let snapshot_limit = config.snapshot_max_file_bytes.min(config.max_file_bytes);
    let snapshot_eligible = snapshot_limit > 0
        && config.max_snapshot_bytes > 0
        && opened.declared_size <= snapshot_limit
        && opened.declared_size <= config.max_snapshot_bytes;
    let mut staged = if snapshot_eligible {
        match store.stage_snapshot() {
            Ok(staged) => Some(staged),
            Err(error) => {
                tracing::warn!(%error, "immutable artifact snapshot staging failed; using source fallback");
                None
            }
        }
    } else {
        None
    };

    let mut file = opened.file;
    let mut hasher = Sha256::new();
    let mut byte_count = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let read = file.read(&mut buffer).map_err(|_| {
            ArtifactEgressError::new(
                "source_read_failed",
                format!("The source `{}` could not be read.", source.display),
            )
        })?;
        if read == 0 {
            break;
        }
        byte_count = byte_count.saturating_add(read as u64);
        if byte_count > config.max_file_bytes {
            return Err(ArtifactEgressError::new(
                "artifact_too_large",
                format!(
                    "The source `{}` grew beyond artifactEgress.maxFileBytes while it was read.",
                    source.display
                ),
            ));
        }
        hasher.update(&buffer[..read]);
        if let Some(snapshot) = staged.as_mut()
            && snapshot.writer().write_all(&buffer[..read]).is_err()
        {
            tracing::warn!("immutable artifact snapshot write failed; using source fallback");
            staged = None;
        }
    }
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    if byte_count > snapshot_limit || byte_count > config.max_snapshot_bytes {
        staged = None;
    } else if let Some(snapshot) = staged.as_mut() {
        snapshot.set_byte_count(byte_count);
    }

    let now = unix_time_ms();
    let sha256 = format!("{:x}", hasher.finalize());
    let record = store
        .publish_export(
            NewArtifactRecord {
                source_root: opened.canonical_root,
                source_path: source.display.clone(),
                source_root_identity: opened.root_identity,
                name: source.name.clone(),
                mime_type: mime_type_for_path(&source.relative).to_string(),
                original_bytes: byte_count,
                original_sha256: sha256.clone(),
                created_at_unix_ms: now,
                last_accessed_at_unix_ms: now,
            },
            staged,
            config.max_snapshot_bytes,
        )
        .map_err(|message| ArtifactEgressError::new("artifact_store_failed", message))?;
    let uri = format!("{ARTIFACT_RESOURCE_URI_PREFIX}{}", record.token);
    let resource = Resource::new(uri, record.name.clone())
        .with_title(record.name.clone())
        .with_description("File exported from the active Codexify project")
        .with_mime_type(record.mime_type.clone())
        .with_size(record.original_bytes);
    Ok(RegisteredArtifact {
        resource,
        sha256,
        byte_count: record.original_bytes,
        mime_type: record.mime_type,
        name: record.name,
        snapshot_stored: record.snapshot_stored,
        fallback_to_source: config.fallback_to_source,
    })
}

struct OpenedProjectFile {
    file: std::fs::File,
    declared_size: u64,
    canonical_root: String,
    root_identity: Option<SourceRootIdentity>,
}

fn open_project_file_for_export(
    work_dir: &Path,
    source: &SourcePath,
    max_file_bytes: u64,
) -> Result<OpenedProjectFile, ArtifactEgressError> {
    let root = absolute_root(work_dir)?;
    let canonical_root = std::fs::canonicalize(root).map_err(|_| {
        ArtifactEgressError::new(
            "source_unsafe",
            "The active project root could not be resolved safely.",
        )
    })?;
    let canonical_root_text = canonical_root.to_str().ok_or_else(|| {
        ArtifactEgressError::new(
            "source_unsafe",
            "The active project root cannot be represented in durable artifact metadata.",
        )
    })?;
    let expected_identity =
        source_root_identity(&std::fs::metadata(&canonical_root).map_err(|_| {
            ArtifactEgressError::new(
                "source_unsafe",
                "The active project root could not be inspected safely.",
            )
        })?);
    let directory = Dir::open_ambient_dir(&canonical_root, ambient_authority()).map_err(|_| {
        ArtifactEgressError::new(
            "source_unsafe",
            "The active project root could not be opened safely.",
        )
    })?;
    let opened_root = directory
        .try_clone()
        .map_err(|_| {
            ArtifactEgressError::new(
                "source_unsafe",
                "The active project root could not be revalidated safely.",
            )
        })?
        .into_std_file();
    let opened_identity = source_root_identity(&opened_root.metadata().map_err(|_| {
        ArtifactEgressError::new(
            "source_unsafe",
            "The active project root identity could not be inspected safely.",
        )
    })?);
    if expected_identity != opened_identity {
        return Err(ArtifactEgressError::new(
            "source_unsafe",
            "The active project root changed while the export was opened.",
        ));
    }
    let file = open_cap_file_no_follow(&directory, &source.relative).map_err(|error| {
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
    Ok(OpenedProjectFile {
        file: file.into_std(),
        declared_size: metadata.len(),
        canonical_root: canonical_root_text.to_string(),
        root_identity: opened_identity,
    })
}

fn open_cap_file_no_follow(root: &Dir, path: &Path) -> std::io::Result<cap_std::fs::File> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    root.open_with(path, &options)
}

#[cfg(unix)]
fn source_root_identity(metadata: &std::fs::Metadata) -> Option<SourceRootIdentity> {
    use std::os::unix::fs::MetadataExt;
    Some(SourceRootIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn source_root_identity(_metadata: &std::fs::Metadata) -> Option<SourceRootIdentity> {
    None
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn parse_token(uri: &str) -> Option<&str> {
    let token = uri.strip_prefix(ARTIFACT_RESOURCE_URI_PREFIX)?;
    (token.len() == ARTIFACT_TOKEN_LENGTH
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use std::time::Duration;

    fn config() -> ArtifactEgressConfig {
        ArtifactEgressConfig {
            max_file_bytes: 1024,
            snapshot_max_file_bytes: 1024,
            max_snapshot_bytes: 2048,
            max_references: 2,
            reference_ttl_ms: 60_000,
            ..ArtifactEgressConfig::default()
        }
    }

    fn decode_blob(contents: ResourceContents) -> Vec<u8> {
        match contents {
            ResourceContents::BlobResourceContents { blob, .. } => STANDARD.decode(blob).unwrap(),
            _ => panic!("expected blob resource"),
        }
    }

    #[tokio::test]
    async fn durable_export_survives_store_reconstruction_and_keeps_original_snapshot() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("payload.bin"), [0_u8, 1, 2, 255]).unwrap();

        let store = ArtifactEgressStore::new_at(config(), state.path().join("artifacts"));
        let registered = store
            .export_project_file(project.path(), "payload.bin", &CancellationToken::new())
            .await
            .unwrap();
        assert!(registered.snapshot_stored);
        let uri = registered.resource.uri.clone();

        std::fs::write(project.path().join("payload.bin"), b"replacement").unwrap();
        drop(store);

        let reopened = ArtifactEgressStore::new_at(config(), state.path().join("artifacts"));
        let contents = reopened
            .read_resource(&uri, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decode_blob(contents), [0_u8, 1, 2, 255]);
    }

    #[tokio::test]
    async fn evicted_snapshot_falls_back_to_latest_source_bytes() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut config = config();
        config.max_snapshot_bytes = 8;
        config.snapshot_max_file_bytes = 8;
        std::fs::write(project.path().join("one.bin"), b"11111111").unwrap();
        std::fs::write(project.path().join("two.bin"), b"22222222").unwrap();
        let store = ArtifactEgressStore::new_at(config, state.path().join("artifacts"));

        let one = store
            .export_project_file(project.path(), "one.bin", &CancellationToken::new())
            .await
            .unwrap();
        store
            .export_project_file(project.path(), "two.bin", &CancellationToken::new())
            .await
            .unwrap();
        std::fs::write(project.path().join("one.bin"), b"latest!!").unwrap();

        let contents = store
            .read_resource(&one.resource.uri, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decode_blob(contents), b"latest!!");
    }

    #[tokio::test]
    async fn strict_mode_returns_none_after_snapshot_eviction() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut config = config();
        config.fallback_to_source = false;
        config.max_snapshot_bytes = 1;
        std::fs::write(project.path().join("strict.bin"), b"ab").unwrap();
        let store = ArtifactEgressStore::new_at(config, state.path().join("artifacts"));
        let exported = store
            .export_project_file(project.path(), "strict.bin", &CancellationToken::new())
            .await
            .unwrap();
        assert!(!exported.snapshot_stored);
        assert!(
            store
                .read_resource(&exported.resource.uri, &CancellationToken::new())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn missing_snapshot_and_deleted_source_returns_none() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut config = config();
        config.max_snapshot_bytes = 0;
        std::fs::write(project.path().join("gone.bin"), b"gone").unwrap();
        let store = ArtifactEgressStore::new_at(config, state.path().join("artifacts"));
        let exported = store
            .export_project_file(project.path(), "gone.bin", &CancellationToken::new())
            .await
            .unwrap();
        std::fs::remove_file(project.path().join("gone.bin")).unwrap();
        assert!(
            store
                .read_resource(&exported.resource.uri, &CancellationToken::new())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn file_above_snapshot_threshold_is_source_backed_from_export() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut config = config();
        config.max_file_bytes = 8;
        config.snapshot_max_file_bytes = 3;
        config.max_snapshot_bytes = 1024;
        std::fs::write(project.path().join("live.bin"), b"1234").unwrap();
        let store = ArtifactEgressStore::new_at(config, state.path().join("artifacts"));
        let exported = store
            .export_project_file(project.path(), "live.bin", &CancellationToken::new())
            .await
            .unwrap();
        assert!(!exported.snapshot_stored);
        std::fs::write(project.path().join("live.bin"), b"5678").unwrap();
        let contents = store
            .read_resource(&exported.resource.uri, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decode_blob(contents), b"5678");
    }

    #[tokio::test]
    async fn zero_snapshot_budget_is_source_backed() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut config = config();
        config.max_snapshot_bytes = 0;
        std::fs::write(project.path().join("live.bin"), b"old").unwrap();
        let store = ArtifactEgressStore::new_at(config, state.path().join("artifacts"));
        let exported = store
            .export_project_file(project.path(), "live.bin", &CancellationToken::new())
            .await
            .unwrap();
        assert!(!exported.snapshot_stored);
        std::fs::write(project.path().join("live.bin"), b"new").unwrap();
        let contents = store
            .read_resource(&exported.resource.uri, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decode_blob(contents), b"new");
    }

    #[tokio::test]
    async fn snapshot_read_refreshes_lru_before_next_eviction() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut config = config();
        config.max_file_bytes = 16;
        config.snapshot_max_file_bytes = 4;
        config.max_snapshot_bytes = 8;
        for (name, bytes) in [
            ("one.bin", b"1111"),
            ("two.bin", b"2222"),
            ("three.bin", b"3333"),
        ] {
            std::fs::write(project.path().join(name), bytes).unwrap();
        }
        let store = ArtifactEgressStore::new_at(config, state.path().join("artifacts"));
        let one = store
            .export_project_file(project.path(), "one.bin", &CancellationToken::new())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
        let two = store
            .export_project_file(project.path(), "two.bin", &CancellationToken::new())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert_eq!(
            decode_blob(
                store
                    .read_resource(&one.resource.uri, &CancellationToken::new())
                    .await
                    .unwrap()
                    .unwrap()
            ),
            b"1111"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
        let three = store
            .export_project_file(project.path(), "three.bin", &CancellationToken::new())
            .await
            .unwrap();

        std::fs::write(project.path().join("one.bin"), b"aaaa").unwrap();
        std::fs::write(project.path().join("two.bin"), b"bbbb").unwrap();
        std::fs::write(project.path().join("three.bin"), b"cccc").unwrap();
        assert_eq!(
            decode_blob(
                store
                    .read_resource(&one.resource.uri, &CancellationToken::new())
                    .await
                    .unwrap()
                    .unwrap()
            ),
            b"1111"
        );
        assert_eq!(
            decode_blob(
                store
                    .read_resource(&two.resource.uri, &CancellationToken::new())
                    .await
                    .unwrap()
                    .unwrap()
            ),
            b"bbbb"
        );
        assert_eq!(
            decode_blob(
                store
                    .read_resource(&three.resource.uri, &CancellationToken::new())
                    .await
                    .unwrap()
                    .unwrap()
            ),
            b"3333"
        );
    }

    #[tokio::test]
    async fn two_store_instances_share_one_snapshot_budget() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let artifact_root = state.path().join("artifacts");
        let mut config = config();
        config.max_file_bytes = 16;
        config.snapshot_max_file_bytes = 4;
        config.max_snapshot_bytes = 8;
        for (name, bytes) in [("a.bin", b"aaaa"), ("b.bin", b"bbbb"), ("c.bin", b"cccc")] {
            std::fs::write(project.path().join(name), bytes).unwrap();
        }
        let first = ArtifactEgressStore::new_at(config.clone(), artifact_root.clone());
        let second = ArtifactEgressStore::new_at(config, artifact_root.clone());
        first
            .export_project_file(project.path(), "a.bin", &CancellationToken::new())
            .await
            .unwrap();
        second
            .export_project_file(project.path(), "b.bin", &CancellationToken::new())
            .await
            .unwrap();
        first
            .export_project_file(project.path(), "c.bin", &CancellationToken::new())
            .await
            .unwrap();

        let total: u64 = std::fs::read_dir(artifact_root.join("snapshots"))
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        assert!(total <= 8, "snapshot occupancy was {total} bytes");
    }

    #[tokio::test]
    async fn native_reference_ttl_does_not_expire_durable_resource() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut config = config();
        config.reference_ttl_ms = 1;
        std::fs::write(project.path().join("durable.bin"), b"durable").unwrap();
        let store = ArtifactEgressStore::new_at(config.clone(), state.path().join("artifacts"));
        let exported = store
            .export_project_file(project.path(), "durable.bin", &CancellationToken::new())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        drop(store);

        let reopened = ArtifactEgressStore::new_at(config, state.path().join("artifacts"));
        let contents = reopened
            .read_resource(&exported.resource.uri, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decode_blob(contents), b"durable");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn source_fallback_rejects_symlink_replacement() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut config = config();
        config.max_snapshot_bytes = 0;
        std::fs::write(project.path().join("linked.bin"), b"safe").unwrap();
        std::fs::write(outside.path().join("secret.bin"), b"secret").unwrap();
        let store = ArtifactEgressStore::new_at(config, state.path().join("artifacts"));
        let exported = store
            .export_project_file(project.path(), "linked.bin", &CancellationToken::new())
            .await
            .unwrap();
        std::fs::remove_file(project.path().join("linked.bin")).unwrap();
        symlink(
            outside.path().join("secret.bin"),
            project.path().join("linked.bin"),
        )
        .unwrap();
        assert!(
            store
                .read_resource(&exported.resource.uri, &CancellationToken::new())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn source_root_identity_replacement_fails_closed() {
        let base = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let project = base.path().join("project");
        let moved = base.path().join("project-old");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("report.bin"), b"safe").unwrap();
        let mut config = config();
        config.max_snapshot_bytes = 0;
        let store = ArtifactEgressStore::new_at(config, state.path().join("artifacts"));
        let exported = store
            .export_project_file(&project, "report.bin", &CancellationToken::new())
            .await
            .unwrap();

        std::fs::rename(&project, &moved).unwrap();
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("report.bin"), b"replacement").unwrap();
        assert!(
            store
                .read_resource(&exported.resource.uri, &CancellationToken::new())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_traversal_and_files_over_the_limit() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("large.bin"), vec![0_u8; 1025]).unwrap();
        let cancellation = CancellationToken::new();
        let store = ArtifactEgressStore::new_at(config(), state.path().join("artifacts"));

        let traversal = store
            .export_project_file(project.path(), "../secret", &cancellation)
            .await
            .unwrap_err();
        assert_eq!(traversal.code(), "source_invalid");

        let too_large = store
            .export_project_file(project.path(), "large.bin", &cancellation)
            .await
            .unwrap_err();
        assert_eq!(too_large.code(), "artifact_too_large");

        let control = store
            .export_project_file(project.path(), "bad\nname", &cancellation)
            .await
            .unwrap_err();
        assert_eq!(control.code(), "source_invalid");
    }

    #[tokio::test]
    async fn honors_cancellation_before_export_and_resource_read() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("cancel.bin"), b"cancel").unwrap();
        let store = ArtifactEgressStore::new_at(config(), state.path().join("artifacts"));

        let cancelled_export = CancellationToken::new();
        cancelled_export.cancel();
        let error = store
            .export_project_file(project.path(), "cancel.bin", &cancelled_export)
            .await
            .unwrap_err();
        assert_eq!(error.code(), "file_export_cancelled");

        let uri = store
            .export_project_file(project.path(), "cancel.bin", &CancellationToken::new())
            .await
            .unwrap()
            .resource
            .uri;
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

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.bin"), b"secret").unwrap();
        symlink(
            outside.path().join("secret.bin"),
            project.path().join("linked.bin"),
        )
        .unwrap();

        let store = ArtifactEgressStore::new_at(config(), state.path().join("artifacts"));
        let error = store
            .export_project_file(project.path(), "linked.bin", &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.code(), "source_unsafe");
    }
}
