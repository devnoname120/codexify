//! Optional conversation-scoped Markdown communication, separate from repository files.

pub mod attachments;
pub mod notification;
mod offline;
pub(crate) mod output;
mod storage;
mod title;
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

pub use storage::rollout;
pub use storage::{
    AGENT_STOP_MESSAGE, AppendReceipt, BrowserAppendReceipt, ChatFile, ChatSnapshot,
    NotificationState, OwnerChatOrganization, OwnerChatSummary, TranscriptCheckpoint,
    UnreadMessage, UnreadSnapshot, UserSendReceipt, WidgetPage,
};
pub use storage::{QueueReceipt, QueueSubmission, QueuedMessage};
pub use wait::WaitOutcome;

pub const DEFAULT_MAX_WAIT_MS: u64 = 55_000;
pub const DEFAULT_OWNER_CHAT_PORT: u16 = 3120;
pub const MAX_UNREAD_BYTES: usize = 16 * 1024 * 1024;
pub const USER_MESSAGE_FIELD: &str = "new_chat_message_from_user";
pub const AWAY_AFTER_MS: u64 = 180_000;
pub const DEFAULT_OFFLINE_TIMEOUT_MINUTES: u64 = 5;
pub const OFFLINE_AFTER_MS: u64 = DEFAULT_OFFLINE_TIMEOUT_MINUTES * 60_000;
pub const AUTOMATION_OFFLINE_AFTER_MS: u64 = OFFLINE_AFTER_MS;
pub const PAIRING_PRESENCE_GRACE_MS: u64 = 5_000;
pub const DEFAULT_AUTOMATION_DESCRIPTOR_PATH: &str = "~/.codexify/browser/descriptor.json";
pub const DEFAULT_AUTOMATION_CONNECTOR_NAME: &str = "";
pub const DEFAULT_AUTOMATION_MAX_QUEUED_ACTIONS: usize = 64;

pub(crate) const MAX_AUTOMATIC_RETRY_ATTEMPTS: usize = 32;
pub(crate) const MAX_RETRY_DELAY_SECONDS: u64 = 86_400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct RetryTimings {
    pub action_delays_seconds: Vec<u64>,
    pub chatgpt_access_delays_seconds: Vec<u64>,
    pub goal_recovery_delays_seconds: Vec<u64>,
    pub jitter_percent: u64,
    pub approval_resume_grace_seconds: u64,
    pub conversation_load_interval_seconds: u64,
    pub conversation_unavailable_cooldown_seconds: u64,
    pub rate_limit_cooldown_seconds: u64,
}

impl Default for RetryTimings {
    fn default() -> Self {
        Self {
            action_delays_seconds: vec![30, 60, 120, 240, 480],
            chatgpt_access_delays_seconds: vec![180, 300, 600, 1200, 1800, 3600],
            goal_recovery_delays_seconds: vec![60, 120, 240],
            jitter_percent: 10,
            approval_resume_grace_seconds: 180,
            conversation_load_interval_seconds: 180,
            conversation_unavailable_cooldown_seconds: 300,
            rate_limit_cooldown_seconds: 180,
        }
    }
}

