use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::MAX_UNREAD_BYTES;

#[path = "widget.rs"]
mod widget;
pub use widget::{UserSendReceipt, WidgetPage};

const HEADER: &str = "# Codexify Chat\n\nAppend user messages at the bottom and save the file. Do not change earlier content\nwhile the agent is active. The agent sends messages only through chat_write.\n";
const AGENT_START: &str = "\n\n<!-- codexify-agent-message:v1:start id=\"";
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
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Cursor {
    offset: u64,
    anchor: Vec<u8>,
    #[serde(default)]
    last_agent_end: Option<u64>,
    #[serde(default)]
    notification: NotificationState,
    #[serde(default)]
    delivered_through: u64,
    #[serde(default)]
    last_agent_call_at_ms: Option<u64>,
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

pub struct ChatFile {
    path: PathBuf,
    cursor_path: Option<PathBuf>,
    cursor: Mutex<Option<Cursor>>,
}

impl ChatFile {
    pub(super) fn new(path: PathBuf, persistent: bool) -> Self {
        let cursor_path = persistent.then(|| path.with_file_name("cursor.json"));
        Self {
            path,
            cursor_path,
            cursor: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
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
                if cancellation.is_cancelled() {
                    return Err(
                        "Markdown chat read was cancelled; no messages were consumed".into(),
                    );
                }
                let snapshot = snapshot(file, cursor)?;
                if consume && snapshot.end != cursor.offset {
                    if cancellation.is_cancelled() {
                        return Err(
                            "Markdown chat read was cancelled; no messages were consumed".into(),
                        );
                    }
                    let next = advanced_cursor(file, cursor.clone(), snapshot.end)?;
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
        if message.is_empty() {
            return Err("chat_write.message must not be empty".into());
        }
        if message.len() > MAX_UNREAD_BYTES {
            return Err(
                "Agent message exceeds the 16 MiB safety ceiling; nothing was appended".into(),
            );
        }
        self.with_cursor(|cursor, file| {
            let before = snapshot(file, cursor)?;
            let id = format!("{}-{}", chrono::Utc::now().timestamp_micros(), MESSAGE_COUNTER.fetch_add(1, Ordering::Relaxed));
            let block = format!("{AGENT_START}{id}\" -->\n\n## Agent\n\n{message}\n\n<!-- codexify-agent-message:v1:end id=\"{id}\" -->\n");
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
            let user_text = before.text + &user_text(&gap)?;
            let mut next = advanced_cursor(file, cursor.clone(), end)?;
            next.last_agent_end = Some(end);
            next.notification = NotificationState::NotConfigured;
            let cursor_warning = self.save_cursor(&next).err();
            *cursor = next;
            Ok(AppendReceipt { user_text, end_offset: end, cursor_warning })
        })
    }
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

fn snapshot(file: &mut File, cursor: &Cursor) -> Result<ChatSnapshot, String> {
    check_cursor(file, cursor)?;
    let end = file.metadata().map_err(io_error)?.len();
    let bytes = read_range(file, cursor.offset, end, MAX_UNREAD_BYTES)?;
    let text = String::from_utf8(bytes).map_err(
        |_| "CHAT.md contains incomplete UTF-8; finish saving and retry. Nothing was consumed.",
    )?;
    Ok(ChatSnapshot {
        text: user_text(&text)?,
        end,
        notification: cursor.notification,
    })
}

fn user_text(text: &str) -> Result<String, String> {
    widget::user_text(text)
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
            chat.with_cursor(|cursor, file| Ok(snapshot(file, cursor)?.text))
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
                .with_cursor(|cursor, file| Ok(snapshot(file, cursor)?.text))
                .unwrap(),
            ""
        );
    }
}
