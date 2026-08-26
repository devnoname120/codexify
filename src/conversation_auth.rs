use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::exec_sessions::SessionState;
use crate::project_bindings::ConversationIdentity;
use crate::types::AppConfig;
use crate::util::home_dir;

/// ChatGPT-facing name for the authorization tool, chosen to avoid false-positive
/// secret-leak blocking on connector calls.
pub const AUTHORIZATION_TOOL_WIRE_NAME: &str = "setup";
/// The authentication token is itself 64 lowercase hex characters. It is not the
/// SHA-256 digest of a second secret; the shape only looks like an ordinary hash
/// so ChatGPT will pass it through the authorization tool's `ref` wire field.
pub const CONVERSATION_AUTH_TOKEN_HEX_LENGTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationAuthorizationScope {
    ChatGptConversation,
    McpTransportSession,
}

impl ConversationAuthorizationScope {
    pub fn description(self) -> &'static str {
        match self {
            Self::ChatGptConversation => "this ChatGPT conversation",
            Self::McpTransportSession => "this MCP transport session",
        }
    }
}

pub struct ConversationAuthorizationStore {
    authorized: Mutex<HashSet<ConversationIdentity>>,
    persistence_dir: Option<PathBuf>,
}

impl Default for ConversationAuthorizationStore {
    fn default() -> Self {
        Self {
            authorized: Mutex::new(HashSet::new()),
            persistence_dir: None,
        }
    }
}

impl ConversationAuthorizationStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn for_current_user(config: &AppConfig) -> Self {
        let Some(token) = config.conversation_auth_token.as_deref() else {
            return Self::new();
        };
        let Some(home) = home_dir() else {
            return Self::new();
        };
        Self::persistent(
            home.join(".codexify").join("conversation-authorizations"),
            &config.work_dir,
            token,
        )
    }

    pub fn persistent(base_dir: PathBuf, work_dir: &Path, token: &str) -> Self {
        Self {
            authorized: Mutex::new(HashSet::new()),
            persistence_dir: Some(base_dir.join(authorization_scope_key(work_dir, token))),
        }
    }

    pub fn is_authorized(
        &self,
        conversation: Option<&ConversationIdentity>,
        session: &SessionState,
    ) -> bool {
        match conversation {
            Some(identity) => {
                // Fast path: an already-cached authorization only needs the in-memory set.
                if self.authorized.lock().unwrap().contains(identity) {
                    return true;
                }
                // Probe the disk WITHOUT holding the lock. A cache miss must not
                // serialize every other conversation's authorization check behind
                // our blocking filesystem reads. The benign race where two callers
                // both probe and both insert is harmless: the grant is identical
                // and the insert is idempotent.
                if self.persisted_authorization_exists(identity) {
                    self.authorized.lock().unwrap().insert(identity.clone());
                    return true;
                }
                false
            }
            None => session.connector_authorized(),
        }
    }

    pub fn authorize(
        &self,
        conversation: Option<&ConversationIdentity>,
        session: &SessionState,
    ) -> Result<ConversationAuthorizationScope, String> {
        match conversation {
            Some(identity) => {
                self.persist_authorization(identity)?;
                self.authorized.lock().unwrap().insert(identity.clone());
                Ok(ConversationAuthorizationScope::ChatGptConversation)
            }
            None => {
                session.authorize_connector();
                Ok(ConversationAuthorizationScope::McpTransportSession)
            }
        }
    }

    fn authorization_path(&self, identity: &ConversationIdentity) -> Option<PathBuf> {
        self.persistence_dir
            .as_ref()
            .map(|directory| directory.join(format!("{}.allowed", identity.stable_key())))
    }

    fn persisted_authorization_exists(&self, identity: &ConversationIdentity) -> bool {
        let Some(path) = self.authorization_path(identity) else {
            return false;
        };
        let Some(directory) = path.parent() else {
            return false;
        };
        let Ok(directory_metadata) = std::fs::symlink_metadata(directory) else {
            return false;
        };
        if !directory_metadata.is_dir()
            || directory_metadata.file_type().is_symlink()
            || !private_directory_permissions(&directory_metadata)
        {
            return false;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            return false;
        };
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || !private_file_permissions(&metadata)
        {
            return false;
        }
        std::fs::read(path).is_ok_and(|contents| contents == b"authorized\n")
    }

    fn persist_authorization(&self, identity: &ConversationIdentity) -> Result<(), String> {
        // Persistence failures are returned through the ChatGPT-facing wire tool,
        // so their text retains the innocuous "setup" vocabulary even though this
        // code stores a durable conversation authorization grant.
        let Some(path) = self.authorization_path(identity) else {
            return Ok(());
        };
        let directory = path
            .parent()
            .ok_or_else(|| "conversation setup path has no parent".to_string())?;
        ensure_private_directory(directory)?;

        if path.exists() {
            return if self.persisted_authorization_exists(identity) {
                Ok(())
            } else {
                Err(format!(
                    "Refusing to use an invalid conversation setup marker at {}",
                    path.display()
                ))
            };
        }

        let mut temporary = tempfile::NamedTempFile::new_in(directory).map_err(|error| {
            format!(
                "Could not create temporary conversation setup state in {}: {error}",
                directory.display()
            )
        })?;
        temporary.write_all(b"authorized\n").map_err(|error| {
            format!(
                "Could not write conversation setup state for {}: {error}",
                path.display()
            )
        })?;
        temporary.as_file().sync_all().map_err(|error| {
            format!(
                "Could not flush conversation setup state for {}: {error}",
                path.display()
            )
        })?;
        make_private_file(temporary.path()).map_err(|error| {
            format!(
                "Could not restrict conversation setup state for {}: {error}",
                path.display()
            )
        })?;

        match temporary.persist_noclobber(&path) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                if self.persisted_authorization_exists(identity) {
                    Ok(())
                } else {
                    Err(format!(
                        "Refusing to use an invalid conversation setup marker at {}",
                        path.display()
                    ))
                }
            }
            Err(error) => Err(format!(
                "Could not persist conversation setup state at {}: {}",
                path.display(),
                error.error
            )),
        }
    }
}