impl RetryTimings {
    fn validate(&self) -> Result<(), String> {
        for (key, delays) in [
            ("actionDelaysSeconds", &self.action_delays_seconds),
            (
                "chatgptAccessDelaysSeconds",
                &self.chatgpt_access_delays_seconds,
            ),
            (
                "goalRecoveryDelaysSeconds",
                &self.goal_recovery_delays_seconds,
            ),
        ] {
            if delays.is_empty() || delays.len() > MAX_AUTOMATIC_RETRY_ATTEMPTS {
                return Err(format!(
                    "agentChat.automation.retryTimings.{key} must contain 1 to {MAX_AUTOMATIC_RETRY_ATTEMPTS} retry delays"
                ));
            }
            if delays
                .iter()
                .any(|delay| !(1..=MAX_RETRY_DELAY_SECONDS).contains(delay))
            {
                return Err(format!(
                    "agentChat.automation.retryTimings.{key} must contain whole seconds between 1 and {MAX_RETRY_DELAY_SECONDS}"
                ));
            }
        }
        for (key, seconds) in [
            (
                "approvalResumeGraceSeconds",
                self.approval_resume_grace_seconds,
            ),
            (
                "conversationLoadIntervalSeconds",
                self.conversation_load_interval_seconds,
            ),
            (
                "conversationUnavailableCooldownSeconds",
                self.conversation_unavailable_cooldown_seconds,
            ),
            ("rateLimitCooldownSeconds", self.rate_limit_cooldown_seconds),
        ] {
            if !(1..=MAX_RETRY_DELAY_SECONDS).contains(&seconds) {
                return Err(format!(
                    "agentChat.automation.retryTimings.{key} must be whole seconds between 1 and {MAX_RETRY_DELAY_SECONDS}"
                ));
            }
        }
        if self.jitter_percent > 100 {
            return Err(
                "agentChat.automation.retryTimings.jitterPercent must be between 0 and 100".into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentAutomationConfig {
    pub enabled: bool,
    pub descriptor_path: String,
    pub connector_name: String,
    pub max_queued_actions: usize,
    pub auto_approve_tool_calls: bool,
    pub automatic_retries: bool,
    pub retry_timings: RetryTimings,
    pub chatgpt_project_url: Option<String>,
    #[serde(deserialize_with = "deserialize_request_block_list")]
    pub request_block_list: Vec<String>,
    pub request_blocking_enabled: bool,
}

impl Default for AgentAutomationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            descriptor_path: DEFAULT_AUTOMATION_DESCRIPTOR_PATH.into(),
            connector_name: DEFAULT_AUTOMATION_CONNECTOR_NAME.into(),
            max_queued_actions: DEFAULT_AUTOMATION_MAX_QUEUED_ACTIONS,
            auto_approve_tool_calls: true,
            automatic_retries: true,
            retry_timings: RetryTimings::default(),
            chatgpt_project_url: None,
            request_block_list: vec![
                r"^https?://(www\.)?chatgpt\.com/.*/rgstr".into(),
                r"^https?://(www\.)?chatgpt\.com/backend-api/.*/conversations".into(),
                r"^https?://(www\.)?chatgpt\.com/backend-api/conversations".into(),
                r"^https?://(www\.)?chatgpt\.com/backend-api/sidebar".into(),
                r"^https?://(www\.)?chatgpt\.com/backend-api/.*/sidebar".into(),
            ],
            request_blocking_enabled: false,
        }
    }
}

impl AgentAutomationConfig {
    pub(crate) fn effective_request_block_list(&self) -> &[String] {
        if self.request_blocking_enabled {
            &self.request_block_list
        } else {
            &[]
        }
    }

