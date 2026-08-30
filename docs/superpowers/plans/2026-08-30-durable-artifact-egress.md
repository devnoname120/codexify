# Durable Artifact Egress Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make native `export_host_file` attachments durable across Codexify restarts, retain immutable snapshots in a global per-user 5 GiB LRU disk store, and fall back to the latest safe source file after snapshot eviction.

**Architecture:** Replace the native in-memory `ArtifactEgressStore` payload cache with a disk-backed `ArtifactStore` rooted at `~/.codexify/artifacts`. Native export becomes one coordinated operation: safely open/hash the project file, optionally stage an immutable snapshot, persist a versioned bearer-capability record, and return the MCP resource link only after the record is durable. `resources/read` resolves the record on demand, serves the retained snapshot when present, otherwise safely reopens the recorded project-relative source path; bridged upstream resource links keep their existing process-local TTL/reference semantics.

**Tech Stack:** Rust 2024, Tokio, rmcp, cap-std, serde/serde_json, sha2, base64, tempfile, standard-library atomic filesystem operations and create-new lock files.

---

## File map

- Create `src/artifact_store.rs`: durable record format, private per-user directories, atomic record writes, snapshot publication, cross-process mutation lock, occupancy scan, LRU eviction, record lookup, snapshot opening, and test-only custom store roots.
- Modify `src/artifact_egress.rs`: project-path validation, root identity capture/validation, bounded streaming hash/snapshot preparation, native export orchestration, snapshot/source-backed `resources/read`, and native egress tests.
- Modify `src/lib.rs`: register the internal durable artifact-store module.
- Modify `src/types.rs`: replace the obsolete native RAM-cache field with durable snapshot policy fields while retaining bridged reference TTL/count fields.
- Modify `src/config.rs`: update config parsing/default/validation tests, including acceptance-and-ignore compatibility for legacy `maxCachedBytes`.
- Modify `src/tools/export_host_file.rs`: call the new durable export operation, remove five-minute expiry output, expose `snapshotStored`/`fallbackToSource`, and update tool wording/tests.
- Modify `src/server.rs`: initialize the process-wide store with the per-user artifact root and update native resource-not-found wording/fixtures.
- Modify `codexify.config.json`: check in the 5 GiB/100 MiB/fallback defaults and remove `maxCachedBytes`.
- Modify `README.md` and `docs/ARCHITECTURE.md`: document durable native resources separately from short-lived bridged resources.

### Task 1: Egress configuration model and backwards-compatible parsing

**Files:**
- Modify: `src/types.rs:275-395`
- Modify: `src/config.rs:2534-2602`
- Test: inline tests in `src/config.rs`

- [ ] **Step 1: Write failing default/override/legacy config tests**

Replace the current artifact-egress config assertions with tests that establish the new contract:

```rust
#[test]
fn artifact_egress_config_accepts_durable_defaults_and_camel_case_overrides() {
    let empty: FileConfig = serde_json::from_str(r#"{"artifactEgress":{}}"#).unwrap();
    let empty = empty.artifact_egress.unwrap();
    assert!(empty.enabled);
    assert_eq!(
        empty.max_file_bytes,
        crate::types::DEFAULT_ARTIFACT_EGRESS_MAX_FILE_BYTES
    );
    assert_eq!(
        empty.snapshot_max_file_bytes,
        crate::types::DEFAULT_ARTIFACT_EGRESS_SNAPSHOT_MAX_FILE_BYTES
    );
    assert_eq!(
        empty.max_snapshot_bytes,
        crate::types::DEFAULT_ARTIFACT_EGRESS_MAX_SNAPSHOT_BYTES
    );
    assert!(empty.fallback_to_source);
    assert_eq!(
        empty.max_references,
        crate::types::DEFAULT_ARTIFACT_EGRESS_MAX_REFERENCES
    );
    assert_eq!(
        empty.reference_ttl_ms,
        crate::types::DEFAULT_ARTIFACT_EGRESS_REFERENCE_TTL_MS
    );

    let configured: FileConfig = serde_json::from_str(
        r#"{
            "artifactEgress": {
                "enabled": false,
                "maxFileBytes": 4096,
                "snapshotMaxFileBytes": 2048,
                "maxSnapshotBytes": 8192,
                "fallbackToSource": false,
                "maxReferences": 4,
                "referenceTtlMs": 5000
            }
        }"#,
    )
    .unwrap();
    let configured = configured.artifact_egress.unwrap();
    assert!(!configured.enabled);
    assert_eq!(configured.max_file_bytes, 4096);
    assert_eq!(configured.snapshot_max_file_bytes, 2048);
    assert_eq!(configured.max_snapshot_bytes, 8192);
    assert!(!configured.fallback_to_source);
    assert_eq!(configured.max_references, 4);
    assert_eq!(configured.reference_ttl_ms, 5000);
}

#[test]
fn artifact_egress_legacy_max_cached_bytes_is_accepted_but_does_not_change_snapshot_defaults() {
    let parsed: FileConfig = serde_json::from_str(
        r#"{"artifactEgress":{"maxCachedBytes":8192}}"#,
    )
    .unwrap();
    let egress = parsed.artifact_egress.unwrap();
    assert_eq!(
        egress.max_snapshot_bytes,
        crate::types::DEFAULT_ARTIFACT_EGRESS_MAX_SNAPSHOT_BYTES
    );
    assert_eq!(
        egress.snapshot_max_file_bytes,
        crate::types::DEFAULT_ARTIFACT_EGRESS_SNAPSHOT_MAX_FILE_BYTES
    );
}
```

Update the unsafe-limit test so `maxFileBytes == 0`, `maxReferences == 0`, and `referenceTtlMs == 0` still fail, while `snapshotMaxFileBytes == 0` and `maxSnapshotBytes == 0` are accepted as explicit ways to force source-backed mode.

- [ ] **Step 2: Run the focused config tests and verify they fail**

Run:

```sh
cargo test artifact_egress_config --lib
```

Expected: FAIL because `snapshot_max_file_bytes`, `max_snapshot_bytes`, and `fallback_to_source` do not exist yet and the old `max_cached_bytes` assertions still describe the previous model.

- [ ] **Step 3: Implement the new configuration fields and defaults**

Change `ArtifactEgressConfig` to:

