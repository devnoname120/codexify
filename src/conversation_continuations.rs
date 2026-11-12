use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::project_bindings::ConversationIdentity;
use crate::types::AppConfig;

const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 1024 * 1024;
const MAX_RECORDS: usize = 10_000;

pub(crate) const RETIRED_MESSAGE: &str = "This Codexify task continued in another ChatGPT conversation. This conversation is now read-only.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConversationOwnership {
    Active { task: ConversationIdentity },
    Retired { task: ConversationIdentity },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaimedContinuation {
    pub task: ConversationIdentity,
    pub workspace: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskOwnerSnapshot {
    pub task: ConversationIdentity,
    pub owner_key: String,
    pub generation: u64,
    pub active: bool,
}

impl TaskOwnerSnapshot {
    #[cfg(test)]
    fn with_active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }
}

pub(crate) struct ModelCallGuard {
    store: Weak<ConversationContinuationStore>,
    physical_key: String,
}

impl Drop for ModelCallGuard {
    fn drop(&mut self) {
        let Some(store) = self.store.upgrade() else {
            return;
        };
        let Ok(mut runtime) = store.runtime.lock() else {
            return;
        };
        let Some(count) = runtime.in_flight.get_mut(&self.physical_key) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            runtime.in_flight.remove(&self.physical_key);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct StoredTask {
    legacy_key: String,
    owner: String,
    generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_claim_digest: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct StoredToken {
    task: String,
    generation: u64,
    workspace: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct StoredState {
    version: u32,
    #[serde(default)]
    tasks: BTreeMap<String, StoredTask>,
    #[serde(default)]
    aliases: BTreeMap<String, String>,
    #[serde(default)]
    tokens: BTreeMap<String, StoredToken>,
}

impl Default for StoredState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            tasks: BTreeMap::new(),
            aliases: BTreeMap::new(),
            tokens: BTreeMap::new(),
        }
    }
}

struct RuntimeState {
    stored: StoredState,
    in_flight: HashMap<String, u64>,
}

pub(crate) struct ConversationContinuationStore {
    path: PathBuf,
    runtime: Mutex<RuntimeState>,
}

impl ConversationContinuationStore {
    pub(crate) fn for_current_user(config: &AppConfig) -> Result<Self, String> {
        let home =
            crate::util::home_dir().ok_or("Conversation handoffs require a home directory")?;
        let scope = Sha256::digest(format!("{}\0{}", config.work_dir.display(), config.port));
        Self::new(
            home.join(".codexify")
                .join("conversation-continuations")
                .join(format!("{scope:x}.json")),
        )
    }