    fn validate(&self) -> Result<(), String> {
        self.retry_timings.validate()?;
        if self.request_block_list.len() > 128 {
            return Err(
                "agentChat.automation.requestBlockList accepts at most 128 patterns".into(),
            );
        }
        for (index, pattern) in self.request_block_list.iter().enumerate() {
            if pattern.is_empty()
                || pattern.len() > 4096
                || !pattern.is_ascii()
                || pattern.chars().any(char::is_control)
            {
                return Err(format!(
                    "agentChat.automation.requestBlockList[{index}] must be 1 to 4096 printable ASCII characters"
                ));
            }
        }
        if let Some(project) = &self.chatgpt_project_url {
            let url = reqwest::Url::parse(project).map_err(
                |_| "agentChat.automation.chatgptProjectUrl must be a ChatGPT project URL",
            )?;
            let parts = url
                .path()
                .trim_end_matches('/')
                .split('/')
                .collect::<Vec<_>>();
            if project.len() > 4096
                || parts.get(2).is_none_or(|id| id.len() <= 4)
                || url.scheme() != "https"
                || url.host_str() != Some("chatgpt.com")
                || url.port().is_some()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || parts.len() != 4
                || parts[1] != "g"
                || !parts[2].starts_with("g-p-")
                || parts[3] != "project"
            {
                return Err("agentChat.automation.chatgptProjectUrl must be an HTTPS ChatGPT project URL ending in /project".into());
            }
        }
        if self.descriptor_path.trim().is_empty()
            || self.descriptor_path.len() > 4096
            || self.descriptor_path.contains('\0')
        {
            return Err(
                "agentChat.automation.descriptorPath must be a nonempty path up to 4096 bytes"
                    .into(),
            );
        }
        if self.connector_name.len() > 128 || self.connector_name.chars().any(char::is_control) {
            return Err(
                "agentChat.automation.connectorName must contain at most 128 visible characters"
                    .into(),
            );
        }
        if !(1..=1024).contains(&self.max_queued_actions) {
            return Err("agentChat.automation.maxQueuedActions must be between 1 and 1024".into());
        }
        Ok(())
    }
}

fn deserialize_request_block_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Vec<String>>::deserialize(deserializer).map(Option::unwrap_or_default)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageMode {
    Steer,
    #[default]
    Queue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct MarkdownChatConfig {
    pub enabled: bool,
    pub port: Option<u16>,
    pub max_wait_ms: u64,
    pub offline_timeout_minutes: u64,
    pub default_message_mode: MessageMode,
    pub rollout_reminder_interval: u64,
    pub rollout_enabled: bool,
    #[serde(rename = "analyticsEnabled", skip_serializing)]
    #[doc(hidden)]
    pub _legacy_analytics_enabled: Option<bool>,
    pub turn_renewal: crate::agent_automation::turn_renewal::TurnRenewalConfig,
    pub extras: ExtrasConfig,
    pub notifications: Option<NotificationsConfig>,
    pub automation: AgentAutomationConfig,
}

impl Default for MarkdownChatConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: Some(DEFAULT_OWNER_CHAT_PORT),
            max_wait_ms: DEFAULT_MAX_WAIT_MS,
            offline_timeout_minutes: DEFAULT_OFFLINE_TIMEOUT_MINUTES,
            default_message_mode: MessageMode::Queue,
            rollout_reminder_interval: rollout::DEFAULT_REMINDER_INTERVAL,
            rollout_enabled: true,
            _legacy_analytics_enabled: None,
            turn_renewal: Default::default(),
            extras: Default::default(),
            notifications: None,
            automation: AgentAutomationConfig::default(),
        }
    }
}