```rust
pub const DEFAULT_ARTIFACT_EGRESS_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
pub const DEFAULT_ARTIFACT_EGRESS_SNAPSHOT_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
pub const DEFAULT_ARTIFACT_EGRESS_MAX_SNAPSHOT_BYTES: u64 = 5 * 1024 * 1024 * 1024;
pub const DEFAULT_ARTIFACT_EGRESS_MAX_REFERENCES: usize = 64;
pub const DEFAULT_ARTIFACT_EGRESS_REFERENCE_TTL_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ArtifactEgressConfig {
    pub enabled: bool,
    pub max_file_bytes: u64,
    pub snapshot_max_file_bytes: u64,
    pub max_snapshot_bytes: u64,
    pub fallback_to_source: bool,
    pub max_references: usize,
    pub reference_ttl_ms: u64,
}

impl Default for ArtifactEgressConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_file_bytes: DEFAULT_ARTIFACT_EGRESS_MAX_FILE_BYTES,
            snapshot_max_file_bytes: DEFAULT_ARTIFACT_EGRESS_SNAPSHOT_MAX_FILE_BYTES,
            max_snapshot_bytes: DEFAULT_ARTIFACT_EGRESS_MAX_SNAPSHOT_BYTES,
            fallback_to_source: true,
            max_references: DEFAULT_ARTIFACT_EGRESS_MAX_REFERENCES,
            reference_ttl_ms: DEFAULT_ARTIFACT_EGRESS_REFERENCE_TTL_MS,
        }
    }
}
```

Keep validation for the hard native/bridged read limit and bridged reference policy:

```rust
impl ArtifactEgressConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_file_bytes == 0 {
            return Err("artifactEgress.maxFileBytes must be positive".to_string());
        }
        if !(1..=1024).contains(&self.max_references) {
            return Err("artifactEgress.maxReferences must be between 1 and 1024".to_string());
        }
        if self.reference_ttl_ms == 0 {
            return Err("artifactEgress.referenceTtlMs must be positive".to_string());
        }
        Ok(())
    }
}
```

Do not retain `max_cached_bytes` in the struct. Serde's current non-`deny_unknown_fields` behavior deliberately accepts old `maxCachedBytes` keys and ignores them, which is the compatibility behavior covered above.

- [ ] **Step 4: Run the focused config tests and verify they pass**

Run:

```sh
cargo test artifact_egress_config --lib
```

Expected: PASS.

- [ ] **Step 5: Commit the config-model change**

```sh
git add src/types.rs src/config.rs
git commit -m "refactor: define durable artifact egress limits"
```

### Task 2: Durable private artifact records and snapshot storage

**Files:**
- Create: `src/artifact_store.rs`
- Modify: `src/lib.rs`
- Test: inline tests in `src/artifact_store.rs`

- [ ] **Step 1: Write failing durable-store tests**

Create tests around a temporary per-user store root. The core test should prove record persistence survives constructing a replacement store instance:

```rust
#[test]
fn record_survives_store_reconstruction() {
    let state = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new_at(state.path().join("artifacts"));
    let token = "A".repeat(43);
    let source_root = state.path().to_string_lossy().into_owned();
    let record = fixture_record(&token, &source_root, "report.txt", 13);
    store.persist_record(&record).unwrap();
    drop(store);

    let reopened = ArtifactStore::new_at(state.path().join("artifacts"));
    let loaded = reopened.load_record(&token).unwrap().unwrap();
    assert_eq!(loaded.token, token);
    assert_eq!(loaded.source_path, "report.txt");
    assert_eq!(loaded.original_bytes, 13);
}
```

Add these concrete validation tests alongside the restart test:

```rust
#[test]
fn rejects_malformed_mismatched_and_oversized_records() {
    let state = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new_at(state.path().join("artifacts"));
    store.ensure_layout().unwrap();

    let malformed = "B".repeat(43);
    write_private_test_file(&store.record_path_for_test(&malformed), b"not-json");
    assert!(store.load_record(&malformed).is_err());

    let mismatched = "C".repeat(43);
    let source_root = state.path().to_string_lossy().into_owned();
    let mut record = fixture_record(&"D".repeat(43), &source_root, "report.txt", 13);
    write_private_test_file(
        &store.record_path_for_test(&mismatched),
        &serde_json::to_vec(&record).unwrap(),
    );
    assert!(store.load_record(&mismatched).is_err());

    record.token = "E".repeat(43);
    let oversized = serde_json::to_vec(&record).unwrap();
    let mut padded = oversized;
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
    let store = ArtifactStore::new_at(state.path().join("artifacts"));
    store.ensure_layout().unwrap();

    let token = "F".repeat(43);
    let source_root = state.path().to_string_lossy().into_owned();
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
```

`record_path_for_test`, `records_dir_for_test`, `snapshots_dir_for_test`, and `ensure_layout` stay `#[cfg(test)]`/module-private helpers; production callers never receive a filesystem path for a bearer capability. Use these exact signatures:

```rust
#[cfg(test)]
fn record_path_for_test(&self, token: &str) -> PathBuf;

#[cfg(test)]
fn records_dir_for_test(&self) -> PathBuf;

#[cfg(test)]
fn snapshots_dir_for_test(&self) -> PathBuf;

fn ensure_layout(&self) -> Result<(), String>;
```

Define the private-file test helper used above:

```rust
#[cfg(test)]
fn write_private_test_file(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}
```

- [ ] **Step 2: Run the durable-store test and verify it fails**

Run:

```sh
cargo test artifact_store --lib
```

Expected: FAIL because `src/artifact_store.rs` and `ArtifactStore` do not exist.

- [ ] **Step 3: Implement the versioned record and store root**

Create `src/artifact_store.rs` with this externally used shape:

```rust
use std::path::{Path, PathBuf};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

pub const ARTIFACT_RECORD_VERSION: u32 = 1;
pub const ARTIFACT_TOKEN_LENGTH: usize = 43;
const TOKEN_BYTES: usize = 32;
const TOKEN_ATTEMPTS: usize = 8;
const MAX_RECORD_BYTES: u64 = 64 * 1024;

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

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn persist_record(&self, record: &ArtifactRecord) -> Result<(), String>;
}

fn generate_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom::getrandom(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
```

