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
pub const OFFLINE_AFTER_MS: u64 = 300_000;
pub const AUTOMATION_OFFLINE_AFTER_MS: u64 = OFFLINE_AFTER_MS - 60_000;
pub const PAIRING_PRESENCE_GRACE_MS: u64 = 5_000;
pub const DEFAULT_AUTOMATION_DESCRIPTOR_PATH: &str = "~/.codexify/browser/descriptor.json";
pub const DEFAULT_AUTOMATION_CONNECTOR_NAME: &str = "";
pub const DEFAULT_AUTOMATION_MAX_QUEUED_ACTIONS: usize = 64;

pub(crate) const MAX_AUTOMATIC_RETRY_ATTEMPTS: usize = 32;
pub(crate) const MAX_RETRY_DELAY_SECONDS: u64 = 86_400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct RetryTimings {
    pub infinite_retries: bool,
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
            infinite_retries: false,
            action_delays_seconds: vec![30, 60, 120, 240, 480],
            chatgpt_access_delays_seconds: vec![180, 300, 600, 1200, 1800, 3600],
            goal_recovery_delays_seconds: vec![60, 120, 240],
            jitter_percent: 10,
            approval_resume_grace_seconds: 180,
            conversation_load_interval_seconds: 1,
            conversation_unavailable_cooldown_seconds: 300,
            rate_limit_cooldown_seconds: 180,
        }
    }
}

impl RetryTimings {
    pub(crate) fn delay_at(&self, schedule: &[u64], attempt: u32) -> Option<u64> {
        schedule.get(attempt as usize).copied().or_else(|| {
            self.infinite_retries
                .then(|| schedule.last().copied())
                .flatten()
        })
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserSimplicity {
    Off,
    Simplified,
    Bones,
}

impl BrowserSimplicity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Simplified => "simplified",
            Self::Bones => "bones",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentAutomationConfig {
    pub enabled: bool,
    pub descriptor_path: String,
    pub connector_name: String,
    pub max_queued_actions: usize,
    pub browser_action_lanes: usize,
    pub auto_approve_tool_calls: bool,
    pub automatic_retries: bool,
    pub retry_timings: RetryTimings,
    pub chatgpt_project_url: Option<String>,
    #[serde(deserialize_with = "deserialize_request_block_list")]
    pub request_block_list: Vec<String>,
    pub request_blocking_enabled: bool,
    pub minimal_ui: bool,
    /// None retains the older minimalUi boolean's exact behavior.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_simplicity: Option<BrowserSimplicity>,
    pub recovery_enhancements: bool,
}

impl Default for AgentAutomationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            descriptor_path: DEFAULT_AUTOMATION_DESCRIPTOR_PATH.into(),
            connector_name: DEFAULT_AUTOMATION_CONNECTOR_NAME.into(),
            max_queued_actions: DEFAULT_AUTOMATION_MAX_QUEUED_ACTIONS,
            browser_action_lanes: 2,
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
            minimal_ui: false,
            browser_simplicity: None,
            recovery_enhancements: false,
        }
    }
}

impl AgentAutomationConfig {
    pub fn effective_browser_simplicity(&self) -> BrowserSimplicity {
        self.browser_simplicity.unwrap_or(if self.minimal_ui {
            BrowserSimplicity::Simplified
        } else {
            BrowserSimplicity::Off
        })
    }

    pub(crate) fn effective_request_block_list(&self) -> &[String] {
        if self.request_blocking_enabled {
            &self.request_block_list
        } else {
            &[]
        }
    }

    fn validate(&self) -> Result<(), String> {
        self.retry_timings.validate()?;
        if !(1..=4).contains(&self.browser_action_lanes) {
            return Err("agentChat.automation.browserActionLanes must be between 1 and 4".into());
        }
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
    pub features: ChatFeatures,
}

impl Default for MarkdownChatConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: Some(DEFAULT_OWNER_CHAT_PORT),
            max_wait_ms: DEFAULT_MAX_WAIT_MS,
            default_message_mode: MessageMode::Queue,
            rollout_reminder_interval: rollout::DEFAULT_REMINDER_INTERVAL,
            rollout_enabled: true,
            _legacy_analytics_enabled: None,
            turn_renewal: Default::default(),
            extras: Default::default(),
            notifications: None,
            automation: AgentAutomationConfig::default(),
            features: ChatFeatures::default(),
        }
    }
}