/// Optional presentation only. Statistics collection and sharing stay available.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ExtrasConfig {
    pub achievements: bool,
    pub animations: bool,
    pub sounds: bool,
    pub weekly_recap: bool,
    pub daily_streaks: bool,
    pub personal_records: bool,
}
impl Default for ExtrasConfig {
    fn default() -> Self {
        Self {
            achievements: true,
            animations: true,
            sounds: true,
            weekly_recap: true,
            daily_streaks: true,
            personal_records: true,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NotificationsConfig {
    pub urls: Vec<String>,
    #[serde(default = "default_notification_python")]
    pub python_path: String,
    #[serde(default = "default_notification_timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub events: NotificationEvents,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct NotificationEvents {
    pub messages: bool,
    pub completed: bool,
    pub input_required: bool,
    pub goal_blocked: bool,
    pub errors: bool,
    pub offline: bool,
}

impl Default for NotificationEvents {
    fn default() -> Self {
        Self {
            messages: true,
            completed: true,
            input_required: true,
            goal_blocked: true,
            errors: true,
            offline: true,
        }
    }
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
            .field("events", &self.events)
            .finish()
    }
}

impl NotificationsConfig {
    fn validate(&self) -> Result<(), String> {
        if self.urls.is_empty() || self.urls.len() > 32 {
            return Err(
                "agentChat.notifications.urls must contain 1 to 32 Apprise service URLs".into(),
            );
        }
        if self.urls.iter().any(|url| {
            url.len() > 16_384
                || !url.contains("://")
                || url.chars().any(char::is_control)
                || reqwest::Url::parse(url).is_err()
        }) {
            return Err("agentChat.notifications.urls contains an invalid service URL".into());
        }
        if self.python_path.trim().is_empty() || self.python_path.contains('\0') {
            return Err("agentChat.notifications.pythonPath must name a Python interpreter with Apprise installed".into());
        }
        if !(1_000..=60_000).contains(&self.timeout_ms) {
            return Err("agentChat.notifications.timeoutMs must be between 1000 and 60000".into());
        }
        Ok(())
    }
}

impl MarkdownChatConfig {
    pub fn offline_after_ms(&self) -> u64 {
        self.offline_timeout_minutes.saturating_mul(60_000)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(1..=1440).contains(&self.offline_timeout_minutes) {
            return Err(
                "agentChat.offlineTimeoutMinutes must be a whole number between 1 and 1440".into(),
            );
        }
        if self.port == Some(0) {
            return Err("agentChat.port must be between 1 and 65535, or null".into());
        }
        if !(1_000..=300_000).contains(&self.max_wait_ms) {
            return Err("agentChat.maxWaitMs must be between 1000 and 300000".into());
        }
        self.turn_renewal.validate()?;
        if !(1..=10000).contains(&self.rollout_reminder_interval) {
            return Err("agentChat.rolloutReminderInterval must be between 1 and 10000".into());
        }
        if let Some(notifications) = &self.notifications {
            notifications.validate()?;
        }
        self.automation.validate()?;
        Ok(())
    }
}

pub struct MarkdownChatStore {
    offline_after_ms: u64,
    channels: Mutex<HashMap<PathBuf, Arc<ChatFile>>>,
    activity: Mutex<HashMap<String, AgentActivityState>>,
    offline_monitors: Mutex<HashMap<PathBuf, Arc<tokio::sync::Notify>>>,
    transport_namespace: String,
}

#[derive(Clone)]
pub(crate) struct AgentActivity {
    pub at_ms: u64,
    pub epoch: String,
    pub sequence: u64,
}

#[derive(Default)]
struct AgentActivityState {
    last_call_at_ms: u64,
    sequence: u64,
}

impl Default for MarkdownChatStore {
    fn default() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            channels: Mutex::new(HashMap::new()),
            offline_after_ms: OFFLINE_AFTER_MS,
            activity: Mutex::new(HashMap::new()),
            offline_monitors: Mutex::new(HashMap::new()),
            transport_namespace: format!("{}-{timestamp}", std::process::id()),
        }
    }
}