Keep the serialized `Unix` variant available on every target so a store copied between platforms remains parseable. The platform-specific capture helper in `artifact_egress.rs` returns `Some(SourceRootIdentity::Unix { .. })` only on Unix and `None` elsewhere.

Define the record fixture used by the store tests:

```rust
#[cfg(test)]
fn fixture_record(token: &str, source_root: &str, source_path: &str, bytes: u64) -> ArtifactRecord {
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
```

Implement `records/`, `snapshots/`, and `locks/` creation with no-follow checks and Unix modes `0700`; record/temp/snapshot/lock files use `0600`. Create temporary files with a recognizable `.codexify-tmp-` prefix. `ensure_layout` only validates/creates the three private directories; it does not scan the durable store at startup. `persist_record` must serialize pretty JSON, write a private temporary file in `records/`, `sync_all`, and atomically rename it over the record path. `load_record` must enforce `MAX_RECORD_BYTES`, require a regular non-symlink private file, deserialize, require `version == 1`, require the JSON token to equal the filename token, and validate bounded/control-free strings before returning it.

Use the same no-follow primitive already proven by `artifact_ingress::workspace_publish` for every existing record/snapshot file opened from the private store:

```rust
fn open_file_no_follow(root: &cap_std::fs::Dir, path: &Path) -> std::io::Result<cap_std::fs::File> {
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
```

Open the private store subdirectories as `cap_std::fs::Dir` capabilities and use this helper after `symlink_metadata` says the entry is a regular file. Re-check metadata from the opened handle before reading it. Do not implement record/snapshot reads as an `exists()`/`read()` pair that can follow a swapped symlink.

Implement the cross-process mutation lock in this foundation task, because collision-safe token allocation and snapshot publication already require it before LRU eviction exists:

```rust
const LOCK_STALE_MS: u128 = 10 * 60 * 1_000;
const LOCK_TIMEOUT_MS: u128 = 5 * 60 * 1_000;
const LOCK_RETRY_MS: u64 = 50;

struct StoreLock {
    path: PathBuf,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl ArtifactStore {
    fn acquire_store_lock(&self) -> Result<StoreLock, String>;
}
```

`acquire_store_lock` uses a private `locks/store.lock` created with `create_new(true)`, writes PID/timestamp, rejects symlink/non-regular lock paths, removes locks older than `LOCK_STALE_MS`, waits up to `LOCK_TIMEOUT_MS` with `std::thread::sleep(Duration::from_millis(LOCK_RETRY_MS))`, and returns a guard whose `Drop` removes the lock. All callers invoke it from blocking filesystem work, never directly on a Tokio worker thread.

Register the module in `src/lib.rs`:

```rust
mod artifact_store;
```

- [ ] **Step 4: Add snapshot staging/publication primitives**

Expose store operations used by native egress without exposing raw paths to callers:

```rust
pub struct StagedSnapshot {
    temporary: tempfile::NamedTempFile,
    byte_count: u64,
}

impl StagedSnapshot {
    pub(crate) fn writer(&mut self) -> &mut std::fs::File {
        self.temporary.as_file_mut()
    }

    pub(crate) fn set_byte_count(&mut self, byte_count: u64) {
        self.byte_count = byte_count;
    }

    pub(crate) fn byte_count(&self) -> u64 {
        self.byte_count
    }
}

impl ArtifactStore {
    pub fn stage_snapshot(&self) -> Result<StagedSnapshot, String>;

    pub fn publish_export(
        &self,
        record: NewArtifactRecord,
        staged: Option<StagedSnapshot>,
        max_snapshot_bytes: u64,
    ) -> Result<ArtifactRecord, String>;

    pub fn load_record(&self, token: &str) -> Result<Option<ArtifactRecord>, String>;
}
```

`stage_snapshot` creates the private temp file in `snapshots/`. `publish_export` owns bearer-token allocation and the global mutation lock. While holding that lock it makes up to `TOKEN_ATTEMPTS` attempts to generate a fresh 32-byte URL-safe token, rejecting a candidate if either `records/<token>.json` or `snapshots/<token>.blob` already exists. This closes the cross-process collision race rather than checking uniqueness in process memory.

`publish_export` validates `NewArtifactRecord` with the same bounded/control-free/path metadata rules before allocating a token or moving any permanent file. For a staged snapshot, flush and `sync_all` the temporary file before publication; if that durability step fails, discard the staged snapshot and continue with a source-backed record instead of failing the export. Under the global lock, first remove only regular non-symlink `.codexify-tmp-*` files left in `records/`/`snapshots/` by crashed operations, then compute current snapshot occupancy from regular `*.blob` files. In this foundation task, if the new snapshot would exceed `max_snapshot_bytes`, simply discard the staged snapshot and persist a source-backed record. Task 4 upgrades that conservative behavior to LRU eviction. Then construct the versioned `ArtifactRecord`, turn a retained staged file into `snapshots/<token>.blob` with `NamedTempFile::persist_noclobber`, and atomically persist the record before returning it. If durable record publication fails after a snapshot has been moved into place, remove that just-published snapshot before returning the error. `NamedTempFile` cleanup handles abandoned staged snapshots automatically.

Add a concrete cleanup regression:

```rust
#[test]
fn failed_publication_leaves_no_snapshot_or_temporary_file() {
    let state = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new_at(state.path().join("artifacts"));
    store.ensure_layout().unwrap();
    let mut staged = store.stage_snapshot().unwrap();
    use std::io::Write as _;
    staged.writer().write_all(b"abc").unwrap();
    staged.set_byte_count(3);

    let bad = NewArtifactRecord {
        source_root: state.path().to_string_lossy().into_owned(),
        source_path: "bad\npath".to_string(),
        source_root_identity: None,
        name: "bad".to_string(),
        mime_type: "application/octet-stream".to_string(),
        original_bytes: 3,
        original_sha256: "0".repeat(64),
        created_at_unix_ms: 1,
        last_accessed_at_unix_ms: 1,
    };

    assert!(store.publish_export(bad, Some(staged), 1024).is_err());
    let entries = std::fs::read_dir(store.snapshots_dir_for_test())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(entries.is_empty());
}
```

- [ ] **Step 5: Run the durable-store tests**

Run:

```sh
cargo test artifact_store --lib
```

Expected: PASS for record persistence, private-state checks, and snapshot publication primitives.

- [ ] **Step 6: Commit the durable-store foundation**

```sh
git add src/artifact_store.rs src/lib.rs
git commit -m "feat: add durable artifact store"
```

### Task 3: Replace native RAM snapshots with streamed durable exports

**Files:**
- Modify: `src/artifact_egress.rs:1-590`
- Test: inline tests in `src/artifact_egress.rs`

- [ ] **Step 1: Rewrite the primary native egress tests to express restart durability**

Replace the old `snapshot_project_file` + `register` test with a store-root-backed export test:

```rust
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
```

Define the test decoder used by this and the fallback tests:

```rust
fn decode_blob(contents: ResourceContents) -> Vec<u8> {
    match contents {
        ResourceContents::BlobResourceContents { blob, .. } => STANDARD.decode(blob).unwrap(),
        _ => panic!("expected blob resource"),
    }
}
```

Keep and adapt the traversal, oversized-file, cancellation, and Unix symlink-escape tests so they call `export_project_file` directly.

Retain an explicit Windows path-safety regression:

```rust
#[cfg(windows)]
#[tokio::test]
async fn rejects_windows_reserved_source_components() {
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let store = ArtifactEgressStore::new_at(config(), state.path().join("artifacts"));
    for path in ["CON.txt", "AUX.bin", "name."] {
        let error = store
            .export_project_file(project.path(), path, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.code(), "source_invalid");
    }
}
```

- [ ] **Step 2: Run the focused native egress tests and verify they fail**

Run:

```sh
cargo test artifact_egress::tests --lib
```

Expected: FAIL because native egress still uses `Arc<[u8]>`, `ArtifactSnapshot`, `register`, and `snapshot_project_file`.

- [ ] **Step 3: Change `ArtifactEgressStore` to wrap the durable store**

Replace the in-memory `StoreState`/`StoredArtifact` cache with:

```rust
#[derive(Debug)]
pub struct ArtifactEgressStore {
    config: ArtifactEgressConfig,
    store: crate::artifact_store::ArtifactStore,
}

impl ArtifactEgressStore {
    pub fn new(config: ArtifactEgressConfig) -> Result<Self, ArtifactEgressError> {
        let store = crate::artifact_store::ArtifactStore::for_current_user()
            .map_err(|message| ArtifactEgressError::new("artifact_store_unavailable", message))?;
        Ok(Self { config, store })
    }

    #[cfg(test)]
    pub fn new_at(config: ArtifactEgressConfig, root: PathBuf) -> Self {
        Self {
            config,
            store: crate::artifact_store::ArtifactStore::new_at(root),
        }
    }
}
```

Update `RegisteredArtifact` to remove `expires_in_ms` and add:

```rust
pub struct RegisteredArtifact {
    pub resource: Resource,
    pub sha256: String,
    pub byte_count: u64,
    pub mime_type: String,
    pub name: String,
    pub snapshot_stored: bool,
    pub fallback_to_source: bool,
}
```

Delete `ArtifactSnapshot`, `StoredArtifact`, the native `HashMap`/`VecDeque` cache, and native TTL/reference-count eviction code.

Keep `parse_token` in `artifact_egress.rs`, but validate against `crate::artifact_store::ARTIFACT_TOKEN_LENGTH` rather than a second private token-length constant. Token generation itself moves entirely into `ArtifactStore::publish_export`.

- [ ] **Step 4: Capture the canonical source root and optional root identity**

Extend the existing safe open helper so export records the exact canonical root used to create the capability:

```rust
#[derive(Debug, Clone)]
struct OpenedSourceRoot {
    canonical: PathBuf,
    identity: Option<crate::artifact_store::SourceRootIdentity>,
}
```

On Unix, capture stable root identity from `std::os::unix::fs::MetadataExt`:

```rust
#[cfg(unix)]
fn source_root_identity(metadata: &std::fs::Metadata) -> Option<SourceRootIdentity> {
    use std::os::unix::fs::MetadataExt;
    Some(SourceRootIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}
```

Define the non-Unix helper explicitly as well:

```rust
#[cfg(not(unix))]
fn source_root_identity(_metadata: &std::fs::Metadata) -> Option<SourceRootIdentity> {
    None
}
```

Canonical/no-follow path checks remain mandatory on every platform. During fallback reads, if an identity was recorded, re-capture and require equality before opening the relative file.

- [ ] **Step 5: Implement one-pass bounded hashing with optional snapshot staging**

Add an export coordinator with this public signature:

```rust
pub async fn export_project_file(
    &self,
    work_dir: &Path,
    input_path: &str,
    cancellation: &CancellationToken,
) -> Result<RegisteredArtifact, ArtifactEgressError>
```

The blocking streaming worker must:

1. parse `SourcePath`;
2. open the file through `cap_std::fs::Dir` under the canonical root;
3. reject non-regular files and declared size above `max_file_bytes`;
4. stage a snapshot only when declared size is `<= min(snapshot_max_file_bytes, max_file_bytes)` and `<= max_snapshot_bytes`;
5. stream in fixed-size chunks, checking cancellation, updating `Sha256`, enforcing `max_file_bytes` against actual growth, and writing each chunk to the staged snapshot when present;
6. if snapshot temp creation or snapshot writing fails, remove/abandon the staged file but continue hashing the source so the export can degrade to source-backed mode;
7. build `NewArtifactRecord` with original digest/size, canonical root, relative source path, timestamps, and root identity;
8. call `publish_export`, which allocates the collision-safe 256-bit token while holding the global store lock, performs LRU eviction, and durably publishes the record/snapshot;
9. use the returned `ArtifactRecord.token` to build a `Resource` whose URI is `codexify://artifact/<token>` and whose descriptor records the original filename/MIME/size.

Use a blocking chunk loop rather than `read_to_end`; the durable store must never hold the raw payload in an `Arc<[u8]>` or persistent `Vec<u8>`.

- [ ] **Step 6: Run the focused native egress tests**

Run:

```sh
cargo test artifact_egress::tests --lib
```

Expected: PASS for restart durability, immutable retained bytes, traversal/size limits, cancellation, and symlink escape.

