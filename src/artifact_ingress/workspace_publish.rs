use std::future::Future;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
#[cfg(test)]
use std::time::Duration;

use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use getrandom::getrandom;
use tokio::io::AsyncWriteExt;
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

use super::error::{ArtifactIngressError, ArtifactIngressResult};

const PARTIAL_PREFIX: &str = ".codexify-import-";
const PARTIAL_SUFFIX: &str = ".partial";
const PARTIAL_NAME_ATTEMPTS: usize = 16;
const BLOCKING_ACTIVE: u8 = 0;
const BLOCKING_CANCELLED: u8 = 1;
const BLOCKING_COMMITTED: u8 = 2;

#[derive(Debug, Clone)]
pub struct ArtifactDestination {
    relative_path: PathBuf,
    parent: PathBuf,
    display_path: String,
}

impl ArtifactDestination {
    pub fn parse(value: &str) -> ArtifactIngressResult<Self> {
        if value.is_empty() || value.contains('\0') || value.ends_with('/') || value.ends_with('\\')
        {
            return Err(invalid_destination());
        }

        let path = Path::new(value);
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => {
                    validate_platform_component(part)?;
                    normalized.push(part);
                }
                Component::Prefix(_)
                | Component::RootDir
                | Component::CurDir
                | Component::ParentDir => return Err(invalid_destination()),
            }
        }
        normalized
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or_else(invalid_destination)?;
        let parent = normalized
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf();
        // Report a forward-slash relative path in the receipt so the value ChatGPT
        // receives is identical regardless of the host OS separator. Every retained
        // component is a validated `Normal` part, so joining them is lossless.
        let display_path = normalized
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        Ok(Self {
            relative_path: normalized,
            parent,
            display_path,
        })
    }
}