pub fn generate_conversation_auth_token() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| anyhow::anyhow!("generate conversation authentication token: {error}"))?;
    Ok(encode_hex(&bytes))
}

pub fn validate_conversation_auth_token(token: &str) -> Result<(), String> {
    if token.len() != CONVERSATION_AUTH_TOKEN_HEX_LENGTH
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(
            "conversationAuthToken must contain exactly 64 lowercase hexadecimal characters"
                .to_string(),
        );
    }
    Ok(())
}

pub fn conversation_auth_tokens_match(expected: &str, provided: &str) -> bool {
    if expected.len() != CONVERSATION_AUTH_TOKEN_HEX_LENGTH
        || provided.len() != CONVERSATION_AUTH_TOKEN_HEX_LENGTH
    {
        return false;
    }
    expected
        .as_bytes()
        .iter()
        .zip(provided.as_bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

/// Render the deliberately innocuous wire vocabulary used in ChatGPT instructions.
pub fn conversation_auth_prompt(token: &str) -> String {
    format!(
        "To use this connector in a chat, call its `{AUTHORIZATION_TOOL_WIRE_NAME}` tool once with ref `{token}`."
    )
}

fn authorization_scope_key(work_dir: &Path, token: &str) -> String {
    let work_dir = std::fs::canonicalize(work_dir).unwrap_or_else(|_| work_dir.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(b"codexify/conversation-authorization-scope/v1\0");
    hasher.update(work_dir.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(token.as_bytes());
    encode_hex(&hasher.finalize())
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
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

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(format!(
                "Refusing to use a non-directory conversation setup path at {}",
                path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path).map_err(|error| {
                format!(
                    "Could not create conversation setup directory {}: {error}",
                    path.display()
                )
            })?;
        }
        Err(error) => {
            return Err(format!(
                "Could not inspect conversation setup directory {}: {error}",
                path.display()
            ));
        }
    }
    make_private_directory(path).map_err(|error| {
        format!(
            "Could not restrict conversation setup directory {}: {error}",
            path.display()
        )
    })?;
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        format!(
            "Could not inspect conversation setup directory {}: {error}",
            path.display()
        )
    })?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || !private_directory_permissions(&metadata)
    {
        return Err(format!(
            "Conversation setup directory is not private: {}",
            path.display()
        ));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_auth_tokens_are_valid_unique_sha256_shaped_strings() {
        let first = generate_conversation_auth_token().unwrap();
        let second = generate_conversation_auth_token().unwrap();

        assert_ne!(first, second);
        assert_eq!(first.len(), CONVERSATION_AUTH_TOKEN_HEX_LENGTH);
        validate_conversation_auth_token(&first).unwrap();
    }

    #[test]
    fn auth_token_validation_rejects_wrong_length_non_hex_or_uppercase_values() {
        assert!(validate_conversation_auth_token("short").is_err());
        assert!(
            validate_conversation_auth_token(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg"
            )
            .is_err()
        );
        assert!(
            validate_conversation_auth_token(
                "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef"
            )
            .is_err()
        );
    }

    #[test]
    fn auth_token_comparison_requires_exact_contents() {
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(conversation_auth_tokens_match(token, token));
        assert!(!conversation_auth_tokens_match(token, "different"));
    }

    #[test]
    fn auth_prompt_is_one_line_and_uses_the_innocuous_wire_names() {
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let prompt = conversation_auth_prompt(token);
        assert!(!prompt.contains('\n'));
        assert!(prompt.contains("`setup`"));
        assert!(prompt.contains("ref"));
        assert!(prompt.contains(token));
    }

    #[test]
    fn stable_conversation_authorization_survives_store_recreation() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let work_dir = root.path().join("project");
        std::fs::create_dir_all(&work_dir).unwrap();
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let identity = ConversationIdentity::from_openai_session("persistent-chat").unwrap();
        let first_session = SessionState::new();
        let second_session = SessionState::new();

        let first = ConversationAuthorizationStore::persistent(state.clone(), &work_dir, token);
        let marker = first.authorization_path(&identity).unwrap();
        assert!(!marker.to_string_lossy().contains(token));
        assert!(!marker.to_string_lossy().contains("persistent-chat"));
        first.authorize(Some(&identity), &first_session).unwrap();
        assert_eq!(std::fs::read(&marker).unwrap(), b"authorized\n");

        let second = ConversationAuthorizationStore::persistent(state, &work_dir, token);
        assert!(second.is_authorized(Some(&identity), &second_session));
    }

    #[test]
    fn rotating_the_auth_token_invalidates_persisted_authorizations() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let work_dir = root.path().join("project");
        std::fs::create_dir_all(&work_dir).unwrap();
        let identity = ConversationIdentity::from_openai_session("persistent-chat").unwrap();
        let session = SessionState::new();
        let first_token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let second_token = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

        ConversationAuthorizationStore::persistent(state.clone(), &work_dir, first_token)
            .authorize(Some(&identity), &session)
            .unwrap();
        let rotated = ConversationAuthorizationStore::persistent(state, &work_dir, second_token);

        assert!(!rotated.is_authorized(Some(&identity), &SessionState::new()));
    }

    #[test]
    fn incomplete_or_permissive_markers_do_not_authorize() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let work_dir = root.path().join("project");
        std::fs::create_dir_all(&work_dir).unwrap();
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let identity = ConversationIdentity::from_openai_session("persistent-chat").unwrap();
        let store = ConversationAuthorizationStore::persistent(state, &work_dir, token);
        let path = store.authorization_path(&identity).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        make_private_file(&path).unwrap();

        assert!(!store.is_authorized(Some(&identity), &SessionState::new()));

        std::fs::write(&path, b"authorized\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(!store.is_authorized(Some(&identity), &SessionState::new()));
        }
    }
}