- [ ] **Step 7: Commit the streamed durable-export rewrite**

```sh
git add src/artifact_egress.rs
git commit -m "feat: persist native artifact exports"
```

### Task 4: Global LRU eviction and latest-source fallback

**Files:**
- Modify: `src/artifact_store.rs`
- Modify: `src/artifact_egress.rs`
- Test: inline tests in both modules

- [ ] **Step 1: Add failing LRU/source-fallback tests**

Write these failing fallback tests:

```rust
#[tokio::test]
async fn evicted_snapshot_falls_back_to_latest_source_bytes() {
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.max_snapshot_bytes = 8;
    cfg.snapshot_max_file_bytes = 8;

    std::fs::write(project.path().join("one.bin"), b"11111111").unwrap();
    std::fs::write(project.path().join("two.bin"), b"22222222").unwrap();
    let store = ArtifactEgressStore::new_at(cfg, state.path().join("artifacts"));

    let one = store.export_project_file(project.path(), "one.bin", &CancellationToken::new()).await.unwrap();
    let _two = store.export_project_file(project.path(), "two.bin", &CancellationToken::new()).await.unwrap();
    std::fs::write(project.path().join("one.bin"), b"latest!!").unwrap();

    let contents = store.read_resource(&one.resource.uri, &CancellationToken::new()).await.unwrap().unwrap();
    assert_eq!(decode_blob(contents), b"latest!!");
}

#[tokio::test]
async fn strict_mode_returns_none_after_snapshot_eviction() {
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.fallback_to_source = false;
    cfg.max_snapshot_bytes = 1;
    cfg.snapshot_max_file_bytes = 1024;
    std::fs::write(project.path().join("strict.bin"), b"ab").unwrap();
    let store = ArtifactEgressStore::new_at(cfg, state.path().join("artifacts"));
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
    let mut cfg = config();
    cfg.max_snapshot_bytes = 0;
    std::fs::write(project.path().join("gone.bin"), b"gone").unwrap();
    let store = ArtifactEgressStore::new_at(cfg, state.path().join("artifacts"));
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
    let mut cfg = config();
    cfg.max_file_bytes = 8;
    cfg.snapshot_max_file_bytes = 3;
    cfg.max_snapshot_bytes = 1024;
    std::fs::write(project.path().join("live.bin"), b"1234").unwrap();
    let store = ArtifactEgressStore::new_at(cfg, state.path().join("artifacts"));
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
```

Add the deterministic LRU/global-budget/zero-budget tests:

```rust
#[tokio::test]
async fn snapshot_read_refreshes_lru_before_next_eviction() {
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.max_file_bytes = 16;
    cfg.snapshot_max_file_bytes = 4;
    cfg.max_snapshot_bytes = 8;
    for (name, bytes) in [("one.bin", b"1111"), ("two.bin", b"2222"), ("three.bin", b"3333")] {
        std::fs::write(project.path().join(name), bytes).unwrap();
    }
    let store = ArtifactEgressStore::new_at(cfg, state.path().join("artifacts"));
    let one = store.export_project_file(project.path(), "one.bin", &CancellationToken::new()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2)).await;
    let two = store.export_project_file(project.path(), "two.bin", &CancellationToken::new()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2)).await;
    assert_eq!(
        decode_blob(store.read_resource(&one.resource.uri, &CancellationToken::new()).await.unwrap().unwrap()),
        b"1111"
    );
    tokio::time::sleep(Duration::from_millis(2)).await;
    let three = store.export_project_file(project.path(), "three.bin", &CancellationToken::new()).await.unwrap();

    std::fs::write(project.path().join("one.bin"), b"aaaa").unwrap();
    std::fs::write(project.path().join("two.bin"), b"bbbb").unwrap();
    std::fs::write(project.path().join("three.bin"), b"cccc").unwrap();
    assert_eq!(decode_blob(store.read_resource(&one.resource.uri, &CancellationToken::new()).await.unwrap().unwrap()), b"1111");
    assert_eq!(decode_blob(store.read_resource(&two.resource.uri, &CancellationToken::new()).await.unwrap().unwrap()), b"bbbb");
    assert_eq!(decode_blob(store.read_resource(&three.resource.uri, &CancellationToken::new()).await.unwrap().unwrap()), b"3333");
}

#[tokio::test]
async fn two_store_instances_share_one_snapshot_budget() {
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let artifact_root = state.path().join("artifacts");
    let mut cfg = config();
    cfg.max_file_bytes = 16;
    cfg.snapshot_max_file_bytes = 4;
    cfg.max_snapshot_bytes = 8;
    for (name, bytes) in [("a.bin", b"aaaa"), ("b.bin", b"bbbb"), ("c.bin", b"cccc")] {
        std::fs::write(project.path().join(name), bytes).unwrap();
    }
    let first = ArtifactEgressStore::new_at(cfg.clone(), artifact_root.clone());
    let second = ArtifactEgressStore::new_at(cfg, artifact_root.clone());
    first.export_project_file(project.path(), "a.bin", &CancellationToken::new()).await.unwrap();
    second.export_project_file(project.path(), "b.bin", &CancellationToken::new()).await.unwrap();
    first.export_project_file(project.path(), "c.bin", &CancellationToken::new()).await.unwrap();

    let total: u64 = std::fs::read_dir(artifact_root.join("snapshots"))
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    assert!(total <= 8, "snapshot occupancy was {total} bytes");
}

#[tokio::test]
async fn zero_snapshot_budget_is_source_backed() {
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.max_snapshot_bytes = 0;
    std::fs::write(project.path().join("live.bin"), b"old").unwrap();
    let store = ArtifactEgressStore::new_at(cfg, state.path().join("artifacts"));
    let exported = store.export_project_file(project.path(), "live.bin", &CancellationToken::new()).await.unwrap();
    assert!(!exported.snapshot_stored);
    std::fs::write(project.path().join("live.bin"), b"new").unwrap();
    let contents = store.read_resource(&exported.resource.uri, &CancellationToken::new()).await.unwrap().unwrap();
    assert_eq!(decode_blob(contents), b"new");
}

#[tokio::test]
async fn native_reference_ttl_does_not_expire_durable_resource() {
    let project = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let artifact_root = state.path().join("artifacts");
    let mut cfg = config();
    cfg.reference_ttl_ms = 1;
    std::fs::write(project.path().join("durable.bin"), b"durable").unwrap();
    let reopened_cfg = cfg.clone();
    let store = ArtifactEgressStore::new_at(cfg, artifact_root.clone());
    let exported = store.export_project_file(project.path(), "durable.bin", &CancellationToken::new()).await.unwrap();
    let uri = exported.resource.uri;
    tokio::time::sleep(Duration::from_millis(5)).await;
    drop(store);

    let reopened = ArtifactEgressStore::new_at(reopened_cfg, artifact_root);
    let contents = reopened.read_resource(&uri, &CancellationToken::new()).await.unwrap().unwrap();
    assert_eq!(decode_blob(contents), b"durable");
}

#[cfg(unix)]
#[tokio::test]
async fn source_root_identity_replacement_fails_closed() {
    let parent = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let project = parent.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("file.bin"), b"original").unwrap();
    let mut cfg = config();
    cfg.max_snapshot_bytes = 0;
    let store = ArtifactEgressStore::new_at(cfg, state.path().join("artifacts"));
    let exported = store.export_project_file(&project, "file.bin", &CancellationToken::new()).await.unwrap();

    std::fs::rename(&project, parent.path().join("old-project")).unwrap();
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("file.bin"), b"replacement").unwrap();
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
async fn source_fallback_rejects_symlink_replacement() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("file.bin"), b"original").unwrap();
    std::fs::write(outside.path().join("secret.bin"), b"secret").unwrap();
    let mut cfg = config();
    cfg.max_snapshot_bytes = 0;
    let store = ArtifactEgressStore::new_at(cfg, state.path().join("artifacts"));
    let exported = store.export_project_file(project.path(), "file.bin", &CancellationToken::new()).await.unwrap();
    std::fs::remove_file(project.path().join("file.bin")).unwrap();
    symlink(outside.path().join("secret.bin"), project.path().join("file.bin")).unwrap();

    assert!(
        store
            .read_resource(&exported.resource.uri, &CancellationToken::new())
            .await
            .unwrap()
            .is_none()
    );
}
```