/// Feature gates stay opt-in; enabled agent teams may delegate by default.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ChatFeatures {
    pub agent_teams: bool,
    pub default_delegation: bool,
    pub external_agent_control: bool,
    pub auto_mode: bool,
    pub agent_folders: bool,
    pub conversation_recovery: bool,
}
impl Default for ChatFeatures {
    fn default() -> Self {
        Self {
            agent_teams: false,
            default_delegation: true,
            external_agent_control: false,
            auto_mode: false,
            agent_folders: false,
            conversation_recovery: false,
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
    pub fn validate(&self) -> Result<(), String> {
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
            activity: Mutex::new(HashMap::new()),
            offline_monitors: Mutex::new(HashMap::new()),
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
        self.chat_at_path(path, conversation.is_some())
    }

    pub(crate) fn chat_at_path(
        &self,
        path: PathBuf,
        persistent: bool,
    ) -> Result<Arc<ChatFile>, String> {
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "Markdown chat store is unavailable")?;
        let chat = channels
            .entry(path.clone())
            .or_insert_with(|| Arc::new(ChatFile::new(path, persistent)))
            .clone();
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
                    offline::monitor(chat, monitor_wake, OFFLINE_AFTER_MS, move || {
                        let config = config.clone();
                        let workspace = workspace.clone();
                        async move {
                            notification::publish_event(
                                &config,
                                notification::NotificationEvent::Offline,
                                &workspace,
                                offline::MESSAGE.to_string(),
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
    fn browser_simplicity_levels_preserve_legacy_choices_and_validate_values() {
        for (value, expected) in [
            (serde_json::json!({}), BrowserSimplicity::Off),
            (serde_json::json!({"minimalUi":false}), BrowserSimplicity::Off),
            (serde_json::json!({"minimalUi":true}), BrowserSimplicity::Simplified),
            (serde_json::json!({"minimalUi":true,"browserSimplicity":"off"}), BrowserSimplicity::Off),
            (serde_json::json!({"minimalUi":false,"browserSimplicity":"bones"}), BrowserSimplicity::Bones),
            (serde_json::json!({"browserSimplicity":"simplified"}), BrowserSimplicity::Simplified),
        ] {
            let config: AgentAutomationConfig = serde_json::from_value(value).unwrap();
            assert_eq!(config.effective_browser_simplicity(), expected);
            let restored: AgentAutomationConfig = serde_json::from_value(serde_json::to_value(config).unwrap()).unwrap();
            assert_eq!(restored.effective_browser_simplicity(), expected);
        }
        for invalid in [serde_json::json!(true), serde_json::json!("fast"), serde_json::json!(3), serde_json::json!({})] {
            assert!(serde_json::from_value::<AgentAutomationConfig>(serde_json::json!({"browserSimplicity":invalid})).is_err());
        }
    }

    #[test]
    fn added_features_are_opt_in_for_new_and_partial_configurations() {
        for value in [serde_json::json!({}), serde_json::json!({"features":{},"automation":{"retryTimings":{}}})] {
            let config: MarkdownChatConfig = serde_json::from_value(value).unwrap();
            assert_eq!(config.features, ChatFeatures::default());
            assert!(config.features.default_delegation);
            assert!(serde_json::to_value(&config.features).unwrap().as_object().unwrap().iter().filter(|(key,_)|key.as_str()!="defaultDelegation").all(|(_,value)|value == false));
            assert!(!config.automation.minimal_ui);
            assert!(!config.automation.recovery_enhancements);
            assert!(!config.automation.retry_timings.infinite_retries);
            assert!(!config.automation.request_blocking_enabled);
        }
        let mut config: MarkdownChatConfig = serde_json::from_value(serde_json::json!({
            "features":{"agentTeams":true},"automation":{"minimalUi":true}
        })).unwrap();
        assert!(config.features.agent_teams && config.automation.minimal_ui);
        assert!(!config.features.auto_mode && !config.features.conversation_recovery);
        config.features.agent_teams = false;
        assert!(!serde_json::from_value::<MarkdownChatConfig>(serde_json::to_value(config).unwrap()).unwrap().features.agent_teams);
        let disabled: MarkdownChatConfig=serde_json::from_value(serde_json::json!({"features":{"agentTeams":true,"defaultDelegation":false}})).unwrap();
        assert!(!disabled.features.default_delegation);
        let saved=serde_json::to_value(disabled).unwrap();
        assert_eq!(saved["features"]["defaultDelegation"],false);
        assert!(!serde_json::from_value::<MarkdownChatConfig>(saved).unwrap().features.default_delegation);
    }
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
        assert_eq!(custom.retry_timings.conversation_load_interval_seconds, 1);
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
    fn browser_action_lanes_default_to_two_and_are_bounded() {
        assert_eq!(AgentAutomationConfig::default().browser_action_lanes,2);
        for lanes in [0,1,2,4,5] {
            let config:AgentAutomationConfig=serde_json::from_value(serde_json::json!({"browserActionLanes":lanes})).unwrap();
            assert_eq!(config.validate().is_ok(),(1..=4).contains(&lanes));
        }
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
