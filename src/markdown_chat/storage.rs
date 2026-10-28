use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{MAX_UNREAD_BYTES, notification::NotificationEvent};

#[path = "queue.rs"]
mod queue;
#[path = "rollout.rs"]
pub mod rollout;
#[path = "widget.rs"]
mod widget;
pub use queue::{QueueReceipt, QueueSubmission, QueuedMessage};
pub use widget::{
    AGENT_STOP_MESSAGE, BrowserAppendReceipt, OwnerChatOrganization, OwnerChatSummary,
    TranscriptCheckpoint, UnreadMessage, UnreadSnapshot, UserSendReceipt, WidgetPage,
};

const HEADER: &str = "# Codexify Chat\n\nAppend user messages at the bottom and save the file. Do not change earlier content\nwhile the agent is active. The agent sends messages only through chat_write.\n";
const AGENT_START: &str = "\n\n<!-- codexify-agent-message:v1:start id=\"";
const WARNING_START: &str = "\n\n<!-- codexify-warning-message:v1:start id=\"";
const ANCHOR_BYTES: usize = 64;
const MAX_CURSOR_BYTES: u64 = 4096;
static MESSAGE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotificationState {
    #[default]
    NotConfigured,
    Pending,
    Accepted,
    Failed,
    Cancelled,
    Suppressed,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Cursor {
    offset: u64,
    anchor: Vec<u8>,
    #[serde(default)]
    custom_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pinned_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archived_at_ms: Option<u64>,
    // Owner UI read position, independent of the agent's input/delivery cursor.
    #[serde(default)]
    owner_read_through: u64,
    #[serde(default)]
    last_agent_end: Option<u64>,
    #[serde(default)]
    notification: NotificationState,
    #[serde(default)]
    last_message_event: NotificationEvent,
    #[serde(default)]
    input_notification_pending: bool,
    #[serde(default)]
    notification_claims: BTreeMap<String, String>,
    #[serde(default)]
    delivered_through: u64,
    #[serde(default)]
    last_agent_call_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_presence_until_ms: Option<u64>,
    #[serde(default)]
    agent_stop_requested: bool,
    #[serde(default)]
    agent_stop_requests: Vec<String>,
    #[serde(default)]
    offline_notified_for_tool_call_count: Option<u64>,
    #[serde(default)]
    offline_claims: BTreeMap<String, u64>,
    #[serde(default)]
    total_tool_calls: u64,
    #[serde(default)]
    tool_call_epoch: Option<String>,
    #[serde(default)]
    tool_call_sequence: u64,
    // Consume the old flag on read, then omit it from future cursor writes.
    #[serde(default, rename = "ticketRejectionNotified", skip_serializing)]
    _legacy_ticket_rejection_notified: bool,
}

pub struct ChatSnapshot {
    pub text: String,
    pub notification: NotificationState,
    pub end: u64,
}

pub struct AppendReceipt {
    pub user_text: String,
    pub end_offset: u64,
    pub cursor_warning: Option<String>,
}

#[derive(Clone)]
pub(crate) struct OfflineActivity {
    pub(crate) agent_stop_requested: bool,
    pub(crate) last_call_at_ms: Option<u64>,
    pub(crate) agent_presence_until_ms: Option<u64>,
    pub(crate) agent_waiting_until_ms: Option<u64>,
    pub(crate) tool_call_count: u64,
    pub(crate) activity_epoch: Option<String>,
    pub(crate) activity_sequence: u64,
    pub(crate) notified_for_tool_call_count: Option<u64>,
}

impl OfflineActivity {
    pub(crate) fn is_online(&self, now_ms: u64, offline_after_ms: u64) -> bool {
        self.agent_presence_until_ms.map_or_else(
            || {
                self.last_call_at_ms
                    .is_some_and(|last| now_ms.saturating_sub(last) < offline_after_ms)
            },
            |until| now_ms < until,
        )
    }
    pub(crate) fn last_call_for_presence(&self, now_ms: u64) -> Option<u64> {
        self.agent_presence_until_ms
            .filter(|until| now_ms >= *until)
            .map_or(self.last_call_at_ms, |_| None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OfflineClaim {
    Notification,
    AutomationGoal,
}

impl OfflineClaim {
    fn key(self) -> &'static str {
        match self {
            Self::Notification => "notification",
            Self::AutomationGoal => "automation_goal",
        }
    }
}

pub struct ChatFile {
    offline_after_ms: AtomicU64,
    path: PathBuf,
    cursor_path: Option<PathBuf>,
    cursor: Mutex<Option<Cursor>>,
    owner_summary_cache: Mutex<Option<OwnerSummaryCache>>,
    pub(super) waiting: Mutex<super::wait::AgentWaiting>,
    queue_monitor_started: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
struct OwnerSummaryCache {
    length: u64,
    modified: std::time::SystemTime,
    title: String,
    last_entry_end: u64,
    last_incoming_entry_end: u64,
    last_entry_at_ms: u64,
    last_user_entry_end: u64,
    last_user_entry_at_ms: Option<u64>,
}

impl ChatFile {
    pub(super) fn new(path: PathBuf, persistent: bool) -> Self {
        let cursor_path = persistent.then(|| path.with_file_name("cursor.json"));
        Self {
            path,
            offline_after_ms: AtomicU64::new(super::OFFLINE_AFTER_MS),
            cursor_path,
            cursor: Mutex::new(None),
            owner_summary_cache: Mutex::new(None),
            waiting: Mutex::new(super::wait::AgentWaiting::default()),
            queue_monitor_started: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn set_offline_timeout(&self, offline_after_ms: u64) {
        self.offline_after_ms
            .store(offline_after_ms, Ordering::Relaxed);
    }

    pub(crate) fn offline_after_ms(&self) -> u64 {
        self.offline_after_ms.load(Ordering::Relaxed)
    }

    pub async fn set_owner_title(self: &Arc<Self>, title: String) -> Result<(), String> {
        let title = title.trim().to_string();
        if title.chars().count() > 200 {
            return Err("Chat name must not exceed 200 characters".into());
        }
        let has_text = !super::title::plain_text(&title, 200).is_empty();
        self.run(move |chat| {
            chat.with_cursor(|cursor, _| {
                // Saving an unchanged displayed name must preserve literal Markdown
                // characters originally supplied through code spans or escapes.
                if has_text
                    && cursor
                        .custom_title
                        .as_deref()
                        .is_some_and(|current| super::title::plain_text(current, 200) == title)
                {
                    return Ok(());
                }
                let mut next = cursor.clone();
                next.custom_title = has_text.then_some(title);
                chat.save_cursor(&next)?;
                *cursor = next;
                Ok(())
            })
        })
        .await
    }

    async fn run<T, F>(self: &Arc<Self>, operation: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&Self) -> Result<T, String> + Send + 'static,
    {
        let chat = self.clone();
        tokio::task::spawn_blocking(move || operation(&chat))
            .await
            .map_err(|_| "Markdown chat file operation was interrupted".to_string())?
    }

    pub async fn ensure(self: &Arc<Self>) -> Result<(), String> {
        self.run(|chat| chat.with_cursor(|_, _| Ok(()))).await
    }

    pub async fn read(self: &Arc<Self>, consume: bool) -> Result<ChatSnapshot, String> {
        self.read_cancellable(consume, CancellationToken::new())
            .await
    }

    pub async fn read_cancellable(
        self: &Arc<Self>,
        consume: bool,
        cancellation: CancellationToken,
    ) -> Result<ChatSnapshot, String> {
        self.run(move |chat| {
            chat.with_cursor(|cursor, file| {
                check_schema_handoff(chat)?;
                if cancellation.is_cancelled() {
                    return Err(
                        "Markdown chat read was cancelled; no messages were consumed".into(),
                    );
                }
                let snapshot = snapshot(chat, file, cursor)?;
                if !snapshot.text.is_empty() {
                    chat.clear_agent_waiting();
                }
                if consume && snapshot.end != cursor.offset {
                    if cancellation.is_cancelled() {
                        return Err(
                            "Markdown chat read was cancelled; no messages were consumed".into(),
                        );
                    }
                    let mut next = advanced_cursor(file, cursor.clone(), snapshot.end)?;
                    if !snapshot.text.is_empty() {
                        next.input_notification_pending = false;
                    }
                    chat.save_cursor(&next)?;
                    *cursor = next;
                }
                Ok(snapshot)
            })
        })
        .await
    }

    pub async fn append(self: &Arc<Self>, message: String) -> Result<AppendReceipt, String> {
        self.run(move |chat| chat.append_sync(&message, || {}))
            .await
    }

    pub async fn append_with_event(
        self: &Arc<Self>,
        message: String,
        event: NotificationEvent,
    ) -> Result<AppendReceipt, String> {
        self.run(move |chat| chat.append_sync_with_event(&message, event, || {}))
            .await
    }

    pub(crate) async fn claim_input_notification(self: &Arc<Self>) -> Result<bool, String> {
        self.run(|chat| {
            chat.with_cursor(|cursor, file| {
                if cursor.input_notification_pending
                    || cursor.agent_stop_requested
                    || cursor.last_message_event == NotificationEvent::Completed
                    || cursor.last_agent_end.is_none()
                    || !snapshot(chat, file, cursor)?.text.is_empty()
                {
                    return Ok(false);
                }
                let mut next = cursor.clone();
                next.input_notification_pending = true;
                chat.save_cursor(&next)?;
                *cursor = next;
                Ok(true)
            })
        })
        .await
    }

    pub(crate) async fn claim_notification(
        self: &Arc<Self>,
        category: &str,
        key: &str,
    ) -> Result<bool, String> {
        let category = category.to_owned();
        let key = key.to_owned();
        self.run(move |chat| {
            chat.with_cursor(|cursor, _| {
                if cursor.notification_claims.get(&category) == Some(&key) {
                    return Ok(false);
                }
                let mut next = cursor.clone();
                next.notification_claims.insert(category, key);
                chat.save_cursor(&next)?;
                *cursor = next;
                Ok(true)
            })
        })
        .await
    }

    pub(crate) async fn clear_notification_claim(
        self: &Arc<Self>,
        category: &str,
    ) -> Result<(), String> {
        let category = category.to_owned();
        self.run(move |chat| {
            chat.with_cursor(|cursor, _| {
                if !cursor.notification_claims.contains_key(&category) {
                    return Ok(());
                }
                let mut next = cursor.clone();
                next.notification_claims.remove(&category);
                chat.save_cursor(&next)?;
                *cursor = next;
                Ok(())
            })
        })
        .await
    }

    pub(crate) async fn warn_ticket_rejection(self: &Arc<Self>) -> Result<(), String> {
        self.run(|chat| {
            chat.with_cursor(|cursor, file| {
                check_cursor(file, cursor)?;
                let block = warning_block(crate::agent_tickets::WARNING, cursor.total_tool_calls);
                file.write_all(block.as_bytes()).map_err(io_error)?;
                file.sync_all().map_err(io_error)?;
                // A server notice must not acknowledge user text on behalf of the winning branch.
                Ok(())
            })
        })
        .await
    }

    pub async fn set_notification(
        self: &Arc<Self>,
        end_offset: u64,
        notification: NotificationState,
    ) -> Result<(), String> {
        self.run(move |chat| {
            chat.with_cursor(|cursor, _| {
                if cursor.last_agent_end == Some(end_offset) {
                    let mut next = cursor.clone();
                    next.notification = notification;
                    chat.save_cursor(&next)?;
                    *cursor = next;
                }
                Ok(())
            })
        })
        .await
    }

    pub(crate) async fn offline_activity(self: &Arc<Self>) -> Result<OfflineActivity, String> {
        self.run(|chat| {
            chat.with_cursor(|cursor, _| {
                Ok(OfflineActivity {
                    agent_stop_requested: cursor.agent_stop_requested,
                    last_call_at_ms: cursor.last_agent_call_at_ms,
                    agent_presence_until_ms: cursor.agent_presence_until_ms,
                    agent_waiting_until_ms: chat.agent_waiting_until_ms(),
                    tool_call_count: cursor.total_tool_calls,
                    activity_epoch: cursor.tool_call_epoch.clone(),
                    activity_sequence: cursor.tool_call_sequence,
                    notified_for_tool_call_count: cursor.offline_notified_for_tool_call_count,
                })
            })
        })
        .await
    }

    pub(super) async fn claim_offline_notification(
        self: &Arc<Self>,
        now_ms: u64,
        offline_after_ms: u64,
    ) -> Result<bool, String> {
        self.claim_offline(OfflineClaim::Notification, now_ms, offline_after_ms)
            .await
    }

    pub async fn claim_offline_automation_goal(
        self: &Arc<Self>,
        now_ms: u64,
        offline_after_ms: u64,
    ) -> Result<bool, String> {
        self.claim_offline(OfflineClaim::AutomationGoal, now_ms, offline_after_ms)
            .await
    }

    async fn claim_offline(
        self: &Arc<Self>,
        claim: OfflineClaim,
        now_ms: u64,
        offline_after_ms: u64,
    ) -> Result<bool, String> {
        self.run(move |chat| {
            chat.with_cursor(|cursor, _| {
                if cursor.agent_stop_requested {
                    return Ok(false);
                }
                let Some(last_call_at_ms) = cursor.last_agent_call_at_ms else {
                    return Ok(false);
                };
                if claim == OfflineClaim::Notification && cursor.agent_presence_until_ms.is_some() {
                    // Pairing verification is setup, not a working turn going silent.
                    return Ok(false);
                }
                let claimed_for = cursor.offline_claims.get(claim.key()).copied().or_else(|| {
                    (claim == OfflineClaim::Notification)
                        .then_some(cursor.offline_notified_for_tool_call_count)
                        .flatten()
                });
                if now_ms
                    < cursor
                        .agent_presence_until_ms
                        .unwrap_or_else(|| last_call_at_ms.saturating_add(offline_after_ms))
                    || claimed_for == Some(cursor.total_tool_calls)
                {
                    return Ok(false);
                }
                let mut next = cursor.clone();
                next.offline_claims
                    .insert(claim.key().into(), cursor.total_tool_calls);
                if claim == OfflineClaim::Notification {
                    next.offline_notified_for_tool_call_count = Some(cursor.total_tool_calls);
                }
                chat.save_cursor(&next)?;
                *cursor = next;
                Ok(true)
            })
        })
        .await
    }

    fn with_cursor<T>(
        &self,
        operation: impl FnOnce(&mut Cursor, &mut File) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut state = self
            .cursor
            .lock()
            .map_err(|_| "Markdown chat cursor is unavailable")?;
        let directory = self
            .path
            .parent()
            .ok_or("CHAT.md has no parent directory")?;
        private_directory(directory.parent().ok_or("Chat directory has no parent")?)?;
        private_directory(directory)?;
        if let Err(error) = rollout::ensure(self) {
            tracing::warn!(%error, "could not initialize this chat's progress file");
        }
        let has_cursor = self.cursor_path.as_ref().is_some_and(|path| path.exists());
        let mut file = match std::fs::symlink_metadata(&self.path) {
            Ok(_) => open_regular(&self.path, true)?,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && state.is_none()
                    && !has_cursor =>
            {
                let mut options = OpenOptions::new();
                options.read(true).append(true).create_new(true);
                private_options(&mut options);
                match options.open(&self.path) {
                    Ok(mut file) => {
                        file.write_all(HEADER.as_bytes()).map_err(io_error)?;
                        file.sync_all().map_err(io_error)?;
                        file
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        open_regular(&self.path, true)?
                    }
                    Err(error) => return Err(io_error(error)),
                }
            }
            Err(error) => {
                return Err(format!(
                    "CHAT.md is missing or unavailable: {error}. Restore the transcript before continuing."
                ));
            }
        };
        if state.is_none() {
            let cursor = match self.cursor_path.as_ref().filter(|_| has_cursor) {
                Some(path) => {
                    let mut cursor_file = open_regular(path, false)?;
                    if cursor_file.metadata().map_err(io_error)?.len() > MAX_CURSOR_BYTES {
                        return Err("Markdown chat cursor is oversized".into());
                    }
                    let mut bytes = Vec::new();
                    cursor_file.read_to_end(&mut bytes).map_err(io_error)?;
                    let cursor: Cursor = serde_json::from_slice(&bytes)
                        .map_err(|_| "Markdown chat cursor is invalid; restore cursor.json rather than discarding unread messages")?;
                    if cursor.anchor.len() > ANCHOR_BYTES
                        || cursor.anchor.len() as u64 > cursor.offset
                    {
                        return Err("Markdown chat cursor has invalid boundaries".into());
                    }
                    cursor
                }
                None => {
                    let length = file.metadata().map_err(io_error)?.len();
                    let prefix =
                        read_range(&mut file, 0, length.min(HEADER.len() as u64), HEADER.len())?;
                    let end = if prefix == HEADER.as_bytes() {
                        HEADER.len() as u64
                    } else {
                        0
                    };
                    let cursor = advanced_cursor(&mut file, Cursor::default(), end)?;
                    self.save_cursor(&cursor)?;
                    cursor
                }
            };
            *state = Some(cursor);
        }
        operation(state.as_mut().expect("cursor initialized"), &mut file)
    }

    fn save_cursor(&self, cursor: &Cursor) -> Result<(), String> {
        let Some(path) = &self.cursor_path else {
            return Ok(());
        };
        if std::fs::symlink_metadata(path).is_ok() {
            let _ = open_regular(path, false)?;
        }
        let mut temporary =
            tempfile::NamedTempFile::new_in(path.parent().ok_or("Cursor has no parent")?)
                .map_err(io_error)?;
        serde_json::to_writer(&mut temporary, cursor)
            .map_err(|_| "Could not serialize Markdown chat cursor")?;
        temporary.as_file().sync_all().map_err(io_error)?;
        temporary
            .persist(path)
            .map_err(|error| io_error(error.error))?;
        Ok(())
    }

    fn append_sync(
        &self,
        message: &str,
        before_append: impl FnOnce(),
    ) -> Result<AppendReceipt, String> {
        self.append_sync_with_event(message, NotificationEvent::Message, before_append)
    }

    fn append_sync_with_event(
        &self,
        message: &str,
        event: NotificationEvent,
        before_append: impl FnOnce(),
    ) -> Result<AppendReceipt, String> {
        if message.is_empty() {
            return Err("chat_write.message must not be empty".into());
        }
        if message.len() > MAX_UNREAD_BYTES {
            return Err(
                "Agent message exceeds the 16 MiB safety ceiling; nothing was appended".into(),
            );
        }
        self.with_cursor(|cursor, file| {
            check_schema_handoff(self)?;
            let before = snapshot(self, file, cursor)?;
            let block = agent_block(message, cursor.total_tool_calls);
            before_append();
            file.write_all(block.as_bytes()).map_err(io_error)?;
            let end = file.stream_position().map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
            let same_file = same_file::Handle::from_file(file.try_clone().map_err(io_error)?)
                .and_then(|handle| same_file::Handle::from_path(&self.path).map(|current| handle == current))
                .map_err(io_error)?;
            if !same_file {
                return Err("CHAT.md was replaced during chat_write; the cursor was not advanced. Read the current file before retrying.".into());
            }
            let start = end.checked_sub(block.len() as u64).ok_or("Invalid chat append position")?;
            if start < before.end || read_range(file, start, end, block.len())? != block.as_bytes() {
                return Err("CHAT.md changed during the agent append; no user messages were consumed. Restore an append-only transcript before retrying.".into());
            }
            check_cursor(file, cursor)?;
            // The file descriptor's write position excludes later user appends; EOF does not.
            let gap = read_range(file, before.end, start, MAX_UNREAD_BYTES.saturating_sub(before.text.len()))?;
            let gap = String::from_utf8(gap).map_err(|_| "CHAT.md contains incomplete UTF-8; finish saving before retrying")?;
            let user_text = before.text + &user_text(&gap, &self.path)?;
            // A settings edit can begin a handoff while the write is in flight.
            // Preserve the unread cursor even when the agent text was appended.
            check_schema_handoff(self)?;
            if !user_text.is_empty() {
                self.clear_agent_waiting();
            }
            let mut next = advanced_cursor(file, cursor.clone(), end)?;
            next.last_agent_end = Some(end);
            next.notification = NotificationState::NotConfigured;
            next.last_message_event = event;
            if !user_text.is_empty() { next.input_notification_pending = false; }
            let cursor_warning = self.save_cursor(&next).err();
            *cursor = next;
            Ok(AppendReceipt { user_text, end_offset: end, cursor_warning })
        })
    }
}

fn agent_block(message: &str, tool_call_count: u64) -> String {
    let id = format!(
        "{}-{}",
        chrono::Utc::now().timestamp_micros(),
        MESSAGE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    agent_block_with_id(&id, message, tool_call_count, super::now_ms())
}

fn agent_block_with_id(
    id: &str,
    message: &str,
    tool_call_count: u64,
    created_at_ms: u64,
) -> String {
    format!(
        "{AGENT_START}{id}\" created_at_ms=\"{created_at_ms}\" tool_call_count=\"{tool_call_count}\" -->\n\n## Agent\n\n{message}\n\n<!-- codexify-agent-message:v1:end id=\"{id}\" -->\n"
    )
}

fn warning_block(message: &str, tool_call_count: u64) -> String {
    let id = format!(
        "{}-{}",
        chrono::Utc::now().timestamp_micros(),
        MESSAGE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let created_at_ms = super::now_ms();
    format!(
        "{WARNING_START}{id}\" created_at_ms=\"{created_at_ms}\" tool_call_count=\"{tool_call_count}\" -->\n\n## Warning\n\n{message}\n\n<!-- codexify-warning-message:v1:end id=\"{id}\" -->\n"
    )
}

fn private_directory(path: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(io_error)?;
    let metadata = std::fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Markdown chat directory must be a real directory, not a symlink".into());
    }
    Ok(())
}

fn private_options(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

fn open_regular(path: &Path, append: bool) -> Result<File, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("Markdown chat files must be regular files, not symlinks or devices".into());
    }
    let mut options = OpenOptions::new();
    options.read(true).append(append);
    private_options(&mut options);
    let file = options.open(path).map_err(io_error)?;
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err("Markdown chat file is not a regular file".into());
    }
    Ok(file)
}

fn io_error(error: std::io::Error) -> String {
    format!("Markdown chat file operation failed: {error}")
}

fn read_range(file: &mut File, start: u64, end: u64, limit: usize) -> Result<Vec<u8>, String> {
    let length = end
        .checked_sub(start)
        .ok_or("CHAT.md was truncated; restore its earlier content")?;
    if length > limit as u64 {
        return Err("Unread CHAT.md text exceeds the 16 MiB safety ceiling. Nothing was truncated or consumed; reduce the pending append and retry.".into());
    }
    file.seek(SeekFrom::Start(start)).map_err(io_error)?;
    let mut data = vec![0; length as usize];
    file.read_exact(&mut data).map_err(io_error)?;
    Ok(data)
}

fn check_cursor(file: &mut File, cursor: &Cursor) -> Result<(), String> {
    if file.metadata().map_err(io_error)?.len() < cursor.offset
        || read_range(
            file,
            cursor.offset - cursor.anchor.len() as u64,
            cursor.offset,
            ANCHOR_BYTES,
        )? != cursor.anchor
    {
        return Err("CHAT.md is no longer append-only at the read cursor. Restore earlier content before appending; nothing was consumed.".into());
    }
    Ok(())
}

fn advanced_cursor(file: &mut File, mut cursor: Cursor, end: u64) -> Result<Cursor, String> {
    cursor.anchor = read_range(
        file,
        end.saturating_sub(ANCHOR_BYTES as u64),
        end,
        ANCHOR_BYTES,
    )?;
    cursor.offset = end;
    Ok(cursor)
}

fn check_schema_handoff(chat: &ChatFile) -> Result<(), String> {
    if queue::automation_state(chat)
        .ok()
        .flatten()
        .is_some_and(|state| state["pairing"]["schemaRolloverPending"] == true)
    {
        return Err("The connector schema changed. Codexify is continuing this task in a fresh ChatGPT conversation; user messages remain unread for its new owner. Do not repeat chat_write or end the assistant turn. Call chat_await while the handoff completes.".into());
    }
    Ok(())
}

fn snapshot(chat: &ChatFile, file: &mut File, cursor: &Cursor) -> Result<ChatSnapshot, String> {
    check_cursor(file, cursor)?;
    let end = file.metadata().map_err(io_error)?.len();
    let bytes = read_range(file, cursor.offset, end, MAX_UNREAD_BYTES)?;
    let text = String::from_utf8(bytes).map_err(
        |_| "CHAT.md contains incomplete UTF-8; finish saving and retry. Nothing was consumed.",
    )?;
    Ok(ChatSnapshot {
        text: user_text(&text, &chat.path)?,
        end,
        notification: cursor.notification,
    })
}

fn user_text(text: &str, chat_path: &Path) -> Result<String, String> {
    widget::user_text(text, chat_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_append_between_snapshot_and_agent_write_is_returned_not_lost() {
        let directory = tempfile::tempdir().unwrap();
        let chat = ChatFile::new(directory.path().join("chat/CHAT.md"), false);
        let receipt = chat
            .append_sync("agent reply", || {
                OpenOptions::new()
                    .append(true)
                    .open(chat.path())
                    .unwrap()
                    .write_all(b"racing user text\n")
                    .unwrap();
            })
            .unwrap();
        assert_eq!(receipt.user_text, "racing user text\n");
        assert!(
            chat.with_cursor(|cursor, file| Ok(snapshot(&chat, file, cursor)?.text))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn replay_after_lost_cursor_excludes_agent_blocks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat/CHAT.md");
        ChatFile::new(path.clone(), false)
            .append_sync("agent-only", || {})
            .unwrap();
        let reopened = ChatFile::new(path, false);
        assert_eq!(
            reopened
                .with_cursor(|cursor, file| Ok(snapshot(&reopened, file, cursor)?.text))
                .unwrap(),
            ""
        );
    }
}