impl MarkdownChatStore {
    pub(crate) fn with_offline_timeout(offline_after_ms: u64) -> Self {
        Self {
            offline_after_ms,
            ..Self::default()
        }
    }

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
    ) -> Option<AgentActivity> {
        let mut activity = self.activity.lock().ok()?;
        let state = activity
            .entry(self.owner(conversation, session))
            .or_default();
        state.last_call_at_ms = state.last_call_at_ms.max(at_ms);
        state.sequence = state.sequence.saturating_add(1);
        Some(AgentActivity {
            at_ms: state.last_call_at_ms,
            epoch: self.transport_namespace.clone(),
            sequence: state.sequence,
        })
    }

    pub(crate) fn agent_activity(
        &self,
        conversation: Option<&ConversationIdentity>,
        session: &SessionState,
    ) -> Option<AgentActivity> {
        let activity = self.activity.lock().ok()?;
        let state = activity.get(&self.owner(conversation, session))?;
        Some(AgentActivity {
            at_ms: state.last_call_at_ms,
            epoch: self.transport_namespace.clone(),
            sequence: state.sequence,
        })
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
        let path = crate::memory::task_state_dir(config)
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
        self.chat_at_path_with_timeout(
            path,
            conversation.is_some(),
            config.markdown_chat.offline_after_ms(),
        )
    }

    pub(crate) fn chat_at_path(
        &self,
        path: PathBuf,
        persistent: bool,
    ) -> Result<Arc<ChatFile>, String> {
        self.chat_at_path_with_timeout(path, persistent, self.offline_after_ms)
    }

    fn chat_at_path_with_timeout(
        &self,
        path: PathBuf,
        persistent: bool,
        offline_after_ms: u64,
    ) -> Result<Arc<ChatFile>, String> {
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "Markdown chat store is unavailable")?;
        let chat = channels
            .entry(path.clone())
            .or_insert_with(|| {
                let chat = ChatFile::new(path, persistent);
                chat.set_offline_timeout(offline_after_ms);
                Arc::new(chat)
            })
            .clone();
        // Configure existing channels too, before any new queue monitor can run.
        chat.set_offline_timeout(offline_after_ms);
        drop(channels);
        chat.start_queue_monitor();
        Ok(chat)
    }

    pub(crate) fn monitor_offline(
        &self,
        chat: Arc<ChatFile>,
        config: &MarkdownChatConfig,
        workspace: String,
    ) {
        if !notification::enabled(config, notification::NotificationEvent::Offline) {
            return;
        }
        let Ok(mut monitors) = self.offline_monitors.lock() else {
            tracing::warn!("could not start agent chat offline notification monitor");
            return;
        };
        let wake = monitors
            .entry(chat.path().to_path_buf())
            .or_insert_with(|| {
                let wake = Arc::new(tokio::sync::Notify::new());
                let config = config.clone();
                let monitor_wake = wake.clone();
                tokio::spawn(async move {
                    let offline_after_ms = config.offline_after_ms();
                    offline::monitor(chat, monitor_wake, offline_after_ms, move || {
                        let config = config.clone();
                        let workspace = workspace.clone();
                        async move {
                            notification::publish_event(
                                &config,
                                notification::NotificationEvent::Offline,
                                &workspace,
                                offline::message(offline_after_ms),
                                &tokio_util::sync::CancellationToken::new(),
                            )
                            .await
                        }
                    })
                    .await;
                });
                wake
            });
        wake.notify_one();
    }

    #[cfg(test)]
    pub(crate) fn offline_monitor_count(&self) -> usize {
        self.offline_monitors.lock().unwrap().len()
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
        .chat(config, context.task_conversation.as_ref(), session)?;
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
        return Err("Only this Codexify task's CHAT.md is available through chat history reads. Use chat_read for new messages.".into());
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
    fn retry_timings_default_and_partial_configuration() {
        let defaults = AgentAutomationConfig::default();
        let omitted: AgentAutomationConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(omitted.retry_timings, defaults.retry_timings);
        let custom: AgentAutomationConfig = serde_json::from_value(serde_json::json!({
            "retryTimings": {"chatgptAccessDelaysSeconds":[12, 48], "jitterPercent":0}
        }))
        .unwrap();
        custom.validate().unwrap();
        assert_eq!(custom.retry_timings.chatgpt_access_delays_seconds, [12, 48]);
        assert_eq!(custom.retry_timings.jitter_percent, 0);
        assert_eq!(
            custom.retry_timings.action_delays_seconds,
            defaults.retry_timings.action_delays_seconds
        );
        assert_eq!(custom.retry_timings.conversation_load_interval_seconds, 180);
    }

    #[test]
    fn retry_timings_validate_lists_units_bounds_and_unknown_fields() {
        for key in [
            "actionDelaysSeconds",
            "chatgptAccessDelaysSeconds",
            "goalRecoveryDelaysSeconds",
        ] {
            for invalid in [
                serde_json::json!([]),
                serde_json::json!([0]),
                serde_json::json!([86401]),
                serde_json::json!(vec![1; 33]),
            ] {
                let config: AgentAutomationConfig =
                    serde_json::from_value(serde_json::json!({"retryTimings":{key:invalid}}))
                        .unwrap();
                assert!(config.validate().unwrap_err().contains(key));
            }
            for invalid in [
                serde_json::json!([-1]),
                serde_json::json!([1.5]),
                serde_json::json!(["3m"]),
            ] {
                assert!(
                    serde_json::from_value::<AgentAutomationConfig>(
                        serde_json::json!({"retryTimings":{key:invalid}})
                    )
                    .is_err()
                );
            }
        }
        for key in [
            "approvalResumeGraceSeconds",
            "conversationLoadIntervalSeconds",
            "conversationUnavailableCooldownSeconds",
            "rateLimitCooldownSeconds",
            "jitterPercent",
        ] {
            for invalid in [if key == "jitterPercent" { 101 } else { 0 }, 86401] {
                let config: AgentAutomationConfig =
                    serde_json::from_value(serde_json::json!({"retryTimings":{key:invalid}}))
                        .unwrap();
                assert!(config.validate().unwrap_err().contains(key));
            }
        }
        assert!(
            serde_json::from_value::<AgentAutomationConfig>(
                serde_json::json!({"retryTimings":{"unknownDelay":42}})
            )
            .is_err()
        );
    }

    #[test]
    fn request_block_list_defaults_and_configuration_bounds() {
        for (value, expected_len) in [
            (serde_json::json!({}), 5),
            (serde_json::json!({"requestBlockList":null}), 0),
        ] {
            let config: AgentAutomationConfig = serde_json::from_value(value).unwrap();
            assert_eq!(config.request_block_list.len(), expected_len);
            assert!(!config.request_blocking_enabled);
            assert!(config.effective_request_block_list().is_empty());
        }
        let mut config: AgentAutomationConfig = serde_json::from_value(
            serde_json::json!({"requestBlockList":["^https?://example\\.com/"]}),
        )
        .unwrap();
        assert!(config.validate().is_ok());
        config.request_blocking_enabled = true;
        assert_eq!(
            config.effective_request_block_list(),
            config.request_block_list.as_slice()
        );
        for pattern in ["".to_string(), "é".to_string(), "x".repeat(4097)] {
            config.request_block_list = vec![pattern];
            assert!(config.validate().is_err());
        }
        config.request_block_list = vec!["x".into(); 129];
        assert!(config.validate().is_err());
        assert!(
            serde_json::from_value::<AgentAutomationConfig>(
                serde_json::json!({"requestBlockList":[1]})
            )
            .is_err()
        );
    }

    #[test]
    fn new_chat_project_configuration_is_optional_and_validated() {
        assert!(AgentAutomationConfig::default().validate().is_ok());
        for (url, valid) in [
            (
                "https://chatgpt.com/g/g-p-0123456789abcdef0123456789abcdef/project",
                true,
            ),
            (
                "https://chatgpt.com/g/g-p-0123456789abcdef0123456789abcdef-example/project/",
                true,
            ),
            ("https://example.com/g/g-p-id/project", false),
            ("http://chatgpt.com/g/g-p-id/project", false),
            ("https://chatgpt.com/c/id", false),
            ("https://chatgpt.com/g/g-p-id/project?token=private", false),
            ("https://user:secret@chatgpt.com/g/g-p-id/project", false),
        ] {
            let config = AgentAutomationConfig {
                chatgpt_project_url: Some(url.into()),
                ..AgentAutomationConfig::default()
            };
            assert_eq!(config.validate().is_ok(), valid);
        }
    }

    #[test]
    fn activity_is_conversation_scoped_with_transport_fallback() {
        let store = MarkdownChatStore::default();
        let first = SessionState::new();
        let second = SessionState::new();
        let a = ConversationIdentity::from_openai_session("a").unwrap();
        let b = ConversationIdentity::from_openai_session("b").unwrap();
        let first_call = store.record_agent_call(Some(&a), &first, 1000).unwrap();
        let second_call = store.record_agent_call(Some(&a), &second, 800).unwrap();
        assert_eq!(first_call.sequence, 1);
        assert_eq!(second_call.sequence, 2);
        let shared = store.agent_activity(Some(&a), &second).unwrap();
        assert_eq!(shared.at_ms, 1000);
        assert_eq!(shared.sequence, 2);
        assert!(store.agent_activity(Some(&b), &first).is_none());
        assert!(store.agent_activity(None, &first).is_none());
        let transport = store.record_agent_call(None, &first, 1500).unwrap();
        assert_eq!(transport.at_ms, 1500);
        assert_eq!(transport.sequence, 1);
        assert!(store.agent_activity(None, &second).is_none());
    }

    #[tokio::test]
    async fn no_offline_monitor_is_started_without_notifications() {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().to_path_buf());
        config.markdown_chat.enabled = true;
        let store = MarkdownChatStore::default();
        let session = SessionState::new();
        let chat = store.chat(&config, None, &session).unwrap();

        store.monitor_offline(chat, &config.markdown_chat, "test".into());

        assert_eq!(store.offline_monitor_count(), 0);
    }
}