- [ ] **Step 2: Run the focused tests and verify they fail**

Run:

```sh
cargo test artifact_egress::tests --lib
cargo test artifact_store --lib
```

Expected: FAIL because budget-aware LRU and source fallback are not implemented yet.

- [ ] **Step 3: Extend the existing mutation lock to cover LRU/read-recency changes**

Reuse the `StoreLock` implemented in Task 2. Every occupancy scan, snapshot eviction, `snapshotStored` correction, and `lastAccessedAtUnixMs` update must happen while that same global lock is held. Do not introduce an in-process mutex or a second lock file: two Codexify processes must serialize through the one `locks/store.lock` capability-store lock established earlier.

- [ ] **Step 4: Implement occupancy scanning and true LRU eviction**

While holding the global lock, scan `snapshots/*.blob`, reject/non-follow non-regular entries, and pair valid snapshots with their records. A regular snapshot with no valid matching record is an orphan: unlink it while holding the lock because no capability can resolve it. Compute occupancy from actual file metadata rather than a cached aggregate. Sort remaining eviction candidates by `record.last_accessed_at_unix_ms`, then `record.created_at_unix_ms`, then token for deterministic ties.

Before publishing a staged snapshot of `new_bytes`, repeatedly remove the least-recently-used snapshot until:

```rust
current_snapshot_bytes.saturating_add(new_bytes) <= max_snapshot_bytes
```

On successful unlink, atomically update that record to `snapshot_stored = false`. If unlink fails because the snapshot is actively open, skip that candidate and try the next one; if no removable candidate can satisfy the budget, degrade the new export to source-backed mode rather than failing the export.

- [ ] **Step 5: Add snapshot-open/access methods**

Add:

```rust
pub enum StoredPayload {
    Snapshot { file: std::fs::File, record: ArtifactRecord },
    SourceOnly { record: ArtifactRecord },
}

impl ArtifactStore {
    pub fn resolve_payload(&self, token: &str) -> Result<Option<StoredPayload>, String>;
}
```

While holding the lock, load the record and attempt to open `snapshots/<token>.blob` as a regular non-symlink file. On a snapshot hit, update `last_accessed_at_unix_ms` atomically before releasing the lock and return the already-open file handle. Treat an LRU timestamp write failure as best-effort bookkeeping: log a sanitized warning and still serve the already-open immutable snapshot. On a snapshot miss, normalize stale `snapshot_stored = true` to false when that record write succeeds and return `SourceOnly` either way.

In `ArtifactEgressStore::read_resource`, clone the `ArtifactStore`, token, and cancellation state into `tokio::task::spawn_blocking`, call synchronous `resolve_payload` there, then perform bounded file encoding in the same blocking task. This keeps all durable-store filesystem locking and large-file reads off Tokio worker threads.

- [ ] **Step 6: Implement bounded blob encoding from a file handle**

In `src/artifact_egress.rs`, replace the old `STANDARD.encode(snapshot.bytes.as_ref())` path with a chunked blocking encoder that checks cancellation and the hard per-read size limit. Hash the bytes in the same pass so source-backed reads can diagnose when the current file differs from the original export without a second read:

```rust
struct EncodedBlob {
    blob: String,
    byte_count: u64,
    sha256: String,
}

fn encode_file_as_blob(
    mut file: std::fs::File,
    max_file_bytes: u64,
    cancellation: CancellationToken,
) -> Result<EncodedBlob, ArtifactEgressError>
```

Use `base64::write::EncoderWriter` over a `Vec<u8>` for the encoded output, read the source in fixed chunks, update `Sha256`, count actual bytes, stop with `artifact_too_large` at `max_file_bytes + 1`, and check `cancellation.is_cancelled()` between chunks. `String::from_utf8(encoded_bytes)` consumes the encoded `Vec<u8>` rather than retaining a second application-managed copy.

- [ ] **Step 7: Implement safe source fallback**

On `StoredPayload::SourceOnly`:

1. return `None` immediately if `fallback_to_source == false`;
2. parse `record.source_path` with the existing `SourcePath::parse`;
3. require the stored `source_root` to be absolute and canonicalize the current directory at that path;
4. require canonicalized path text to equal the stored canonical root;
5. when `source_root_identity` is present, require the current root identity to match it;
6. open the relative path through `cap_std::fs::Dir` and require a regular file;
7. encode through the same bounded/cancellable file encoder;
8. keep the record's filename/MIME metadata, even if current bytes/digest differ.

A missing/unsafe/replaced source returns `Ok(None)` for a valid capability rather than an internal server error; cancellation and actual I/O/encoding failures remain explicit `ArtifactEgressError`s. On a successful fallback, compare `EncodedBlob.sha256`/`byte_count` with `record.original_sha256`/`original_bytes` and emit only a sanitized debug/audit indication that the current source differs; never overwrite the original digest stored in the durable record and never log the absolute source root.

- [ ] **Step 8: Run LRU/fallback tests**

Run:

```sh
cargo test artifact_egress::tests --lib
cargo test artifact_store --lib
```

Expected: PASS, including shared-root multiple-store budget enforcement and LRU access ordering.

- [ ] **Step 9: Commit LRU and source fallback**

```sh
git add src/artifact_store.rs src/artifact_egress.rs
git commit -m "feat: add artifact LRU and source fallback"
```

### Task 5: Wire the durable store through the tool/server contract

**Files:**
- Modify: `src/tools/export_host_file.rs:23-214`
- Modify: `src/server.rs:263-303,575-704` and test fixtures constructing `ArtifactEgressStore`
- Modify: `src/tools/import_host_file.rs` test fixture only
- Modify: `src/tools/setup.rs` test fixture only
- Test: inline tests in `src/tools/export_host_file.rs`, `src/server.rs`, `src/bridge.rs`

- [ ] **Step 1: Write failing tool-result contract assertions**

Update the export tool test to require durable status and absence of expiry:

```rust
let structured = result.structured_content.unwrap();
assert_eq!(structured["path"], "report.txt");
assert_eq!(structured["name"], "report.txt");
assert_eq!(structured["bytes"], 13);
assert_eq!(structured["mimeType"], "text/plain");
assert_eq!(structured["snapshotStored"], true);
assert_eq!(structured["fallbackToSource"], true);
assert!(structured.get("expiresInMs").is_none());
assert!(structured.get("resourceUri").is_none());
assert!(!result.joined_text().contains("available for"));
```

Change the test setup to create a second `TempDir` and construct the store exactly as follows so unit tests never write to the real `~/.codexify` directory:

```rust
let state = tempfile::tempdir().unwrap();
let store = ArtifactEgressStore::new_at(
    config.artifact_egress.clone(),
    state.path().join("artifacts"),
);
```

- [ ] **Step 2: Run the export tool test and verify it fails**

Run:

```sh
cargo test export_host_file --lib
```

Expected: FAIL because the tool still advertises `expiresInMs` and calls the removed snapshot/register pipeline.

- [ ] **Step 3: Update `ExportHostFile::run` and schema**

Replace the two-step call with:

```rust
let registered = match store
    .export_project_file(&config.work_dir, &path, cancellation)
    .await
{
    Ok(registered) => registered,
    Err(error) => return ToolResult::error(error.to_string()),
};
```

Return text that distinguishes the two modes without absolute paths:

```rust
let durability = if registered.snapshot_stored {
    "An immutable snapshot is retained in Codexify's durable artifact store."
} else if registered.fallback_to_source {
    "No immutable snapshot was retained; the resource resolves to the latest safe version at the recorded project path."
} else {
    "No immutable snapshot was retained and source fallback is disabled."
};
let text = format!(
    "Exported `{path}` as `{}` ({} bytes, SHA-256 {}). {durability}",
    registered.name, registered.byte_count, registered.sha256,
);
```

The output schema becomes exactly:

```rust
Some(json!({
    "type": "object",
    "properties": {
        "path": { "type": "string" },
        "name": { "type": "string" },
        "bytes": { "type": "integer", "minimum": 0 },
        "sha256": { "type": "string", "pattern": "^[0-9a-f]{64}$" },
        "mimeType": { "type": "string" },
        "snapshotStored": { "type": "boolean" },
        "fallbackToSource": { "type": "boolean" }
    },
    "required": [
        "path", "name", "bytes", "sha256", "mimeType",
        "snapshotStored", "fallbackToSource"
    ],
    "additionalProperties": false
}))
```

Update the description/behavior reason to say the tool creates private durable return-resource bookkeeping rather than a short-lived RAM reference.

- [ ] **Step 4: Make server startup initialize the user-global store fallibly**

Because `ArtifactEgressStore::new` now returns `Result`, initialize it before the config enters its `Arc`:

```rust
let artifact_egress = Arc::new(
    ArtifactEgressStore::new(config.artifact_egress.clone())
        .map_err(|error| anyhow::anyhow!("initialize artifact egress: {error}"))?,
);
```

Update test-only context fixtures to use `new_at` with temp state roots where they actually exercise native export; fixtures that never touch egress can construct a temporary store root local to the test helper. Do not let unit tests create persistent user artifacts.

Change the native miss message from:

```rust
"Unknown or expired exported-file resource"
```

to:

```rust
"Unknown or unavailable exported-file resource"
```

because native capabilities no longer expire on the bridged-resource TTL.

- [ ] **Step 5: Prove bridged upstream TTL behavior is unchanged**

Run:

```sh
cargo test bridged_resource_capabilities_expire_and_reads_are_cancellable --lib
```

Expected: PASS. `BridgedResourceStore` must continue to use `max_references` and `reference_ttl_ms` exactly as before and must not use `ArtifactStore`.

- [ ] **Step 6: Run tool/server focused tests**

Run:

```sh
cargo test export_host_file --lib
cargo test server::tests --lib
cargo test bridge::tests --lib
```

Expected: PASS.

- [ ] **Step 7: Commit the public contract wiring**

```sh
git add src/tools/export_host_file.rs src/server.rs src/tools/import_host_file.rs src/tools/setup.rs
git commit -m "feat: expose durable artifact resources"
```

### Task 6: Default config and documentation migration

**Files:**
- Modify: `codexify.config.json:68-74`
- Modify: `README.md:411-414,582-588,820-828,926-946,1422-1423`
- Modify: `docs/ARCHITECTURE.md:90-96,117-122,474-480,851-856,1013-1014`
- Test: documentation/config consistency tests already present in the repository

