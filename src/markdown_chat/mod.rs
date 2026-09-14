//! Optional conversation-scoped Markdown communication, separate from repository files.

pub mod notification;
pub(crate) mod output;
mod storage;
mod wait;

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::exec_sessions::SessionState;
use crate::project_bindings::ConversationIdentity;
use crate::types::AppConfig;

pub use storage::{
    AppendReceipt, ChatFile, ChatSnapshot, NotificationState, UserSendReceipt, WidgetPage,
};
pub use wait::WaitOutcome;

pub const DEFAULT_MAX_WAIT_MS: u64 = 270_000;
pub const MAX_UNREAD_BYTES: usize = 16 * 1024 * 1024;
pub const USER_MESSAGE_FIELD: &str = "new_chat_message_from_user";

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct MarkdownChatConfig {
    pub enabled: bool,
    pub max_wait_ms: u64,
    pub ntfy: Option<NtfyConfig>,
    pub notifications: Option<NotificationsConfig>,
}

impl Default for MarkdownChatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_wait_ms: DEFAULT_MAX_WAIT_MS,
            ntfy: None,
            notifications: None,
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

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NotificationsConfig {
    pub urls: Vec<String>,
    #[serde(default = "default_notification_python")]
    pub python_path: String,
    #[serde(default = "default_notification_timeout")]
    pub timeout_ms: u64,
}

fn default_notification_python() -> String {
    if cfg!(windows) { "python" } else { "python3" }.into()
}

fn default_notification_timeout() -> u64 {
    15_000
}

impl fmt::Debug for NotificationsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NotificationsConfig")
            .field("destinations", &self.urls.len())
            .field("python_path", &self.python_path)
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

impl NotificationsConfig {
    fn validate(&self) -> Result<(), String> {
        if self.urls.is_empty() || self.urls.len() > 32 {
            return Err(
                "markdownChat.notifications.urls must contain 1 to 32 Apprise service URLs".into(),
            );
        }
        if self.urls.iter().any(|url| {
            url.len() > 16_384
                || !url.contains("://")
                || url.chars().any(char::is_control)
                || reqwest::Url::parse(url).is_err()
        }) {
            return Err("markdownChat.notifications.urls contains an invalid service URL".into());
        }
        if self.python_path.trim().is_empty() || self.python_path.contains('\0') {
            return Err("markdownChat.notifications.pythonPath must name a Python interpreter with Apprise installed".into());
        }
        if !(1_000..=60_000).contains(&self.timeout_ms) {
            return Err(
                "markdownChat.notifications.timeoutMs must be between 1000 and 60000".into(),
            );
        }
        Ok(())
    }
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
        if let Some(notifications) = &self.notifications {
            if self.ntfy.is_some() {
                return Err(
                    "configure markdownChat.notifications or legacy markdownChat.ntfy, not both"
                        .into(),
                );
            }
            notifications.validate()?;
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
    activity: Mutex<HashMap<String, u64>>,
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
            activity: Mutex::new(HashMap::new()),
            transport_namespace: format!("{}-{timestamp}", std::process::id()),
        }
    }
}

impl MarkdownChatStore {
    fn owner(&self, conversation: Option<&ConversationIdentity>, session: &SessionState) -> String {
        match conversation {
            Some(identity) => identity.stable_key().to_string(),
            None => format!(
                "transport-{}-{}",
                self.transport_namespace,
                session.audit_id()
            ),
        }
    }

    pub(crate) fn record_agent_call(
        &self,
        conversation: Option<&ConversationIdentity>,
        session: &SessionState,
        at_ms: u64,
    ) {
        if let Ok(mut activity) = self.activity.lock() {
            let last = activity
                .entry(self.owner(conversation, session))
                .or_default();
            *last = (*last).max(at_ms);
        }
    }

    pub(crate) fn last_agent_call(
        &self,
        conversation: Option<&ConversationIdentity>,
        session: &SessionState,
    ) -> Option<u64> {
        self.activity
            .lock()
            .ok()?
            .get(&self.owner(conversation, session))
            .copied()
    }

    pub fn chat(
        &self,
        config: &AppConfig,
        conversation: Option<&ConversationIdentity>,
        session: &SessionState,
    ) -> Result<Arc<ChatFile>, String> {
        if !config.markdown_chat.enabled {
            return Err("Markdown chat is disabled in the server configuration.".into());
        }
        let owner = self.owner(conversation, session);
        let path = crate::memory::memory_dir(config)
            .join("chats")
            .join(owner)
            .join("CHAT.md");
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .map_err(|_| "Cannot resolve the Markdown chat directory")?
                .join(path)
        };
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

pub(crate) async fn history_call(
    args: &serde_json::Value,
    config: &AppConfig,
    session: &SessionState,
    context: &crate::tool::ToolRequestContext,
) -> Result<Option<(serde_json::Value, AppConfig)>, String> {
    if !config.markdown_chat.enabled {
        return Ok(None);
    }
    let Some(input) = args.get("path").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    let chat = context
        .markdown_chat
        .chat(config, context.conversation.as_ref(), session)?;
    let requested = crate::safe_path::lexical_normalize(&config.work_dir.join(input));
    let file = crate::safe_path::lexical_normalize(chat.path());
    let channel_dir = file.parent().ok_or("CHAT.md has no parent")?;
    let chats_dir = channel_dir
        .parent()
        .ok_or("CHAT.md has no metadata directory")?;
    if !requested.starts_with(chats_dir) {
        return Ok(None);
    }
    if requested != file {
        return Err("Only this conversation's CHAT.md is available through chat history reads. Use chat_read for new messages.".into());
    }
    chat.ensure().await?;
    let mut args = args.clone();
    args["path"] = serde_json::Value::String("CHAT.md".into());
    let mut config = config.clone();
    config.work_dir = channel_dir.to_path_buf();
    Ok(Some((args, config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_is_conversation_scoped_with_transport_fallback() {
        let store = MarkdownChatStore::default();
        let first = SessionState::new();
        let second = SessionState::new();
        let a = ConversationIdentity::from_openai_session("a").unwrap();
        let b = ConversationIdentity::from_openai_session("b").unwrap();
        store.record_agent_call(Some(&a), &first, 1000);
        store.record_agent_call(Some(&a), &second, 800);
        assert_eq!(store.last_agent_call(Some(&a), &second), Some(1000));
        assert_eq!(store.last_agent_call(Some(&b), &first), None);
        assert_eq!(store.last_agent_call(None, &first), None);
        store.record_agent_call(None, &first, 1500);
        assert_eq!(store.last_agent_call(None, &first), Some(1500));
        assert_eq!(store.last_agent_call(None, &second), None);
    }
}