    pub(crate) fn new(path: PathBuf) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err("Conversation handoff state path must be absolute".into());
        }
        let stored = load_state(&path)?;
        Ok(Self {
            path,
            runtime: Mutex::new(RuntimeState {
                stored,
                in_flight: HashMap::new(),
            }),
        })
    }

    #[cfg(test)]
    pub(crate) fn resolve(
        &self,
        physical: &ConversationIdentity,
    ) -> Result<ConversationOwnership, String> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| "Conversation handoff state is unavailable")?;
        resolve_stored(&runtime.stored, physical)
    }

    pub(crate) fn owner_snapshot(
        &self,
        physical: &ConversationIdentity,
    ) -> Result<TaskOwnerSnapshot, String> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| "Conversation handoff state is unavailable")?;
        let Some(task_key) = runtime.stored.aliases.get(physical.stable_key()) else {
            return Ok(TaskOwnerSnapshot {
                task: physical.clone(),
                owner_key: physical.stable_key().to_string(),
                generation: 0,
                active: true,
            });
        };
        let task_record = runtime.stored.tasks.get(task_key).ok_or_else(|| {
            "Conversation handoff state contains a missing task record".to_string()
        })?;
        let task = ConversationIdentity::from_stable_keys(
            task_key.clone(),
            task_record.legacy_key.clone(),
        )
        .ok_or_else(|| {
            "Conversation handoff state contains an invalid task identity".to_string()
        })?;
        Ok(TaskOwnerSnapshot {
            task,
            owner_key: task_record.owner.clone(),
            generation: task_record.generation,
            active: task_record.owner == physical.stable_key(),
        })
    }

    pub(crate) fn owner_generation_for_task_key(&self, task_key: &str) -> Result<u64, String> {
        if ConversationIdentity::from_stable_keys(task_key.to_string(), task_key.to_string())
            .is_none()
        {
            return Err("Invalid stable Codexify task identity".into());
        }
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| "Conversation handoff state is unavailable")?;
        Ok(runtime
            .stored
            .tasks
            .get(task_key)
            .map_or(0, |task| task.generation))
    }

    pub(crate) fn begin_model_call(
        self: &Arc<Self>,
        physical: &ConversationIdentity,
    ) -> Result<(ConversationIdentity, ModelCallGuard), String> {
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| "Conversation handoff state is unavailable")?;
        let task = match resolve_stored(&runtime.stored, physical)? {
            ConversationOwnership::Active { task } => task,
            ConversationOwnership::Retired { .. } => return Err(RETIRED_MESSAGE.into()),
        };
        *runtime
            .in_flight
            .entry(physical.stable_key().to_string())
            .or_default() += 1;
        Ok((
            task,
            ModelCallGuard {
                store: Arc::downgrade(self),
                physical_key: physical.stable_key().to_string(),
            },
        ))
    }

    pub(crate) fn issue_token(
        &self,
        physical: &ConversationIdentity,
        task: &ConversationIdentity,
        workspace: &Path,
    ) -> Result<String, String> {
        if !workspace.is_absolute() {
            return Err("The continuation workspace must be absolute".into());
        }
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| "Conversation handoff state is unavailable")?;
        let resolved = match resolve_stored(&runtime.stored, physical)? {
            ConversationOwnership::Active { task } => task,
            ConversationOwnership::Retired { .. } => return Err(RETIRED_MESSAGE.into()),
        };
        if resolved.stable_key() != task.stable_key() {
            return Err("The current conversation does not own this Codexify task".into());
        }

        let mut next = runtime.stored.clone();
        let record = next
            .tasks
            .entry(task.stable_key().to_string())
            .or_insert_with(|| StoredTask {
                legacy_key: task.legacy_stable_key().to_string(),
                owner: physical.stable_key().to_string(),
                generation: 0,
                workspace: None,
                last_claim_digest: None,
            });
        if record.owner != physical.stable_key() || record.legacy_key != task.legacy_stable_key() {
            return Err("The current conversation does not own this Codexify task".into());
        }
        let generation = record.generation;
        record.workspace = Some(workspace.to_string_lossy().into_owned());
        next.aliases.insert(
            physical.stable_key().to_string(),
            task.stable_key().to_string(),
        );
        next.tokens
            .retain(|_, token| token.task != task.stable_key());

        let token = generate_token()?;
        next.tokens.insert(
            token_digest(&token),
            StoredToken {
                task: task.stable_key().to_string(),
                generation,
                workspace: workspace.to_string_lossy().into_owned(),
            },
        );
        validate_state(&next)?;
        persist_state(&self.path, &next)?;
        runtime.stored = next;
        Ok(token)
    }

    /// The authenticated local owner can recover a task whose browser page is
    /// inaccessible. Resolve the actual physical owner rather than cloning it.
    pub(crate) fn issue_recovery_token(&self,task:&ConversationIdentity,workspace:&Path)->Result<String,String> {
        let snapshot=self.owner_snapshot(task)?;
        if snapshot.task.stable_key()!=task.stable_key(){return Err("Recovery task identity changed".into());}
        let owner=ConversationIdentity::from_stable_keys(snapshot.owner_key.clone(),snapshot.owner_key)
            .ok_or("The current task owner is invalid")?;
        self.issue_token(&owner,&snapshot.task,workspace)
    }

    pub(crate) fn claim<F>(
        &self,
        physical: &ConversationIdentity,
        token: &str,
        validate: F,
    ) -> Result<ClaimedContinuation, String>
    where
        F: FnOnce(&ConversationIdentity, &Path) -> Result<(), String>,
    {
        validate_token(token)?;
        let digest = token_digest(token);
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| "Conversation handoff state is unavailable")?;
        let token_record = runtime
            .stored
            .tokens
            .get(&digest)
            .cloned()
            .ok_or_else(invalid_token)?;
        let task_record = runtime
            .stored
            .tasks
            .get(&token_record.task)
            .cloned()
            .ok_or_else(|| "Conversation handoff state is inconsistent".to_string())?;
        if task_record.generation != token_record.generation {
            return Err(invalid_token());
        }
        if task_record.owner == physical.stable_key() {
            return Err("Use the continuation token in a new ChatGPT conversation".into());
        }
        if runtime.stored.aliases.contains_key(physical.stable_key()) {
            return Err("This ChatGPT conversation is already attached to a Codexify task".into());
        }
        if runtime
            .in_flight
            .get(&task_record.owner)
            .copied()
            .unwrap_or_default()
            > 0
        {
            return Err(
                "The previous conversation still has a Codexify call in flight; retry after it finishes"
                    .into(),
            );
        }

        let task = ConversationIdentity::from_stable_keys(
            token_record.task.clone(),
            task_record.legacy_key.clone(),
        )
        .ok_or_else(|| {
            "Conversation handoff state contains an invalid task identity".to_string()
        })?;
        let workspace = PathBuf::from(&token_record.workspace);
        validate(&task, &workspace)?;

        let mut next = runtime.stored.clone();
        let next_task = next
            .tasks
            .get_mut(task.stable_key())
            .ok_or_else(|| "Conversation handoff state is inconsistent".to_string())?;
        if next_task.owner != task_record.owner || next_task.generation != token_record.generation {
            return Err("Conversation handoff changed while it was being claimed".into());
        }
        next_task.owner = physical.stable_key().to_string();
        next_task.generation = next_task
            .generation
            .checked_add(1)
            .ok_or("Conversation handoff generation overflow")?;
        next_task.workspace = Some(token_record.workspace.clone());
        next_task.last_claim_digest = Some(digest);
        next.aliases.insert(
            physical.stable_key().to_string(),
            task.stable_key().to_string(),
        );
        next.tokens
            .retain(|_, token| token.task != task.stable_key());
        validate_state(&next)?;
        persist_state(&self.path, &next)?;
        runtime.stored = next;
        Ok(ClaimedContinuation { task, workspace })
    }

    pub(crate) fn repeated_claim(
        &self,
        physical: &ConversationIdentity,
        token: &str,
    ) -> Result<Option<ClaimedContinuation>, String> {
        validate_token(token)?;
        let digest = token_digest(token);
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| "Conversation handoff state is unavailable")?;
        let task = match resolve_stored(&runtime.stored, physical)? {
            ConversationOwnership::Active { task } => task,
            ConversationOwnership::Retired { .. } => return Err(RETIRED_MESSAGE.into()),
        };
        let Some(record) = runtime.stored.tasks.get(task.stable_key()) else {
            return Ok(None);
        };
        if record.owner != physical.stable_key()
            || record.last_claim_digest.as_deref() != Some(digest.as_str())
        {
            return Ok(None);
        }
        let workspace = record
            .workspace
            .as_deref()
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| "Conversation handoff state contains no valid workspace".to_string())?;
        Ok(Some(ClaimedContinuation { task, workspace }))
    }
}

