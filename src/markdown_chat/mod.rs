//! Optional conversation-scoped Markdown communication, separate from repository files.

mod storage;

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::exec_sessions::SessionState;
use crate::project_bindings::ConversationIdentity;
use crate::types::AppConfig;

pub use storage::{AppendReceipt, ChatFile, ChatSnapshot, NotificationState};

pub const DEFAULT_MAX_WAIT_MS: u64 = 270_000;
pub const MAX_UNREAD_BYTES: usize = 16 * 1024 * 1024;
pub const USER_MESSAGE_FIELD: &str = "new_chat_message_from_user";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct MarkdownChatConfig {
    pub enabled: bool,
    pub max_wait_ms: u64,
    pub ntfy: Option<NtfyConfig>,
}

impl Default for MarkdownChatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_wait_ms: DEFAULT_MAX_WAIT_MS,
            ntfy: None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NtfyConfig {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl fmt::Debug for NtfyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NtfyConfig")
            .field("url", &"<configured endpoint>")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl MarkdownChatConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(1_000..=300_000).contains(&self.max_wait_ms) {
            return Err("markdownChat.maxWaitMs must be between 1000 and 300000".into());
        }
        if let Some(ntfy) = &self.ntfy {
            let url = reqwest::Url::parse(&ntfy.url)
                .map_err(|_| "markdownChat.ntfy.url must be an HTTP(S) topic URL")?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || url.path().trim_matches('/').is_empty()
            {
                return Err("markdownChat.ntfy.url must be an HTTP(S) topic URL without credentials, query or fragment".into());
            }
            if let Some(token) = &ntfy.token
                && (token.is_empty()
                    || reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).is_err())
            {
                return Err("markdownChat.ntfy.token must be a nonempty valid bearer token".into());
            }
        }
        Ok(())
    }
}

pub struct MarkdownChatStore {
    channels: Mutex<HashMap<PathBuf, Arc<ChatFile>>>,
    transport_namespace: String,
}

impl Default for MarkdownChatStore {
    fn default() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            channels: Mutex::new(HashMap::new()),
            transport_namespace: format!("{}-{timestamp}", std::process::id()),
        }
    }
}

impl MarkdownChatStore {
    pub fn chat(
        &self,
        config: &AppConfig,
        conversation: Option<&ConversationIdentity>,
        session: &SessionState,
    ) -> Result<Arc<ChatFile>, String> {
        if !config.markdown_chat.enabled {
            return Err("Markdown chat is disabled in the server configuration.".into());
        }
        let owner = match conversation {
            Some(identity) => identity.stable_key().to_string(),
            None => format!(
                "transport-{}-{}",
                self.transport_namespace,
                session.audit_id()
            ),
        };
        let path = crate::memory::memory_dir(config)
            .join("chats")
            .join(owner)
            .join("CHAT.md");
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "Markdown chat store is unavailable")?;
        Ok(channels
            .entry(path.clone())
            .or_insert_with(|| Arc::new(ChatFile::new(path, conversation.is_some())))
            .clone())
    }
}