- [ ] **Step 1: Update the checked-in default config**

Use this block:

```json
"artifactEgress": {
  "enabled": true,
  "maxFileBytes": 104857600,
  "snapshotMaxFileBytes": 104857600,
  "maxSnapshotBytes": 5368709120,
  "fallbackToSource": true,
  "maxReferences": 64,
  "referenceTtlMs": 300000
}
```

Remove `maxCachedBytes` entirely.

- [ ] **Step 2: Rewrite README native-egress semantics**

Document these exact distinctions:

```text
Native export:
- durable record under ~/.codexify/artifacts/records
- immutable snapshot under ~/.codexify/artifacts/snapshots when eligible
- 5 GiB global per-user LRU snapshot budget by default
- 100 MiB snapshot eligibility threshold by default
- no five-minute native capability expiry
- retained snapshot serves original bytes
- evicted/unstored snapshot serves latest safe source when fallbackToSource=true
- records are not resources/list enumerable

Bridged upstream resource links:
- remain process-local
- maxReferences still applies
- referenceTtlMs is still 5 minutes by default
```

Update the tool table wording from "short-lived" to durable, remove the in-memory cache security bullet, and add the new bounded-state directory to the outside-work-directory state documentation.

- [ ] **Step 3: Update architecture documentation and troubleshooting**

Replace statements that `resources/read` resolves "local snapshots" from process memory with the durable disk resolution order. Troubleshooting for a native artifact should say it becomes unavailable only when the durable record is missing/invalid or both retained snapshot and permitted source fallback are unavailable; restarting Codexify is no longer a reason to re-export.

Keep bridged upstream troubleshooting explicit about TTL/process lifetime.

- [ ] **Step 4: Run formatting/config/doc tests**

Run:

```sh
cargo fmt --check
cargo test skills_and_docs --test skills_and_docs
cargo test meta_suite --test meta_suite
```

Expected: PASS.

- [ ] **Step 5: Commit docs/default config**

```sh
git add codexify.config.json README.md docs/ARCHITECTURE.md
git commit -m "docs: document durable artifact retention"
```

### Task 7: Security/regression matrix and final verification

**Files:**
- Modify only if a failing regression test exposes a defect in the files above.
- Test: full Rust test suite and static checks.

- [ ] **Step 1: Verify the named security regression tests from the earlier tasks are present**

Run this source-level check; every name must be found before final verification:

```sh
rg -n 'fn (record_survives_store_reconstruction|durable_export_survives_store_reconstruction_and_keeps_original_snapshot|evicted_snapshot_falls_back_to_latest_source_bytes|strict_mode_returns_none_after_snapshot_eviction|missing_snapshot_and_deleted_source_returns_none|file_above_snapshot_threshold_is_source_backed_from_export|zero_snapshot_budget_is_source_backed|snapshot_read_refreshes_lru_before_next_eviction|two_store_instances_share_one_snapshot_budget|rejects_malformed_mismatched_and_oversized_records|rejects_symlinked_or_non_private_record_state|failed_publication_leaves_no_snapshot_or_temporary_file|native_reference_ttl_does_not_expire_durable_resource|source_root_identity_replacement_fails_closed|source_fallback_rejects_symlink_replacement|rejects_windows_reserved_source_components|bridged_resource_capabilities_expire_and_reads_are_cancellable)' src
```

Expected: one definition for every listed regression test. These tests collectively cover:

```text
- durable record survives store reconstruction
- immutable retained snapshot ignores later source edits
- LRU eviction preserves record
- evicted snapshot serves latest source by default
- fallback disabled returns not found
- deleted source plus missing snapshot returns not found
- over-threshold file starts source-backed
- zero/too-small snapshot budget degrades to source-backed
- LRU access updates recency
- shared store instances enforce one global byte budget
- malformed record/token mismatch fails closed
- failed publication cleans temporary/staged files
- source path symlink escape fails closed
- Unix source-root replacement (dev/inode mismatch) fails closed
- Windows reserved/path-component validation remains covered
- export/read cancellation remains effective
- native resource no longer expires after referenceTtlMs
- bridged upstream resource still expires after referenceTtlMs
- no unit test writes artifacts to the real user store
```

Then verify test code does not accidentally call the real per-user constructor:

```sh
rg -n 'ArtifactEgressStore::new\(' src -g '*.rs'
```

Expected: the production startup construction in `src/server.rs` is present; all unit-test fixtures use `ArtifactEgressStore::new_at` with a temporary artifact root.

The `native_reference_ttl_does_not_expire_durable_resource` test must set `reference_ttl_ms = 1`, export a native file, sleep at least 5 ms, reconstruct the store, and assert the native resource still reads successfully. The `source_root_identity_replacement_fails_closed` test is `#[cfg(unix)]`: export source-backed content, rename the original project root away, create a different directory at the same pathname with the same relative filename, and assert the old capability does not expose the replacement file because the recorded device/inode pair no longer matches.

- [ ] **Step 2: Run formatter**

```sh
cargo fmt --all
cargo fmt --all --check
```

Expected: both commands exit 0 and the second produces no diff.

- [ ] **Step 3: Run Clippy with warnings denied**

```sh
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: exit 0 with no warnings.

- [ ] **Step 4: Run the full test suite**

```sh
cargo test --all-targets --all-features
```

Expected: all tests pass.

- [ ] **Step 5: Inspect the aggregate project diff**

Call the connector `show_diff` once after the final related code/document change. Review the whole durable-artifact patch for accidental changes, leaked absolute test paths, stale `maxCachedBytes` documentation, stale native `expiresInMs`, and unintended modifications to bridged-resource TTL behavior.

- [ ] **Step 6: Commit any final test-driven corrections**

If final verification required code corrections, commit only those verified corrections:

```sh
git add src tests README.md docs codexify.config.json
git commit -m "test: harden durable artifact egress"
```

If no corrections were necessary, do not create an empty commit.

- [ ] **Step 7: Report completion evidence**

Report the exact passing commands, the durable store path/default limits, the source-fallback behavior, and the commit range. Do not claim the feature complete until formatter, Clippy, full tests, and final diff inspection have all succeeded.