fn resolve_stored(
    state: &StoredState,
    physical: &ConversationIdentity,
) -> Result<ConversationOwnership, String> {
    let Some(task_key) = state.aliases.get(physical.stable_key()) else {
        return Ok(ConversationOwnership::Active {
            task: physical.clone(),
        });
    };
    let task_record = state
        .tasks
        .get(task_key)
        .ok_or_else(|| "Conversation handoff state contains a missing task record".to_string())?;
    let task =
        ConversationIdentity::from_stable_keys(task_key.clone(), task_record.legacy_key.clone())
            .ok_or_else(|| {
                "Conversation handoff state contains an invalid task identity".to_string()
            })?;
    if task_record.owner == physical.stable_key() {
        Ok(ConversationOwnership::Active { task })
    } else {
        Ok(ConversationOwnership::Retired { task })
    }
}

fn generate_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| format!("Could not generate a continuation token: {error}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn validate_token(token: &str) -> Result<(), String> {
    if token.len() != 43
        || URL_SAFE_NO_PAD
            .decode(token)
            .ok()
            .is_none_or(|bytes| bytes.len() != 32)
    {
        return Err(invalid_token());
    }
    Ok(())
}

fn token_digest(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn invalid_token() -> String {
    "The continuation token is invalid, expired, or already used".into()
}

fn valid_key(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_state(state: &StoredState) -> Result<(), String> {
    if state.version != STATE_VERSION {
        return Err(format!(
            "Unsupported conversation handoff state version {}",
            state.version
        ));
    }
    if state.tasks.len() > MAX_RECORDS
        || state.aliases.len() > MAX_RECORDS
        || state.tokens.len() > MAX_RECORDS
    {
        return Err("Conversation handoff state contains too many records".into());
    }
    for (task_key, task) in &state.tasks {
        if !valid_key(task_key) || !valid_key(&task.legacy_key) || !valid_key(&task.owner) {
            return Err("Conversation handoff state contains an invalid identity".into());
        }
        if task
            .workspace
            .as_deref()
            .is_some_and(|workspace| !Path::new(workspace).is_absolute())
            || task
                .last_claim_digest
                .as_deref()
                .is_some_and(|digest| !valid_key(digest))
            || (task.last_claim_digest.is_some() && task.workspace.is_none())
        {
            return Err("Conversation handoff state contains invalid claim metadata".into());
        }
    }
    for (physical, task) in &state.aliases {
        if !valid_key(physical) || !state.tasks.contains_key(task) {
            return Err("Conversation handoff state contains an invalid alias".into());
        }
    }
    for (digest, token) in &state.tokens {
        if !valid_key(digest)
            || !state.tasks.contains_key(&token.task)
            || !Path::new(&token.workspace).is_absolute()
        {
            return Err("Conversation handoff state contains an invalid token record".into());
        }
        let task = &state.tasks[&token.task];
        if token.generation != task.generation {
            return Err("Conversation handoff state contains a stale token record".into());
        }
    }
    for (task_key, task) in &state.tasks {
        if state.aliases.get(&task.owner) != Some(task_key) {
            return Err("Conversation handoff state contains an owner without an alias".into());
        }
    }
    Ok(())
}

fn load_state(path: &Path) -> Result<StoredState, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StoredState::default());
        }
        Err(error) => {
            return Err(format!(
                "Could not inspect conversation handoff state: {error}"
            ));
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("Conversation handoff state must be a regular non-symlink file".into());
    }
    if metadata.len() > MAX_STATE_BYTES {
        return Err("Conversation handoff state is too large".into());
    }
    validate_private_file(&metadata)?;

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("Could not open conversation handoff state: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("Could not inspect open conversation handoff state: {error}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("Conversation handoff state must remain a regular file".into());
    }
    validate_private_file(&metadata)?;
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read conversation handoff state: {error}"))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("Conversation handoff state is too large".into());
    }
    let state: StoredState = serde_json::from_slice(&bytes)
        .map_err(|_| "Conversation handoff state is invalid JSON".to_string())?;
    validate_state(&state)?;
    Ok(state)
}