fn validate_platform_component(component: &std::ffi::OsStr) -> ArtifactIngressResult<()> {
    #[cfg(windows)]
    {
        let value = component.to_string_lossy();
        if value.ends_with(' ')
            || value.ends_with('.')
            || value
                .chars()
                .any(|character| character.is_control() || "<>:\"|?*".contains(character))
        {
            return Err(invalid_destination());
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
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || numbered_device("COM")
            || numbered_device("LPT");
        if reserved {
            return Err(invalid_destination());
        }
    }

    #[cfg(not(windows))]
    let _ = component;

    Ok(())
}

fn invalid_destination() -> ArtifactIngressError {
    ArtifactIngressError::new(
        "destination_invalid",
        "The destination must be a non-empty relative file path inside the active project.",
    )
}

fn deadline_error() -> ArtifactIngressError {
    ArtifactIngressError::new(
        "file_import_timed_out",
        "The native-file import exceeded the configured request timeout.",
    )
}

fn absolute_configured_root(work_dir: &Path) -> ArtifactIngressResult<PathBuf> {
    if work_dir.is_absolute() {
        return Ok(work_dir.to_path_buf());
    }
    std::env::current_dir()
        .map(|current| current.join(work_dir))
        .map_err(|_| {
            ArtifactIngressError::new(
                "destination_unsafe",
                "The active project root could not be made absolute safely.",
            )
        })
}

struct BlockingGate {
    state: AtomicU8,
}

impl BlockingGate {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(BLOCKING_ACTIVE),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == BLOCKING_CANCELLED
    }

    fn cancel(&self) -> bool {
        self.state
            .compare_exchange(
                BLOCKING_ACTIVE,
                BLOCKING_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn commit(&self) -> bool {
        self.state
            .compare_exchange(
                BLOCKING_ACTIVE,
                BLOCKING_COMMITTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

async fn run_blocking_before<T, F>(
    deadline: Instant,
    cancellation: &CancellationToken,
    operation: F,
) -> ArtifactIngressResult<T>
where
    T: Send + 'static,
    F: FnOnce(Arc<BlockingGate>) -> ArtifactIngressResult<T> + Send + 'static,
{
    let gate = Arc::new(BlockingGate::new());
    let worker_gate = Arc::clone(&gate);
    let mut task = tokio::task::spawn_blocking(move || operation(worker_gate));
    tokio::select! {
        biased;
        result = &mut task => match result {
            Ok(result) => result,
            Err(_) => Err(ArtifactIngressError::new(
                "write_failed",
                "A native-file filesystem operation terminated unexpectedly.",
            )),
        },
        _ = cancellation.cancelled() => {
            if gate.cancel() {
                Err(cancellation_error())
            } else {
                task.await.map_err(|_| {
                    ArtifactIngressError::new(
                        "write_failed",
                        "A committed native-file filesystem operation terminated unexpectedly.",
                    )
                })?
            }
        },
        _ = tokio::time::sleep_until(deadline) => {
            if gate.cancel() {
                Err(deadline_error())
            } else {
                task.await.map_err(|_| {
                    ArtifactIngressError::new(
                        "write_failed",
                        "A committed native-file filesystem operation terminated unexpectedly.",
                    )
                })?
            }
        }
    }
}

fn cancellation_error() -> ArtifactIngressError {
    ArtifactIngressError::new(
        "file_import_cancelled",
        "Native file ingress was cancelled by the MCP client.",
    )
}

async fn await_before<T, F>(
    deadline: Instant,
    cancellation: &CancellationToken,
    future: F,
) -> ArtifactIngressResult<T>
where
    F: Future<Output = T>,
{
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(cancellation_error()),
        result = timeout_at(deadline, future) => result.map_err(|_| deadline_error()),
    }
}

pub struct PendingWorkspaceFile {
    configured_root: PathBuf,
    canonical_root: PathBuf,
    root_identity: same_file::Handle,
    parent: Arc<Dir>,
    partial_name: PathBuf,
    partial_identity: Arc<same_file::Handle>,
    destination_relative_path: PathBuf,
    destination_path: String,
    file: Option<tokio::fs::File>,
    published: bool,
}

impl PendingWorkspaceFile {
    pub async fn create_before(
        work_dir: PathBuf,
        destination: ArtifactDestination,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> ArtifactIngressResult<Self> {
        run_blocking_before(deadline, cancellation, move |gate| {
            if gate.is_cancelled() {
                return Err(cancellation_error());
            }
            let pending = Self::create(&work_dir, &destination)?;
            if gate.is_cancelled() {
                drop(pending);
                return Err(cancellation_error());
            }
            Ok(pending)
        })
        .await
    }

    pub fn create(
        work_dir: &Path,
        destination: &ArtifactDestination,
    ) -> ArtifactIngressResult<Self> {
        let configured_root = absolute_configured_root(work_dir)?;
        let canonical_root = std::fs::canonicalize(&configured_root).map_err(|_| {
            ArtifactIngressError::new(
                "destination_unsafe",
                "The active project root could not be resolved safely.",
            )
        })?;
        let root = Arc::new(
            Dir::open_ambient_dir(&canonical_root, ambient_authority()).map_err(|_| {
                ArtifactIngressError::new(
                    "destination_unsafe",
                    "The active project root could not be opened safely.",
                )
            })?,
        );
        let root_identity = same_file::Handle::from_file(
            root.try_clone()
                .map_err(|_| {
                    ArtifactIngressError::new(
                        "destination_unsafe",
                        "The active project root capability could not be cloned safely.",
                    )
                })?
                .into_std_file(),
        )
        .map_err(|_| {
            ArtifactIngressError::new(
                "destination_unsafe",
                "The active project root identity could not be recorded safely.",
            )
        })?;
        if !destination.parent.as_os_str().is_empty() {
            root.create_dir_all(&destination.parent).map_err(|_| {
                ArtifactIngressError::new(
                    "destination_unsafe",
                    "The destination parent could not be created safely inside the active project.",
                )
            })?;
        }
        let parent_path = if destination.parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            destination.parent.as_path()
        };
        let parent = Arc::new(root.open_dir(parent_path).map_err(|_| {
            ArtifactIngressError::new(
                "destination_unsafe",
                "The destination parent is not a safe directory inside the active project.",
            )
        })?);
        match root.symlink_metadata(&destination.relative_path) {
            Ok(_) => {
                return Err(ArtifactIngressError::new(
                    "destination_exists",
                    format!(
                        "The destination `{}` already exists and was not replaced.",
                        destination.display_path
                    ),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(ArtifactIngressError::new(
                    "destination_unsafe",
                    "The destination path could not be inspected safely.",
                ));
            }
        }

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let mut opened = None;
        for _ in 0..PARTIAL_NAME_ATTEMPTS {
            let partial_name = random_partial_name()?;
            match parent.open_with(&partial_name, &options) {
                Ok(file) => {
                    opened = Some((partial_name, file));
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(_) => {
                    return Err(ArtifactIngressError::new(
                        "write_failed",
                        "A private partial file could not be created in the destination directory.",
                    ));
                }
            }
        }
        let (partial_name, file) = opened.ok_or_else(|| {
            ArtifactIngressError::new(
                "write_failed",
                "A unique private partial filename could not be allocated.",
            )
        })?;
        let partial_identity = Arc::new(
            same_file::Handle::from_file(
                file.try_clone()
                    .map_err(|_| {
                        ArtifactIngressError::new(
                            "write_failed",
                            "The private partial file handle could not be cloned safely.",
                        )
                    })?
                    .into_std(),
            )
            .map_err(|_| {
                ArtifactIngressError::new(
                    "write_failed",
                    "The private partial file identity could not be recorded safely.",
                )
            })?,
        );

        Ok(Self {
            configured_root,
            canonical_root,
            root_identity,
            parent,
            partial_name,
            partial_identity,
            destination_relative_path: destination.relative_path.clone(),
            destination_path: destination.display_path.clone(),
            file: Some(tokio::fs::File::from_std(file.into_std())),
            published: false,
        })
    }

    pub async fn write_chunk(&mut self, bytes: &[u8]) -> ArtifactIngressResult<()> {
        let file = self.file.as_mut().ok_or_else(|| {
            ArtifactIngressError::new(
                "write_failed",
                "The pending destination file is unavailable.",
            )
        })?;
        file.write_all(bytes).await.map_err(|_| {
            ArtifactIngressError::new(
                "write_failed",
                "The native file could not be written completely.",
            )
        })
    }

    #[cfg(test)]
    pub async fn publish(self, expected_size: u64) -> ArtifactIngressResult<String> {
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(60))
            .ok_or_else(deadline_error)?;
        self.publish_before(expected_size, deadline, &CancellationToken::new())
            .await
    }

    pub async fn publish_before(
        mut self,
        expected_size: u64,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> ArtifactIngressResult<String> {
        let file = self.file.as_mut().ok_or_else(|| {
            ArtifactIngressError::new(
                "write_failed",
                "The pending destination file is unavailable.",
            )
        })?;
        await_before(deadline, cancellation, file.flush())
            .await?
            .map_err(|_| {
                ArtifactIngressError::new(
                    "write_failed",
                    "The native file could not be flushed before publication.",
                )
            })?;
        await_before(deadline, cancellation, file.sync_all())
            .await?
            .map_err(|_| {
                ArtifactIngressError::new(
                    "write_failed",
                    "The native file could not be synchronized before publication.",
                )
            })?;
        let metadata = await_before(deadline, cancellation, file.metadata())
            .await?
            .map_err(|_| {
                ArtifactIngressError::new(
                    "write_failed",
                    "The completed native file could not be verified before publication.",
                )
            })?;
        if !metadata.is_file() || metadata.len() != expected_size {
            return Err(ArtifactIngressError::new(
                "write_failed",
                "The completed native file failed its pre-publication integrity check.",
            ));
        }
        let verified_clone = await_before(deadline, cancellation, file.try_clone())
            .await?
            .map_err(|_| {
                ArtifactIngressError::new(
                    "write_failed",
                    "The verified native file handle could not be cloned.",
                )
            })?;
        let verified_file = await_before(deadline, cancellation, verified_clone.into_std()).await?;

        run_blocking_before(deadline, cancellation, move |gate| {
            self.publish_blocking(expected_size, verified_file, &gate)
        })
        .await
    }

    fn publish_blocking(
        mut self,
        expected_size: u64,
        verified_file: std::fs::File,
        gate: &BlockingGate,
    ) -> ArtifactIngressResult<String> {
        let verified_identity = same_file::Handle::from_file(verified_file).map_err(|_| {
            ArtifactIngressError::new(
                "write_failed",
                "The verified native file identity could not be recorded.",
            )
        })?;
        if verified_identity != *self.partial_identity {
            return Err(ArtifactIngressError::new(
                "publication_integrity_failed",
                "The verified native file handle no longer matches the original partial file.",
            ));
        }
        if gate.is_cancelled() {
            return Err(cancellation_error());
        }

        let current_canonical = std::fs::canonicalize(&self.configured_root).map_err(|_| {
            ArtifactIngressError::new(
                "destination_unsafe",
                "The active project root moved before atomic publication.",
            )
        })?;
        if current_canonical != self.canonical_root {
            return Err(ArtifactIngressError::new(
                "destination_unsafe",
                "The active project root now resolves to a different path.",
            ));
        }
        let publication_root = Dir::open_ambient_dir(&current_canonical, ambient_authority())
            .map_err(|_| {
                ArtifactIngressError::new(
                    "destination_unsafe",
                    "The active project root moved before atomic publication.",
                )
            })?;
        let current_identity = same_file::Handle::from_file(
            publication_root
                .try_clone()
                .map_err(|_| {
                    ArtifactIngressError::new(
                        "destination_unsafe",
                        "The active project root could not be revalidated before publication.",
                    )
                })?
                .into_std_file(),
        )
        .map_err(|_| {
            ArtifactIngressError::new(
                "destination_unsafe",
                "The active project root identity could not be revalidated before publication.",
            )
        })?;
        if current_identity != self.root_identity {
            return Err(ArtifactIngressError::new(
                "destination_unsafe",
                "The active project root was replaced before atomic publication.",
            ));
        }
        if gate.is_cancelled() {
            return Err(cancellation_error());
        }

        let source_path_identity =
            published_file_identity(&self.parent, &self.partial_name, expected_size).map_err(
                |_| {
                    ArtifactIngressError::new(
                        "publication_integrity_failed",
                        "The private partial path changed before atomic publication.",
                    )
                },
            )?;
        if source_path_identity != verified_identity {
            return Err(ArtifactIngressError::new(
                "publication_integrity_failed",
                "The private partial path no longer names the verified native file.",
            ));
        }

        match self.parent.hard_link(
            &self.partial_name,
            &publication_root,
            &self.destination_relative_path,
        ) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(ArtifactIngressError::new(
                    "destination_exists",
                    format!(
                        "The destination `{}` already exists and was not replaced.",
                        self.destination_path
                    ),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                return Err(ArtifactIngressError::new(
                    "publication_unsupported",
                    "The destination filesystem does not support atomic no-overwrite publication.",
                ));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
            {
                return Err(ArtifactIngressError::new(
                    "destination_unsafe",
                    "The destination directory changed before atomic publication.",
                ));
            }
            Err(_) => {
                return Err(ArtifactIngressError::new(
                    "publication_failed",
                    "The completed native file could not be published atomically.",
                ));
            }
        }

        let published_identity = match published_file_identity(
            &publication_root,
            &self.destination_relative_path,
            expected_size,
        ) {
            Ok(identity) => identity,
            Err(error) => {
                let _ = remove_if_identity_matches(
                    &publication_root,
                    &self.destination_relative_path,
                    &verified_identity,
                );
                return Err(error);
            }
        };
        if published_identity != verified_identity {
            let source_after =
                published_file_identity(&self.parent, &self.partial_name, expected_size).ok();
            if source_after.as_ref() == Some(&published_identity) {
                let _ = remove_if_identity_matches(
                    &publication_root,
                    &self.destination_relative_path,
                    &published_identity,
                );
            }
            return Err(ArtifactIngressError::new(
                "publication_integrity_failed",
                "The published destination did not match the verified native file.",
            ));
        }
        if gate.is_cancelled() {
            let _ = remove_if_identity_matches(
                &publication_root,
                &self.destination_relative_path,
                &published_identity,
            );
            return Err(cancellation_error());
        }

        match remove_if_identity_matches(
            &self.parent,
            &self.partial_name,
            self.partial_identity.as_ref(),
        ) {
            Ok(true) | Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    error_kind = ?error.kind(),
                    "native-file import could not clean its published partial link"
                );
            }
        }
        if !gate.commit() {
            let _ = remove_if_identity_matches(
                &publication_root,
                &self.destination_relative_path,
                &published_identity,
            );
            return Err(cancellation_error());
        }
        self.published = true;
        Ok(self.destination_path.clone())
    }
}

fn published_file_identity(
    root: &Dir,
    path: &Path,
    expected_size: u64,
) -> ArtifactIngressResult<same_file::Handle> {
    let metadata = root.symlink_metadata(path).map_err(|_| {
        ArtifactIngressError::new(
            "publication_integrity_failed",
            "The published destination could not be inspected safely.",
        )
    })?;
    if !metadata.is_file() || metadata.len() != expected_size {
        return Err(ArtifactIngressError::new(
            "publication_integrity_failed",
            "The published destination was not the expected regular file.",
        ));
    }

    let file = open_file_no_follow(root, path).map_err(|_| {
        ArtifactIngressError::new(
            "publication_integrity_failed",
            "The published destination could not be opened without following links.",
        )
    })?;
    let opened = file.metadata().map_err(|_| {
        ArtifactIngressError::new(
            "publication_integrity_failed",
            "The published destination handle could not be inspected.",
        )
    })?;
    if !opened.is_file() || opened.len() != expected_size {
        return Err(ArtifactIngressError::new(
            "publication_integrity_failed",
            "The published destination handle was not the expected regular file.",
        ));
    }
    same_file::Handle::from_file(file.into_std()).map_err(|_| {
        ArtifactIngressError::new(
            "publication_integrity_failed",
            "The published destination identity could not be verified.",
        )
    })
}

fn open_file_no_follow(root: &Dir, path: &Path) -> io::Result<cap_std::fs::File> {
    let mut options = OpenOptions::new();
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

fn remove_if_identity_matches(
    root: &Dir,
    path: &Path,
    expected: &same_file::Handle,
) -> io::Result<bool> {
    let file = match open_file_no_follow(root, path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let current = same_file::Handle::from_file(file.into_std())?;
    if &current != expected {
        return Ok(false);
    }
    root.remove_file(path)?;
    Ok(true)
}

impl Drop for PendingWorkspaceFile {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        self.file.take();
        let parent = Arc::clone(&self.parent);
        let partial_name = self.partial_name.clone();
        let partial_identity = Arc::clone(&self.partial_identity);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            // Cancellation may drop this guard on a Tokio worker; cleanup must
            // not turn a bounded request into an unbounded filesystem wait.
            runtime.spawn_blocking(move || {
                let _ =
                    remove_if_identity_matches(&parent, &partial_name, partial_identity.as_ref());
            });
        } else {
            let _ = remove_if_identity_matches(&parent, &partial_name, partial_identity.as_ref());
        }
    }
}

fn random_partial_name() -> ArtifactIngressResult<PathBuf> {
    let mut random = [0_u8; 16];
    getrandom(&mut random).map_err(|_| {
        ArtifactIngressError::new(
            "write_failed",
            "A secure temporary filename could not be generated.",
        )
    })?;
    let mut hex = String::with_capacity(random.len() * 2);
    for byte in random {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing into a String cannot fail");
    }
    Ok(PathBuf::from(format!(
        "{PARTIAL_PREFIX}{hex}{PARTIAL_SUFFIX}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn partials(path: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(path)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with(PARTIAL_PREFIX) && name.ends_with(PARTIAL_SUFFIX)
                    })
            })
            .collect()
    }

    async fn wait_for_no_partials(path: &Path) {
        for _ in 0..100 {
            if partials(path).is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("partial files remained in {}", path.display());
    }

    #[test]
    fn destination_rejects_ambiguous_or_escaping_paths() {
        for path in [
            "",
            ".",
            "../outside.bin",
            "nested/../outside.bin",
            "/absolute.bin",
            "directory/",
            "nul\0byte",
        ] {
            assert_eq!(
                ArtifactDestination::parse(path).unwrap_err().code(),
                "destination_invalid",
                "{path:?}"
            );
        }
        assert!(ArtifactDestination::parse("fixtures/input.bin").is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn destination_rejects_windows_aliases_and_alternate_streams() {
        for path in [
            "file.txt:stream",
            "CON",
            "aux.txt",
            "COM1.log",
            "LPT9",
            "trailing.",
            "trailing ",
            "bad?.txt",
        ] {
            assert_eq!(
                ArtifactDestination::parse(path).unwrap_err().code(),
                "destination_invalid",
                "{path:?}"
            );
        }
    }

    #[tokio::test]
    async fn publishes_complete_bytes_only_after_finish() {
        let root = tempfile::tempdir().unwrap();
        let destination = ArtifactDestination::parse("nested/file.bin").unwrap();
        let mut pending = PendingWorkspaceFile::create(root.path(), &destination).unwrap();
        pending.write_chunk(b"first").await.unwrap();
        pending.write_chunk(b" second").await.unwrap();

        assert!(!root.path().join("nested/file.bin").exists());
        assert_eq!(partials(&root.path().join("nested")).len(), 1);

        let path = pending.publish(12).await.unwrap();
        assert_eq!(path, "nested/file.bin");
        assert_eq!(
            std::fs::read(root.path().join("nested/file.bin")).unwrap(),
            b"first second"
        );
        wait_for_no_partials(&root.path().join("nested")).await;
    }

    #[tokio::test]
    async fn refuses_to_replace_a_file_or_symlink() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("existing.bin"), b"original").unwrap();
        let destination = ArtifactDestination::parse("existing.bin").unwrap();
        let error = match PendingWorkspaceFile::create(root.path(), &destination) {
            Ok(_) => panic!("existing file should not be opened as a destination"),
            Err(error) => error,
        };
        assert_eq!(error.code(), "destination_exists");
        assert_eq!(
            std::fs::read(root.path().join("existing.bin")).unwrap(),
            b"original"
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("missing-target", root.path().join("link.bin")).unwrap();
            let destination = ArtifactDestination::parse("link.bin").unwrap();
            let error = match PendingWorkspaceFile::create(root.path(), &destination) {
                Ok(_) => panic!("existing symlink should not be opened as a destination"),
                Err(error) => error,
            };
            assert_eq!(error.code(), "destination_exists");
        }
    }

    #[cfg(unix)]
    #[test]
    fn capability_root_rejects_a_parent_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let destination = ArtifactDestination::parse("escape/stolen.bin").unwrap();

        let error = match PendingWorkspaceFile::create(root.path(), &destination) {
            Ok(_) => panic!("symlink escape should not produce a pending file"),
            Err(error) => error,
        };
        assert_eq!(error.code(), "destination_unsafe");
        assert!(!outside.path().join("stolen.bin").exists());
    }

    #[tokio::test]
    async fn drop_removes_an_unpublished_partial() {
        let root = tempfile::tempdir().unwrap();
        let destination = ArtifactDestination::parse("file.bin").unwrap();
        let mut pending = PendingWorkspaceFile::create(root.path(), &destination).unwrap();
        pending.write_chunk(b"partial").await.unwrap();
        assert_eq!(partials(root.path()).len(), 1);
        drop(pending);
        wait_for_no_partials(root.path()).await;
        assert!(!root.path().join("file.bin").exists());
    }

    #[tokio::test]
    async fn concurrent_publication_has_one_atomic_winner() {
        let root = tempfile::tempdir().unwrap();
        let destination = ArtifactDestination::parse("winner.bin").unwrap();
        let mut first = PendingWorkspaceFile::create(root.path(), &destination).unwrap();
        let mut second = PendingWorkspaceFile::create(root.path(), &destination).unwrap();
        first.write_chunk(b"first").await.unwrap();
        second.write_chunk(b"second").await.unwrap();

        first.publish(5).await.unwrap();
        let error = second.publish(6).await.unwrap_err();
        assert_eq!(error.code(), "destination_exists");
        assert_eq!(
            std::fs::read(root.path().join("winner.bin")).unwrap(),
            b"first"
        );
        wait_for_no_partials(root.path()).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacing_the_partial_name_cannot_change_the_published_inode() {
        let root = tempfile::tempdir().unwrap();
        let destination = ArtifactDestination::parse("final.bin").unwrap();
        let mut pending = PendingWorkspaceFile::create(root.path(), &destination).unwrap();
        pending.write_chunk(b"trusted").await.unwrap();

        let partial = root.path().join(&pending.partial_name);
        std::fs::remove_file(&partial).unwrap();
        std::fs::write(&partial, b"hostile").unwrap();

        let error = pending.publish(7).await.unwrap_err();
        assert_eq!(error.code(), "publication_integrity_failed");
        assert!(!root.path().join("final.bin").exists());
        assert_eq!(std::fs::read(&partial).unwrap(), b"hostile");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacing_the_partial_name_with_a_symlink_cannot_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("target.bin"), b"outside").unwrap();
        let destination = ArtifactDestination::parse("final.bin").unwrap();
        let mut pending = PendingWorkspaceFile::create(root.path(), &destination).unwrap();
        pending.write_chunk(b"trusted").await.unwrap();

        let partial = root.path().join(&pending.partial_name);
        std::fs::remove_file(&partial).unwrap();
        std::os::unix::fs::symlink(outside.path().join("target.bin"), &partial).unwrap();

        let error = pending.publish(7).await.unwrap_err();
        assert_eq!(error.code(), "publication_integrity_failed");
        assert!(!root.path().join("final.bin").exists());
        assert_eq!(
            std::fs::read(outside.path().join("target.bin")).unwrap(),
            b"outside"
        );
        assert!(std::fs::symlink_metadata(&partial).unwrap().is_symlink());
    }

    // Unix-only: the scenario renames the parent directory while a partial handle is
    // open inside it. Windows forbids that rename at the OS level (sharing violation),
    // so the redirect this guards against cannot even be set up there.
    #[cfg(unix)]
    #[tokio::test]
    async fn moved_parent_cannot_redirect_publication_outside_the_project() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let destination = ArtifactDestination::parse("nested/final.bin").unwrap();
        let mut pending = PendingWorkspaceFile::create(root.path(), &destination).unwrap();
        pending.write_chunk(b"payload").await.unwrap();

        let moved = outside.path().join("moved");
        std::fs::rename(root.path().join("nested"), &moved).unwrap();
        let error = pending.publish(7).await.unwrap_err();

        assert_eq!(error.code(), "destination_unsafe");
        assert!(!root.path().join("nested/final.bin").exists());
        assert!(!moved.join("final.bin").exists());
        wait_for_no_partials(&moved).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn moved_project_root_cannot_redirect_publication_to_a_replacement_symlink() {
        let container = tempfile::tempdir().unwrap();
        let project = container.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let destination = ArtifactDestination::parse("final.bin").unwrap();
        let mut pending = PendingWorkspaceFile::create(&project, &destination).unwrap();
        pending.write_chunk(b"payload").await.unwrap();

        let moved = outside.path().join("moved-project");
        std::fs::rename(&project, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &project).unwrap();
        let error = pending.publish(7).await.unwrap_err();

        assert_eq!(error.code(), "destination_unsafe");
        assert!(!outside.path().join("final.bin").exists());
        assert!(!moved.join("final.bin").exists());
        wait_for_no_partials(&moved).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retargeting_the_configured_project_symlink_fails_closed() {
        let container = tempfile::tempdir().unwrap();
        let first = container.path().join("first");
        let second = container.path().join("second");
        let configured = container.path().join("project");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        std::os::unix::fs::symlink(&first, &configured).unwrap();
        let destination = ArtifactDestination::parse("final.bin").unwrap();
        let mut pending = PendingWorkspaceFile::create(&configured, &destination).unwrap();
        pending.write_chunk(b"payload").await.unwrap();

        std::fs::remove_file(&configured).unwrap();
        std::os::unix::fs::symlink(&second, &configured).unwrap();
        let error = pending.publish(7).await.unwrap_err();

        assert_eq!(error.code(), "destination_unsafe");
        assert!(!first.join("final.bin").exists());
        assert!(!second.join("final.bin").exists());
        wait_for_no_partials(&first).await;
    }

    #[tokio::test]
    async fn blocking_deadline_cancels_work_before_its_side_effect() {
        use std::sync::{Condvar, Mutex};

        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_gate = Arc::clone(&gate);
        let side_effect = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_side_effect = Arc::clone(&side_effect);
        let deadline = Instant::now() + Duration::from_millis(10);
        let cancellation = CancellationToken::new();

        let result = run_blocking_before(deadline, &cancellation, move |blocking_gate| {
            let (lock, condition) = &*worker_gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = condition.wait(released).unwrap();
            }
            if !blocking_gate.is_cancelled() {
                worker_side_effect.store(true, Ordering::Release);
            }
            Ok(())
        })
        .await;
        assert_eq!(result.unwrap_err().code(), "file_import_timed_out");

        let (lock, condition) = &*gate;
        *lock.lock().unwrap() = true;
        condition.notify_all();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!side_effect.load(Ordering::Acquire));
    }
}