fn persist_state(path: &Path, state: &StoredState) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or("Conversation handoff state has no parent directory")?;
    ensure_private_directory(parent)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err("Conversation handoff state must be a regular non-symlink file".into());
    }
    let bytes = serde_json::to_vec(state)
        .map_err(|error| format!("Could not encode conversation handoff state: {error}"))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("Conversation handoff state is too large".into());
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("Could not create conversation handoff state: {error}"))?;
    set_private_file_permissions(temporary.path())?;
    temporary
        .write_all(&bytes)
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|error| format!("Could not write conversation handoff state: {error}"))?;
    temporary.persist(path).map_err(|error| {
        format!(
            "Could not publish conversation handoff state: {}",
            error.error
        )
    })?;
    set_private_file_permissions(path)?;
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("Could not create conversation handoff directory: {error}"))?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect conversation handoff directory: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Conversation handoff directory must be a real directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err("Conversation handoff directory must belong to the current user".into());
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
            |error| format!("Could not protect conversation handoff directory: {error}"),
        )?;
    }
    Ok(())
}

fn validate_private_file(metadata: &std::fs::Metadata) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(
                "Conversation handoff state must belong to the current user and have mode 0600"
                    .into(),
            );
        }
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Could not protect conversation handoff state: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Barrier};

    use base64::Engine as _;
    use sha2::{Digest, Sha256};

    use super::{ConversationContinuationStore, ConversationOwnership};
    use crate::project_bindings::ConversationIdentity;

    fn identity(value: &str) -> ConversationIdentity {
        ConversationIdentity::from_openai_session(value).unwrap()
    }

    fn assert_active(
        store: &ConversationContinuationStore,
        physical: &ConversationIdentity,
        expected_task: &ConversationIdentity,
    ) {
        let ConversationOwnership::Active { task } = store.resolve(physical).unwrap() else {
            panic!("expected active owner");
        };
        assert_eq!(task.stable_key(), expected_task.stable_key());
        assert_eq!(task.legacy_stable_key(), expected_task.legacy_stable_key());
    }

    fn assert_retired(
        store: &ConversationContinuationStore,
        physical: &ConversationIdentity,
        expected_task: &ConversationIdentity,
    ) {
        let ConversationOwnership::Retired { task } = store.resolve(physical).unwrap() else {
            panic!("expected retired owner");
        };
        assert_eq!(task.stable_key(), expected_task.stable_key());
        assert_eq!(task.legacy_stable_key(), expected_task.legacy_stable_key());
    }

    #[test]
    fn issued_tokens_are_random_and_only_digests_are_persisted() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("continuations.json");
        let store = ConversationContinuationStore::new(path.clone()).unwrap();
        let source = identity("source");
        let workspace = root.path().join("workspace");

        let first = store.issue_token(&source, &source, &workspace).unwrap();
        let second = store.issue_token(&source, &source, &workspace).unwrap();

        assert_ne!(first, second);
        for token in [&first, &second] {
            let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(token)
                .unwrap();
            assert_eq!(decoded.len(), 32);
        }
        let stored = std::fs::read_to_string(path).unwrap();
        assert!(!stored.contains(&first));
        assert!(!stored.contains(&second));
        assert!(stored.contains(&format!("{:x}", Sha256::digest(second.as_bytes()))));
    }

    #[test]
    fn reload_resolves_the_current_owner_to_the_original_task_identity() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("continuations.json");
        let workspace = root.path().join("workspace");
        let source = identity("source");
        let destination = identity("destination");
        let token = ConversationContinuationStore::new(path.clone())
            .unwrap()
            .issue_token(&source, &source, &workspace)
            .unwrap();

        let reloaded = ConversationContinuationStore::new(path.clone()).unwrap();
        let claimed = reloaded
            .claim(&destination, &token, |task, saved| {
                assert_eq!(task.stable_key(), source.stable_key());
                assert_eq!(saved, workspace);
                Ok(())
            })
            .unwrap();
        assert_eq!(claimed.task.stable_key(), source.stable_key());
        assert_eq!(claimed.workspace, workspace);

        let after_restart = ConversationContinuationStore::new(path).unwrap();
        assert_retired(&after_restart, &source, &source);
        assert_active(&after_restart, &destination, &source);
    }

    #[test]
    fn completed_claim_can_be_retried_by_the_new_owner_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("continuations.json");
        let workspace = root.path().join("workspace");
        let source = identity("source");
        let destination = identity("destination");
        let store = ConversationContinuationStore::new(path.clone()).unwrap();
        let token = store.issue_token(&source, &source, &workspace).unwrap();
        store.claim(&destination, &token, |_, _| Ok(())).unwrap();

        let restarted = ConversationContinuationStore::new(path).unwrap();
        let repeated = restarted
            .repeated_claim(&destination, &token)
            .unwrap()
            .unwrap();
        assert_eq!(repeated.task.stable_key(), source.stable_key());
        assert_eq!(repeated.workspace, workspace);
        let unrelated = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 32]);
        assert!(
            restarted
                .repeated_claim(&destination, &unrelated)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_new_token_revokes_the_previous_token_for_the_same_generation() {
        let root = tempfile::tempdir().unwrap();
        let store = ConversationContinuationStore::new(root.path().join("state.json")).unwrap();
        let source = identity("source");
        let destination = identity("destination");
        let workspace = root.path().join("workspace");
        let first = store.issue_token(&source, &source, &workspace).unwrap();
        let second = store.issue_token(&source, &source, &workspace).unwrap();

        assert!(
            store
                .claim(&destination, &first, |_, _| Ok(()))
                .unwrap_err()
                .contains("invalid")
        );
        store.claim(&destination, &second, |_, _| Ok(())).unwrap();
        assert_active(&store, &destination, &source);
    }

    #[test]
    fn one_claim_wins_and_replay_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let store =
            Arc::new(ConversationContinuationStore::new(root.path().join("state.json")).unwrap());
        let source = identity("source");
        let workspace = root.path().join("workspace");
        let token = store.issue_token(&source, &source, &workspace).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for name in ["destination-a", "destination-b"] {
            let store = store.clone();
            let barrier = barrier.clone();
            let token = token.clone();
            threads.push(std::thread::spawn(move || {
                let destination = identity(name);
                barrier.wait();
                let result = store.claim(&destination, &token, |_, _| Ok(()));
                (destination, result)
            }));
        }
        barrier.wait();
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            results.iter().filter(|(_, result)| result.is_ok()).count(),
            1
        );
        let winner = results
            .iter()
            .find_map(|(identity, result)| result.is_ok().then_some(identity))
            .unwrap();
        assert_active(&store, winner, &source);
        assert_retired(&store, &source, &source);
        assert!(
            store
                .claim(&identity("replay"), &token, |_, _| Ok(()))
                .unwrap_err()
                .contains("invalid")
        );
    }

    #[test]
    fn failed_claim_validation_preserves_owner_and_token() {
        let root = tempfile::tempdir().unwrap();
        let store = ConversationContinuationStore::new(root.path().join("state.json")).unwrap();
        let source = identity("source");
        let destination = identity("destination");
        let workspace = root.path().join("workspace");
        let token = store.issue_token(&source, &source, &workspace).unwrap();

        assert_eq!(
            store
                .claim(&destination, &token, |_, _| Err("workspace changed".into()))
                .unwrap_err(),
            "workspace changed"
        );
        assert_active(&store, &source, &source);
        store
            .claim(&destination, &token, |task, path| {
                assert_eq!(task.stable_key(), source.stable_key());
                assert_eq!(path, Path::new(&workspace));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn claim_rejects_while_the_source_has_a_model_call_in_flight() {
        let root = tempfile::tempdir().unwrap();
        let store =
            Arc::new(ConversationContinuationStore::new(root.path().join("state.json")).unwrap());
        let source = identity("source");
        let destination = identity("destination");
        let workspace = root.path().join("workspace");
        let token = store.issue_token(&source, &source, &workspace).unwrap();
        let (task, guard) = store.begin_model_call(&source).unwrap();
        assert_eq!(task.stable_key(), source.stable_key());

        assert!(
            store
                .claim(&destination, &token, |_, _| Ok(()))
                .unwrap_err()
                .contains("in flight")
        );
        assert_active(&store, &source, &source);
        drop(guard);
        store.claim(&destination, &token, |_, _| Ok(())).unwrap();
        assert_active(&store, &destination, &source);
    }

    #[test]
    fn owner_snapshots_expose_task_owner_generation_without_raw_sessions() {
        let root = tempfile::tempdir().unwrap();
        let store = ConversationContinuationStore::new(root.path().join("state.json")).unwrap();
        let source = identity("snapshot-source");
        let destination = identity("snapshot-destination");
        let workspace = root.path().join("workspace");

        let initial = store.owner_snapshot(&source).unwrap();
        assert_eq!(initial.task.stable_key(), source.stable_key());
        assert_eq!(initial.owner_key, source.stable_key());
        assert_eq!(initial.generation, 0);
        assert!(initial.active);

        let token = store.issue_token(&source, &source, &workspace).unwrap();
        store.claim(&destination, &token, |_, _| Ok(())).unwrap();

        let retired = store.owner_snapshot(&source).unwrap();
        assert_eq!(retired.task.stable_key(), source.stable_key());
        assert_eq!(retired.owner_key, destination.stable_key());
        assert_eq!(retired.generation, 1);
        assert!(!retired.active);

        let active = store.owner_snapshot(&destination).unwrap();
        assert_eq!(active, retired.with_active(true));
    }
}
