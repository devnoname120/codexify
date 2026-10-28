//! Private, durable user messages. Only a released message enters CHAT.md.
use super::*;
use crate::markdown_chat::{MessageMode, attachments::Attachment};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::time::Duration;

const QUEUE_FILE: &str = "message-queue.json";
const MAX_QUEUE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_QUEUED_MESSAGES: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueuedMessage {
    pub id: String,
    pub markdown: String,
    pub attachment_ids: Vec<String>,
    pub attachments: Vec<Attachment>,
    pub created_at_ms: u64,
    pub revision: u64,
}

#[derive(Debug, Serialize)]
pub struct QueueReceipt {
    pub queued: Option<QueuedMessage>,
    pub sent: Option<UserSendReceipt>,
    pub discarded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueSubmission {
    Steer,
    Trash,
    Edit,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct QueueState {
    version: u32,
    messages: Vec<QueuedMessage>,
    // Only submissions made during this exact blocked transition may wake it.
    // Older queue files have no receipts and remain private while blocked.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    blocked_goal_submissions: BTreeMap<String, BlockedGoal>,
    receipts: BTreeMap<String, String>,
    completed_waiting: bool,
    input_waiting: bool,
    pending: Option<PendingSubmission>,
    in_flight: Option<QueueFlight>,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockedGoal {
    revision: u64,
    created_at_ms: u64,
    finished_at_ms: u64,
}

fn blocked_goal(value: &serde_json::Value) -> Option<BlockedGoal> {
    let goal = value.get("goal")?;
    if goal.get("status")?.as_str()? != "blocked" {
        return None;
    }
    let revision = goal.get("revision")?.as_u64()?;
    (revision > 0).then_some(BlockedGoal {
        revision,
        created_at_ms: goal.get("createdAtMs")?.as_u64()?,
        finished_at_ms: goal.get("finishedAtMs")?.as_u64()?,
    })
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingSubmission {
    id: String,
    mode: MessageMode,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueFlight {
    id: String,
    end: u64,
    progressed: bool,
}

fn lock_file(path: &Path) -> Result<File, String> {
    if std::fs::symlink_metadata(path).is_ok() {
        let _ = open_regular(path, false)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    private_options(&mut options);
    let file = options.open(path).map_err(io_error)?;
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err("Queue lock must be a regular file".into());
    }
    file.lock().map_err(io_error)?;
    Ok(file)
}
fn load(chat: &ChatFile) -> Result<QueueState, String> {
    let path = chat.path.with_file_name(QUEUE_FILE);
    if !path.try_exists().map_err(io_error)? {
        return Ok(QueueState {
            version: 1,
            ..Default::default()
        });
    }
    let file = open_regular(&path, false)?;
    if file.metadata().map_err(io_error)?.len() > MAX_QUEUE_BYTES {
        return Err("Message queue exceeds its storage limit".into());
    }
    let state: QueueState = serde_json::from_reader(file.take(MAX_QUEUE_BYTES + 1)).map_err(
        |_| "Message queue is invalid; restore it rather than discarding queued messages",
    )?;
    if state.version != 1 || state.messages.len() > MAX_QUEUED_MESSAGES {
        return Err("Message queue format or size is unsupported".into());
    }
    let mut ids = BTreeSet::new();
    for entry in &state.messages {
        validate(&entry.id, &entry.markdown, &entry.attachment_ids)?;
        if entry.revision == 0 || !ids.insert(&entry.id) || !state.receipts.contains_key(&entry.id)
        {
            return Err("Message queue has inconsistent identities".into());
        }
    }
    if state
        .blocked_goal_submissions
        .iter()
        .any(|(id, goal)| !ids.contains(id) || goal.revision == 0)
    {
        return Err("Message queue has inconsistent blocked-goal receipts".into());
    }
    Ok(state)
}
fn persist(chat: &ChatFile, state: &QueueState) -> Result<(), String> {
    let bytes = serde_json::to_vec(state).map_err(|_| "Could not encode message queue")?;
    if bytes.len() as u64 > MAX_QUEUE_BYTES {
        return Err("Message queue exceeds its storage limit".into());
    }
    let path = chat.path.with_file_name(QUEUE_FILE);
    if std::fs::symlink_metadata(&path).is_ok() {
        let _ = open_regular(&path, false)?;
    }
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().ok_or("Queue has no parent")?)
        .map_err(io_error)?;
    tmp.write_all(&bytes).map_err(io_error)?;
    tmp.as_file().sync_all().map_err(io_error)?;
    tmp.persist(&path).map_err(|e| io_error(e.error))?;
    #[cfg(unix)]
    File::open(path.parent().unwrap())
        .and_then(|f| f.sync_all())
        .map_err(io_error)?;
    Ok(())
}
fn validate(id: &str, text: &str, ids: &[String]) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 80
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err("Invalid queued message ID".into());
    }
    if text.trim().is_empty() && ids.is_empty() {
        return Err("Queued message must not be blank".into());
    }
    if text.len() > MAX_UNREAD_BYTES
        || ids.len() > 20
        || ids
            .iter()
            .any(|id| !crate::markdown_chat::attachments::valid_id(id))
        || ids.iter().collect::<BTreeSet<_>>().len() != ids.len()
    {
        return Err("Queued message or attachments exceed their limits".into());
    }
    Ok(())
}
fn with_queue<T>(
    chat: &ChatFile,
    goal_lock: bool,
    op: impl FnOnce(&mut QueueState) -> Result<T, String>,
) -> Result<T, String> {
    let dir = chat.path.parent().ok_or("Chat has no parent")?;
    private_directory(dir.parent().ok_or("Chat has no state root")?)?;
    private_directory(dir)?;
    let _goal = if goal_lock {
        Some(lock_file(&dir.join("automation.lock"))?)
    } else {
        None
    };
    let _queue = lock_file(&dir.join("message-queue.lock"))?;
    let mut state = load(chat)?;
    op(&mut state)
}
fn goal_holds_queue(chat: &ChatFile) -> Result<bool, String> {
    goal_holds_queue_for_release(chat, false, None)
}

pub(super) fn automation_state(chat: &ChatFile) -> Result<Option<serde_json::Value>, String> {
    let path = chat.path.with_file_name("automation.json");
    if !path.try_exists().map_err(io_error)? {
        return Ok(None);
    }
    let file = open_regular(&path, false)?;
    if file.metadata().map_err(io_error)?.len() > 1024 * 1024 {
        return Err("Goal state is oversized; queued messages remain private".into());
    }
    serde_json::from_reader(file.take(1024 * 1024 + 1))
        .map(Some)
        .map_err(|_| "Goal state cannot be read; queued messages remain private".into())
}

fn goal_holds_queue_for_release(
    chat: &ChatFile,
    offline: bool,
    submission: Option<&BlockedGoal>,
) -> Result<bool, String> {
    goal_holds_queue_in(automation_state(chat)?.as_ref(), offline, submission)
}

fn goal_holds_queue_in(
    value: Option<&serde_json::Value>,
    offline: bool,
    submission: Option<&BlockedGoal>,
) -> Result<bool, String> {
    let Some(value) = value else { return Ok(false) };
    if value
        .pointer("/turnRenewal/resumeWaitUntilMs")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|until| crate::markdown_chat::now_ms() < until)
    {
        return Ok(true);
    }
    if value
        .pointer("/turnRenewal/handoff")
        .is_some_and(|h| !h.is_null())
    {
        return Ok(true);
    }
    match value.get("goal") {
        None | Some(serde_json::Value::Null) => Ok(false),
        Some(goal) => match goal.get("status").and_then(|s| s.as_str()) {
            Some("completed") => Ok(false),
            Some("blocked")
                if offline
                    && submission
                        .is_some_and(|receipt| blocked_goal(value).as_ref() == Some(receipt)) =>
            {
                Ok(false)
            }
            Some(_) => Ok(true),
            None => Err("Goal status is unavailable; queued messages remain private".into()),
        },
    }
}
fn repair_pending(
    chat: &ChatFile,
    state: &mut QueueState,
) -> Result<Option<UserSendReceipt>, String> {
    repair_pending_at(chat, state, None)
}

fn queue_offline(cursor: &Cursor, now_ms: u64, offline_after_ms: u64) -> bool {
    cursor.agent_presence_until_ms.map_or_else(
        || {
            cursor
                .last_agent_call_at_ms
                .is_none_or(|last| now_ms.saturating_sub(last) >= offline_after_ms)
        },
        |until| now_ms >= until,
    )
}

fn repair_pending_at(
    chat: &ChatFile,
    state: &mut QueueState,
    now_ms: Option<u64>,
) -> Result<Option<UserSendReceipt>, String> {
    let Some(pending) = &state.pending else {
        return Ok(None);
    };
    let entry = state
        .messages
        .iter()
        .find(|m| m.id == pending.id)
        .ok_or("Queue submission lost its message")?
        .clone();
    let mode = pending.mode;
    let sent = chat.with_cursor(|cursor, file| {
        let existing = widget::contains_user_message(file, &entry.id)?;
        if mode == MessageMode::Queue
            && !existing
            && (state.input_waiting
                || goal_holds_queue_for_release(
                    chat,
                    now_ms.is_some_and(|now| queue_offline(cursor, now, chat.offline_after_ms())),
                    state.blocked_goal_submissions.get(&entry.id),
                )?
                || cursor.agent_stop_requested
                || !snapshot(chat, file, cursor)?.text.is_empty())
        {
            return Ok(None);
        }
        widget::append_user_locked(
            chat,
            cursor,
            file,
            entry.id.clone(),
            entry.markdown.clone(),
            entry.attachment_ids.clone(),
        )
        .map(Some)
    })?;
    let Some(sent) = sent else { return Ok(None) };
    state.messages.retain(|m| m.id != entry.id);
    state.blocked_goal_submissions.remove(&entry.id);
    state.pending = None;
    state.completed_waiting = false;
    if mode == MessageMode::Queue {
        state.in_flight = Some(QueueFlight {
            id: entry.id,
            end: sent.end,
            progressed: false,
        });
    }
    persist(chat, state)?;
    Ok(Some(sent))
}

pub(super) fn append_steering(
    chat: &ChatFile,
    id: String,
    message: String,
    attachment_ids: Vec<String>,
) -> Result<UserSendReceipt, String> {
    with_queue(chat, false, |state| {
        if state.messages.iter().any(|m| m.id == id) {
            return Err("This message is queued; use its Steer action to send it".into());
        }
        if state.messages.iter().any(|m| {
            m.attachment_ids
                .iter()
                .any(|id| attachment_ids.contains(id))
        }) {
            return Err(
                "An attachment is reserved for a queued message; steer that message instead".into(),
            );
        }
        if let Some(pending) = &state.pending {
            let committed =
                chat.with_cursor(|_, file| widget::contains_user_message(file, &pending.id))?;
            if committed || pending.mode == MessageMode::Steer {
                let _ = repair_pending(chat, state)?;
            } else {
                // An explicit steering message takes priority over an uncommitted release.
                state.pending = None;
                persist(chat, state)?;
            }
        }
        let sent = chat.with_cursor(|cursor, file| {
            widget::append_user_locked(chat, cursor, file, id, message, attachment_ids)
        })?;
        if state.completed_waiting || state.input_waiting {
            state.completed_waiting = false;
            state.input_waiting = false;
            persist(chat, state)?;
        }
        Ok(sent)
    })
}

impl ChatFile {
    pub async fn queue_input_required(self: &Arc<Self>) -> Result<bool, String> {
        self.run(|chat| with_queue(chat, false, |state| Ok(state.input_waiting)))
            .await
    }
    pub async fn queue_reply_received(self: &Arc<Self>) -> Result<(), String> {
        self.run(|chat| {
            with_queue(chat, false, |state| {
                if state.input_waiting || state.completed_waiting {
                    state.input_waiting = false;
                    state.completed_waiting = false;
                    persist(chat, state)?;
                }
                Ok(())
            })
        })
        .await
    }
    pub async fn queue_goal_holds(self: &Arc<Self>) -> Result<bool, String> {
        self.run(|chat| with_queue(chat, true, |_| goal_holds_queue(chat)))
            .await
    }
    pub async fn queue_user_message(
        self: &Arc<Self>,
        id: String,
        markdown: String,
        attachment_ids: Vec<String>,
    ) -> Result<QueueReceipt, String> {
        validate(&id, &markdown, &attachment_ids)?;
        let result = self
            .run(move |chat| {
                with_queue(chat, true, |state| {
                    let digest = format!(
                        "{:x}",
                        Sha256::digest(
                            serde_json::to_vec(&(&markdown, &attachment_ids))
                                .map_err(|_| "Could not identify queued message")?
                        )
                    );
                    if let Some(previous) = state.receipts.get(&id) {
                        if previous != &digest {
                            return Err(
                                "This queue request ID belongs to a different message".into()
                            );
                        }
                        if let Some(entry) = state.messages.iter().find(|m| m.id == id) {
                            return Ok(QueueReceipt {
                                queued: Some(entry.clone()),
                                sent: None,
                                discarded: false,
                            });
                        }
                        let sent = chat.with_cursor(|_, file| widget::user_receipt(file, &id))?;
                        return Ok(QueueReceipt {
                            queued: None,
                            discarded: sent.is_none(),
                            sent,
                        });
                    }
                    let existing = chat.with_cursor(|cursor, file| {
                        if widget::contains_user_message(file, &id)? {
                            widget::append_user_locked(
                                chat,
                                cursor,
                                file,
                                id.clone(),
                                markdown.clone(),
                                attachment_ids.clone(),
                            )
                            .map(Some)
                        } else {
                            Ok(None)
                        }
                    })?;
                    if let Some(sent) = existing {
                        return Ok(QueueReceipt {
                            queued: None,
                            sent: Some(sent),
                            discarded: false,
                        });
                    }
                    if state.messages.len() >= MAX_QUEUED_MESSAGES {
                        return Err("Message queue is full".into());
                    }
                    let mut attachments = Vec::new();
                    let store =
                        crate::markdown_chat::attachments::AttachmentStore::for_chat(&chat.path)?;
                    for attachment_id in &attachment_ids {
                        let attachment = store.observe(attachment_id)?;
                        if attachment.message_id.is_some()
                            || state
                                .messages
                                .iter()
                                .any(|m| m.attachment_ids.contains(attachment_id))
                        {
                            return Err("An attachment is already submitted or queued".into());
                        }
                        attachments.push(attachment);
                    }
                    let entry = QueuedMessage {
                        id: id.clone(),
                        markdown,
                        attachment_ids,
                        attachments,
                        created_at_ms: crate::markdown_chat::now_ms(),
                        revision: 1,
                    };
                    if let Ok(Some(automation)) = automation_state(chat)
                        && let Some(blocked) = blocked_goal(&automation)
                    {
                        state.blocked_goal_submissions.insert(id.clone(), blocked);
                    }
                    state.receipts.insert(id, digest);
                    state.messages.push(entry.clone());
                    persist(chat, state)?;
                    Ok(QueueReceipt {
                        queued: Some(entry),
                        sent: None,
                        discarded: false,
                    })
                })
            })
            .await;
        if result.is_ok() {
            self.start_queue_monitor();
        }
        result
    }
    /// Read the atomically published queue without starting its monitor or
    /// creating queue/automation lock files just to render a sidebar indicator.
    pub(crate) async fn has_wakeable_queued_messages(self: &Arc<Self>) -> Result<bool, String> {
        self.run(|chat| {
            let state = load(chat)?;
            if state.messages.is_empty() || state.input_waiting {
                return Ok(false);
            }
            let automation = automation_state(chat)?;
            for message in &state.messages {
                if !goal_holds_queue_in(
                    automation.as_ref(),
                    true,
                    state.blocked_goal_submissions.get(&message.id),
                )? {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .await
    }

    pub async fn queued_messages(self: &Arc<Self>) -> Result<Vec<QueuedMessage>, String> {
        let result = self
            .run(|chat| with_queue(chat, false, |state| Ok(state.messages.clone())))
            .await;
        self.start_queue_monitor();
        result
    }
    pub async fn update_queued_message(
        self: &Arc<Self>,
        id: String,
        revision: u64,
        action: QueueSubmission,
        text: Option<String>,
    ) -> Result<QueueReceipt, String> {
        if revision == 0 || id.is_empty() || id.len() > 80 {
            return Err("Invalid queued message identity or revision".into());
        }
        self.run(move |chat| {
            with_queue(chat, true, |state| {
                if let Some(pending) = &state.pending {
                    let committed = chat
                        .with_cursor(|_, file| widget::contains_user_message(file, &pending.id))?;
                    if committed {
                        let _ = repair_pending(chat, state)?;
                    } else if pending.id == id || action == QueueSubmission::Steer {
                        state.pending = None;
                    }
                }
                let Some(index) = state.messages.iter().position(|m| m.id == id) else {
                    let sent = chat.with_cursor(|_, file| widget::user_receipt(file, &id))?;
                    if sent.is_some() && action != QueueSubmission::Steer {
                        return Err(
                            "This message has already been sent and is no longer queued".into()
                        );
                    }
                    if sent.is_none() && action != QueueSubmission::Trash {
                        return Err("This queued message was already removed".into());
                    }
                    return Ok(QueueReceipt {
                        queued: None,
                        discarded: sent.is_none(),
                        sent,
                    });
                };
                if state.messages[index].revision != revision {
                    let entry = &state.messages[index];
                    if action == QueueSubmission::Edit
                        && revision.checked_add(1) == Some(entry.revision)
                        && text.as_ref() == Some(&entry.markdown)
                    {
                        return Ok(QueueReceipt {
                            queued: Some(entry.clone()),
                            sent: None,
                            discarded: false,
                        });
                    }
                    return Err(
                        "This queued message changed; refresh it before trying again".into(),
                    );
                }
                match action {
                    QueueSubmission::Edit => {
                        if state.pending.as_ref().is_some_and(|p| p.id == id) {
                            return Err("This queued message is being sent".into());
                        }
                        let entry = &mut state.messages[index];
                        let text = text.ok_or("Edited text is missing")?;
                        validate(&id, &text, &entry.attachment_ids)?;
                        entry.markdown = text;
                        entry.revision = entry
                            .revision
                            .checked_add(1)
                            .ok_or("Queue revision overflow")?;
                        let entry = entry.clone();
                        persist(chat, state)?;
                        Ok(QueueReceipt {
                            queued: Some(entry),
                            sent: None,
                            discarded: false,
                        })
                    }
                    QueueSubmission::Trash => {
                        state.messages.remove(index);
                        state.blocked_goal_submissions.remove(&id);
                        if state.pending.as_ref().is_some_and(|p| p.id == id) {
                            state.pending = None;
                        }
                        persist(chat, state)?;
                        Ok(QueueReceipt {
                            queued: None,
                            sent: None,
                            discarded: true,
                        })
                    }
                    QueueSubmission::Steer => {
                        state.pending = Some(PendingSubmission {
                            id,
                            mode: MessageMode::Steer,
                        });
                        state.completed_waiting = false;
                        state.input_waiting = false;
                        persist(chat, state)?;
                        let sent = repair_pending(chat, state)?;
                        Ok(QueueReceipt {
                            queued: None,
                            sent,
                            discarded: false,
                        })
                    }
                }
            })
        })
        .await
    }
    pub async fn queue_agent_work(self: &Arc<Self>, progress: bool) -> Result<(), String> {
        if progress && !self.path.with_file_name(QUEUE_FILE).exists() {
            return Ok(());
        }
        self.run(move |chat| {
            with_queue(chat, false, |state| {
                let changed = state.completed_waiting
                    || !progress && !state.input_waiting
                    || progress && state.in_flight.as_ref().is_some_and(|f| !f.progressed);
                state.completed_waiting = false;
                if !progress {
                    state.input_waiting = true;
                }
                if progress && let Some(flight) = &mut state.in_flight {
                    flight.progressed = true;
                }
                if changed {
                    persist(chat, state)?;
                }
                Ok(())
            })
        })
        .await
    }
    pub async fn queue_turn_completed(self: &Arc<Self>) -> Result<bool, String> {
        self.run(|chat| {
            with_queue(chat, true, |state| {
                state.input_waiting = false;
                if goal_holds_queue(chat)? {
                    state.completed_waiting = false;
                    persist(chat, state)?;
                    return Ok(true);
                }
                chat.with_cursor(|cursor, file| {
                    if !snapshot(chat, file, cursor)?.text.is_empty() {
                        state.completed_waiting = false;
                        persist(chat, state)?;
                        return Ok(true);
                    }
                    if state.in_flight.as_ref().is_some_and(|f| !f.progressed) {
                        return Ok(false);
                    }
                    state.in_flight = None;
                    state.completed_waiting = true;
                    persist(chat, state)?;
                    Ok(true)
                })
            })
        })
        .await
    }
    pub async fn release_queued_message(
        self: &Arc<Self>,
        now_ms: u64,
    ) -> Result<Option<UserSendReceipt>, String> {
        if !self.path.with_file_name(QUEUE_FILE).exists() {
            return Ok(None);
        }
        self.run(move |chat| {
            with_queue(chat, true, |state| {
                if let Some(sent) = repair_pending_at(chat, state, Some(now_ms))? {
                    return Ok(Some(sent));
                }
                if state.messages.is_empty() || state.input_waiting {
                    return Ok(None);
                }
                let next = chat.with_cursor(|cursor, file| {
                    let offline = queue_offline(cursor, now_ms, chat.offline_after_ms());
                    if cursor.agent_stop_requested || !snapshot(chat, file, cursor)?.text.is_empty()
                    {
                        return Ok(None);
                    }
                    if state
                        .in_flight
                        .as_ref()
                        .is_some_and(|f| !f.progressed || cursor.offset < f.end)
                    {
                        return Ok(None);
                    }
                    if !state.completed_waiting && !offline {
                        return Ok(None);
                    }
                    let automation = automation_state(chat)?;
                    for message in &state.messages {
                        if !goal_holds_queue_in(
                            automation.as_ref(),
                            offline,
                            state.blocked_goal_submissions.get(&message.id),
                        )? {
                            return Ok(Some(message.id.clone()));
                        }
                    }
                    Ok(None)
                })?;
                let Some(id) = next else {
                    return Ok(None);
                };
                state.pending = Some(PendingSubmission {
                    id,
                    mode: MessageMode::Queue,
                });
                state.completed_waiting = false;
                persist(chat, state)?;
                repair_pending_at(chat, state, Some(now_ms))
            })
        })
        .await
    }
    pub(crate) fn start_queue_monitor(self: &Arc<Self>) {
        if !self.path.with_file_name(QUEUE_FILE).exists()
            || tokio::runtime::Handle::try_current().is_err()
            || self.queue_monitor_started.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(chat) = weak.upgrade() else { return };
                if let Err(error) = chat
                    .release_queued_message(crate::markdown_chat::now_ms())
                    .await
                {
                    tracing::warn!(%error,"queued message remains pending");
                }
                let pending = chat
                    .run(|chat| with_queue(chat, false, |state| Ok(!state.messages.is_empty())))
                    .await
                    .unwrap_or(true);
                if !pending {
                    chat.queue_monitor_started.store(false, Ordering::Release);
                    let again = chat
                        .run(|chat| with_queue(chat, false, |state| Ok(!state.messages.is_empty())))
                        .await
                        .unwrap_or(false);
                    if again {
                        chat.start_queue_monitor();
                    }
                    return;
                }
                drop(chat);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown_chat::OFFLINE_AFTER_MS;
    use crate::markdown_chat::attachments::AttachmentStore;
    use tokio_util::sync::CancellationToken;

    async fn fixture() -> (tempfile::TempDir, Arc<ChatFile>) {
        let root = tempfile::tempdir().unwrap();
        let chat = Arc::new(ChatFile::new(root.path().join("chats/one/CHAT.md"), true));
        chat.ensure().await.unwrap();
        chat.record_agent_call(crate::markdown_chat::now_ms())
            .await
            .unwrap();
        // All boundary tests advance release deterministically, independently of the monitor.
        chat.queue_monitor_started.store(true, Ordering::Release);
        (root, chat)
    }
    async fn enqueue(chat: &Arc<ChatFile>, id: &str) {
        chat.queue_user_message(id.into(), format!("private {id}"), vec![])
            .await
            .unwrap();
    }
    fn state(chat: &ChatFile) -> serde_json::Value {
        serde_json::from_str(
            &std::fs::read_to_string(chat.path.with_file_name(QUEUE_FILE)).unwrap(),
        )
        .unwrap()
    }
    fn goal(chat: &ChatFile, value: &str) {
        std::fs::write(
            chat.path.with_file_name("automation.json"),
            serde_json::json!({"goal":{"status":value,"revision":1,"createdAtMs":1,"finishedAtMs":if value == "blocked" {Some(2)} else {None}}}).to_string(),
        )
        .unwrap();
    }
    async fn text(chat: &Arc<ChatFile>) -> String {
        chat.read(true).await.unwrap().text
    }

    #[tokio::test]
    async fn private_until_completion_and_exactly_one_per_handled_turn() {
        let (_root, chat) = fixture().await;
        enqueue(&chat, "first").await;
        enqueue(&chat, "second").await;
        assert!(
            !std::fs::read_to_string(chat.path())
                .unwrap()
                .contains("private first")
        );
        assert!(text(&chat).await.is_empty());
        assert!(
            chat.release_queued_message(crate::markdown_chat::now_ms())
                .await
                .unwrap()
                .is_none()
        );
        assert!(chat.queue_turn_completed().await.unwrap());
        assert!(
            chat.release_queued_message(crate::markdown_chat::now_ms())
                .await
                .unwrap()
                .is_some()
        );
        assert!(text(&chat).await.contains("private first"));
        assert!(
            !chat.queue_turn_completed().await.unwrap(),
            "repeating completion cannot skip handling the released request"
        );
        assert!(
            chat.release_queued_message(u64::MAX)
                .await
                .unwrap()
                .is_none()
        );
        chat.queue_agent_work(true).await.unwrap();
        assert!(chat.queue_turn_completed().await.unwrap());
        chat.release_queued_message(crate::markdown_chat::now_ms())
            .await
            .unwrap()
            .unwrap();
        assert!(text(&chat).await.contains("private second"));
        assert!(chat.queued_messages().await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn steering_has_priority_and_preserves_the_rest_of_the_queue() {
        let (_root, chat) = fixture().await;
        for id in ["first", "second", "third"] {
            enqueue(&chat, id).await;
        }
        chat.queue_turn_completed().await.unwrap();
        chat.update_queued_message("third".into(), 1, QueueSubmission::Steer, None)
            .await
            .unwrap()
            .sent
            .unwrap();
        assert!(text(&chat).await.contains("private third"));
        assert!(
            chat.release_queued_message(crate::markdown_chat::now_ms())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            chat.queued_messages()
                .await
                .unwrap()
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        chat.queue_agent_work(true).await.unwrap();
        chat.queue_turn_completed().await.unwrap();
        chat.append_user("urgent".into(), "Immediate steering".into())
            .await
            .unwrap();
        assert!(
            chat.release_queued_message(crate::markdown_chat::now_ms())
                .await
                .unwrap()
                .is_none()
        );
        assert!(text(&chat).await.contains("Immediate steering"));
        assert_eq!(chat.queued_messages().await.unwrap().len(), 2);
    }
    #[tokio::test]
    async fn active_paused_and_unknown_goals_hold_queue_until_completed() {
        let (_root, chat) = fixture().await;
        enqueue(&chat, "first").await;
        for value in ["active", "paused", "configured", "unknown"] {
            goal(&chat, value);
            chat.queue_turn_completed().await.unwrap();
            assert!(
                chat.release_queued_message(u64::MAX)
                    .await
                    .unwrap()
                    .is_none(),
                "{value}"
            );
            assert!(text(&chat).await.is_empty());
            chat.queue_agent_work(true).await.unwrap();
            assert!(
                !std::fs::read_to_string(chat.path())
                    .unwrap()
                    .contains("private first")
            );
        }
        goal(&chat, "completed");
        chat.queue_turn_completed().await.unwrap();
        chat.release_queued_message(crate::markdown_chat::now_ms())
            .await
            .unwrap()
            .unwrap();
        assert!(text(&chat).await.contains("private first"));
    }

    #[tokio::test]
    async fn blocked_goal_releases_one_message_only_after_the_agent_is_offline() {
        let (_root, chat) = fixture().await;
        let last = chat
            .owner_summary()
            .await
            .unwrap()
            .last_agent_call_at_ms
            .unwrap();
        goal(&chat, "active");
        enqueue(&chat, "older").await;
        goal(&chat, "blocked");
        let blocked_state =
            std::fs::read_to_string(chat.path.with_file_name("automation.json")).unwrap();
        enqueue(&chat, "first").await;
        enqueue(&chat, "second").await;
        with_queue(&chat, false, |state| {
            state.pending = Some(PendingSubmission {
                id: "older".into(),
                mode: MessageMode::Queue,
            });
            persist(&chat, state)
        })
        .unwrap();
        let chat = Arc::new(ChatFile::new(chat.path.clone(), true));
        chat.queue_monitor_started.store(true, Ordering::Release);
        chat.queue_turn_completed().await.unwrap();
        assert!(
            chat.release_queued_message(last + OFFLINE_AFTER_MS - 1)
                .await
                .unwrap()
                .is_none()
        );
        assert!(text(&chat).await.is_empty());
        let sent = chat
            .release_queued_message(last + OFFLINE_AFTER_MS)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sent.id, "first");
        assert!(
            chat.release_queued_message(last + OFFLINE_AFTER_MS + 1)
                .await
                .unwrap()
                .is_none()
        );
        let queued = chat.queued_messages().await.unwrap();
        assert_eq!(
            queued.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["older", "second"]
        );
        assert!(
            !std::fs::read_to_string(chat.path())
                .unwrap()
                .contains("private older")
        );
        assert_eq!(
            std::fs::read_to_string(chat.path.with_file_name("automation.json")).unwrap(),
            blocked_state
        );
    }

    #[tokio::test]
    async fn old_submissions_stay_private_after_block_even_when_retried_or_edited() {
        let (_root, chat) = fixture().await;
        goal(&chat, "active");
        enqueue(&chat, "older").await;
        goal(&chat, "blocked");
        // Repeating an HTTP submission or editing its text is not a new message.
        enqueue(&chat, "older").await;
        chat.update_queued_message(
            "older".into(),
            1,
            QueueSubmission::Edit,
            Some("edited old request".into()),
        )
        .await
        .unwrap();
        assert!(
            chat.release_queued_message(u64::MAX)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!chat.has_wakeable_queued_messages().await.unwrap());
        enqueue(&chat, "newer").await;
        assert!(chat.has_wakeable_queued_messages().await.unwrap());
        let sent = chat
            .release_queued_message(u64::MAX)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sent.id, "newer");
        assert_eq!(chat.queued_messages().await.unwrap()[0].id, "older");
        assert!(!text(&chat).await.contains("edited old request"));
        // Even after handling the new message, blocking is not completion of the
        // original goal and must not drain requests that were queued beforehand.
        chat.queue_agent_work(true).await.unwrap();
        assert!(
            chat.release_queued_message(u64::MAX)
                .await
                .unwrap()
                .is_none()
        );
        goal(&chat, "completed");
        chat.queue_turn_completed().await.unwrap();
        assert_eq!(
            chat.release_queued_message(u64::MAX)
                .await
                .unwrap()
                .unwrap()
                .id,
            "older"
        );
    }

    #[tokio::test]
    async fn legacy_and_previous_block_receipts_cannot_wake_a_new_blocked_transition() {
        let (_root, chat) = fixture().await;
        goal(&chat, "blocked");
        enqueue(&chat, "previous-block").await;
        enqueue(&chat, "legacy").await;
        with_queue(&chat, false, |state| {
            state.blocked_goal_submissions.remove("legacy");
            persist(&chat, state)
        })
        .unwrap();
        // The same revision was resumed and blocked again. Its old queued input
        // predates this transition and cannot inherit a new wake-up permission.
        let path = chat.path.with_file_name("automation.json");
        let mut automation: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        automation["goal"]["finishedAtMs"] = serde_json::json!(3);
        std::fs::write(&path, automation.to_string()).unwrap();
        let reopened = Arc::new(ChatFile::new(chat.path.clone(), true));
        reopened
            .queue_monitor_started
            .store(true, Ordering::Release);
        assert!(
            reopened
                .release_queued_message(u64::MAX)
                .await
                .unwrap()
                .is_none()
        );
        enqueue(&reopened, "current-block").await;
        assert_eq!(
            reopened
                .release_queued_message(u64::MAX)
                .await
                .unwrap()
                .unwrap()
                .id,
            "current-block"
        );
        assert_eq!(reopened.queued_messages().await.unwrap().len(), 2);
        let delivered = text(&reopened).await;
        assert!(delivered.contains("private current-block"));
        assert!(
            !delivered.contains("private previous-block") && !delivered.contains("private legacy")
        );
    }

    #[tokio::test]
    async fn blocked_goal_release_keeps_input_stop_and_handoff_holds() {
        for hold in ["input", "stop", "handoff", "resume"] {
            let (_root, chat) = fixture().await;
            goal(&chat, "blocked");
            enqueue(&chat, "first").await;
            match hold {
                "input" => chat.queue_agent_work(false).await.unwrap(),
                "stop" => chat.request_agent_stop("stop".into()).await.unwrap(),
                _ => {
                    let renewal = if hold == "handoff" {
                        serde_json::json!({"handoff":{}})
                    } else {
                        serde_json::json!({"resumeWaitUntilMs":crate::markdown_chat::now_ms()+60_000})
                    };
                    let path = chat.path.with_file_name("automation.json");
                    let mut automation: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                    automation["turnRenewal"] = renewal;
                    std::fs::write(path, automation.to_string()).unwrap();
                }
            }
            assert!(
                chat.release_queued_message(u64::MAX)
                    .await
                    .unwrap()
                    .is_none(),
                "{hold}"
            );
            assert!(text(&chat).await.is_empty(), "{hold}");
        }
    }

    #[tokio::test]
    async fn monitor_releases_a_blocked_goals_message_without_an_open_widget() {
        let root = tempfile::tempdir().unwrap();
        let chat = Arc::new(ChatFile::new(root.path().join("chats/one/CHAT.md"), true));
        chat.ensure().await.unwrap();
        chat.queue_monitor_started.store(true, Ordering::Release);
        goal(&chat, "blocked");
        enqueue(&chat, "first").await;
        enqueue(&chat, "second").await;
        chat.queue_monitor_started.store(false, Ordering::Release);
        chat.start_queue_monitor();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if chat.queued_messages().await.unwrap().len() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        assert!(text(&chat).await.contains("private first"));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(chat.queued_messages().await.unwrap()[0].id, "second");
    }
    #[tokio::test]
    async fn malformed_goal_or_queue_fails_closed_without_losing_messages() {
        let (_root, chat) = fixture().await;
        enqueue(&chat, "first").await;
        std::fs::write(chat.path.with_file_name("automation.json"), "{").unwrap();
        assert!(chat.release_queued_message(u64::MAX).await.is_err());
        assert_eq!(chat.queued_messages().await.unwrap().len(), 1);
        assert!(text(&chat).await.is_empty());
        let private = std::fs::read(chat.path.with_file_name(QUEUE_FILE)).unwrap();
        std::fs::write(chat.path.with_file_name(QUEUE_FILE), "{").unwrap();
        assert!(chat.queued_messages().await.is_err());
        assert!(
            chat.append_user("urgent".into(), "hello".into())
                .await
                .is_err()
        );
        std::fs::write(chat.path.with_file_name(QUEUE_FILE), private).unwrap();
        assert_eq!(chat.queued_messages().await.unwrap().len(), 1);
    }
    #[tokio::test]
    async fn edit_trash_and_creation_retries_preserve_identity_and_fifo() {
        let (_root, chat) = fixture().await;
        enqueue(&chat, "first").await;
        enqueue(&chat, "second").await;
        let edited = chat
            .update_queued_message(
                "first".into(),
                1,
                QueueSubmission::Edit,
                Some("Edited privately".into()),
            )
            .await
            .unwrap()
            .queued
            .unwrap();
        assert_eq!(edited.revision, 2);
        assert_eq!(
            chat.update_queued_message(
                "first".into(),
                1,
                QueueSubmission::Edit,
                Some("Edited privately".into())
            )
            .await
            .unwrap()
            .queued
            .unwrap()
            .revision,
            2
        );
        assert!(
            chat.update_queued_message("first".into(), 1, QueueSubmission::Trash, None)
                .await
                .is_err()
        );
        let retry = chat
            .queue_user_message("first".into(), "private first".into(), vec![])
            .await
            .unwrap()
            .queued
            .unwrap();
        assert_eq!(retry.markdown, "Edited privately");
        assert!(
            chat.queue_user_message("first".into(), "Different original".into(), vec![])
                .await
                .is_err()
        );
        chat.update_queued_message("second".into(), 1, QueueSubmission::Trash, None)
            .await
            .unwrap();
        assert!(
            chat.queue_user_message("second".into(), "private second".into(), vec![])
                .await
                .unwrap()
                .discarded
        );
        assert!(
            chat.update_queued_message("second".into(), 1, QueueSubmission::Trash, None)
                .await
                .unwrap()
                .discarded
        );
        assert_eq!(chat.queued_messages().await.unwrap().len(), 1);
        assert!(
            !std::fs::read_to_string(chat.path())
                .unwrap()
                .contains("Edited privately")
        );
        chat.queue_turn_completed().await.unwrap();
        chat.release_queued_message(crate::markdown_chat::now_ms())
            .await
            .unwrap();
        assert!(text(&chat).await.contains("Edited privately"));
        let retry = chat
            .queue_user_message("first".into(), "private first".into(), vec![])
            .await
            .unwrap();
        assert!(retry.sent.is_some());
        assert!(!retry.discarded);
        assert!(
            chat.update_queued_message(
                "first".into(),
                2,
                QueueSubmission::Edit,
                Some("too late".into())
            )
            .await
            .is_err()
        );
        assert!(
            chat.update_queued_message("first".into(), 2, QueueSubmission::Steer, None)
                .await
                .unwrap()
                .sent
                .is_some()
        );
    }
    #[tokio::test]
    async fn queued_delivery_respects_the_configured_offline_timeout() {
        let (_root, chat) = fixture().await;
        chat.set_offline_timeout(600_000);
        enqueue(&chat, "slow-turn").await;
        let last = chat
            .owner_summary()
            .await
            .unwrap()
            .last_agent_call_at_ms
            .unwrap();
        assert!(
            chat.release_queued_message(last + 300_000)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            chat.release_queued_message(last + 600_000 - 1)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            chat.widget_page(None, None).await.unwrap().offline_after_ms,
            600_000
        );
        assert!(
            chat.release_queued_message(last + 600_000)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn offline_threshold_stop_and_input_wait_are_distinct() {
        let (_root, chat) = fixture().await;
        enqueue(&chat, "first").await;
        let last = chat
            .owner_summary()
            .await
            .unwrap()
            .last_agent_call_at_ms
            .unwrap();
        assert!(
            chat.release_queued_message(last + OFFLINE_AFTER_MS - 1)
                .await
                .unwrap()
                .is_none()
        );
        chat.queue_agent_work(false).await.unwrap();
        assert!(
            chat.release_queued_message(last + OFFLINE_AFTER_MS)
                .await
                .unwrap()
                .is_none(),
            "input-needed holds queue even when idle"
        );
        chat.queue_agent_work(true).await.unwrap();
        assert!(
            chat.queue_input_required().await.unwrap(),
            "tool progress does not answer the user's pending question"
        );
        assert!(
            chat.release_queued_message(u64::MAX)
                .await
                .unwrap()
                .is_none()
        );
        chat.queue_reply_received().await.unwrap();
        chat.request_agent_stop("stop-request".into())
            .await
            .unwrap();
        assert!(
            chat.release_queued_message(last + OFFLINE_AFTER_MS)
                .await
                .unwrap()
                .is_none()
        );
        assert!(chat.agent_stop_requested().await.unwrap());
        chat.resume_agent().await.unwrap();
        chat.release_queued_message(last + OFFLINE_AFTER_MS)
            .await
            .unwrap()
            .unwrap();
        assert!(text(&chat).await.contains("private first"));
    }
    #[tokio::test]
    async fn attachments_stay_draft_and_unlisted_until_release() {
        let (_root, chat) = fixture().await;
        let file = AttachmentStore::for_chat(chat.path())
            .unwrap()
            .upload(
                "queued-upload",
                "private.txt",
                "text/plain",
                Some(1000),
                false,
                b"private bytes".to_vec(),
            )
            .unwrap();
        chat.queue_user_message(
            "attached".into(),
            "attachment request".into(),
            vec![file.id.clone()],
        )
        .await
        .unwrap();
        assert!(chat.list_attachments().await.unwrap().is_empty());
        assert!(
            chat.queue_user_message(
                "other".into(),
                "duplicate attachment".into(),
                vec![file.id.clone()]
            )
            .await
            .is_err()
        );
        assert!(
            chat.append_user_with_attachments(
                "steer".into(),
                "reserved".into(),
                vec![file.id.clone()]
            )
            .await
            .is_err()
        );
        chat.queue_turn_completed().await.unwrap();
        chat.release_queued_message(crate::markdown_chat::now_ms())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(chat.list_attachments().await.unwrap().len(), 1);
        assert!(text(&chat).await.contains("attachment request"));
    }
    #[tokio::test]
    async fn restart_and_journal_repair_preserve_goal_holds_and_never_duplicate() {
        let (_root, chat) = fixture().await;
        enqueue(&chat, "first").await;
        enqueue(&chat, "second").await;
        with_queue(&chat, false, |s| {
            s.pending = Some(PendingSubmission {
                id: "first".into(),
                mode: MessageMode::Queue,
            });
            persist(&chat, s)
        })
        .unwrap();
        goal(&chat, "blocked");
        let reopened = Arc::new(ChatFile::new(chat.path.clone(), true));
        reopened
            .queue_monitor_started
            .store(true, Ordering::Release);
        assert!(
            reopened
                .release_queued_message(crate::markdown_chat::now_ms())
                .await
                .unwrap()
                .is_none()
        );
        goal(&chat, "completed");
        reopened
            .release_queued_message(u64::MAX)
            .await
            .unwrap()
            .unwrap();
        // Simulate a crash after transcript commit but before queue metadata commit.
        let mut before = load(&reopened).unwrap();
        let first = QueuedMessage {
            id: "first".into(),
            markdown: "private first".into(),
            attachment_ids: vec![],
            attachments: vec![],
            created_at_ms: 0,
            revision: 1,
        };
        before.messages.insert(0, first);
        before.pending = Some(PendingSubmission {
            id: "first".into(),
            mode: MessageMode::Queue,
        });
        persist(&reopened, &before).unwrap();
        goal(&chat, "active");
        reopened
            .release_queued_message(u64::MAX)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(chat.path())
                .unwrap()
                .matches("private first")
                .count(),
            1
        );
        assert_eq!(reopened.queued_messages().await.unwrap().len(), 1);
    }
    #[tokio::test]
    async fn queued_item_waiting_in_journal_can_be_edited_deleted_or_steered() {
        for action in [
            QueueSubmission::Edit,
            QueueSubmission::Trash,
            QueueSubmission::Steer,
        ] {
            let (_root, chat) = fixture().await;
            enqueue(&chat, "first").await;
            enqueue(&chat, "second").await;
            with_queue(&chat, false, |s| {
                s.pending = Some(PendingSubmission {
                    id: "first".into(),
                    mode: MessageMode::Queue,
                });
                persist(&chat, s)
            })
            .unwrap();
            chat.update_queued_message("first".into(), 1, action, Some("edited".into()))
                .await
                .unwrap();
            assert!(state(&chat)["pending"].is_null());
            let body = std::fs::read_to_string(chat.path()).unwrap();
            assert_eq!(
                body.contains("private first"),
                action == QueueSubmission::Steer
            );
            assert!(!body.contains("private second"));
        }
    }
    #[tokio::test]
    async fn concurrent_completion_and_release_cannot_drain_queue() {
        let (_root, chat) = fixture().await;
        for id in ["first", "second", "third"] {
            enqueue(&chat, id).await;
        }
        let mut tasks = vec![];
        for _ in 0..12 {
            let chat = chat.clone();
            tasks.push(tokio::spawn(async move {
                chat.queue_turn_completed().await.unwrap();
                chat.release_queued_message(crate::markdown_chat::now_ms())
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(chat.queued_messages().await.unwrap().len(), 2);
        assert!(text(&chat).await.contains("private first"));
        assert!(text(&chat).await.is_empty());
    }
    #[tokio::test]
    async fn completed_wait_accepts_future_queue_without_input_required_overlay() {
        let (_root, chat) = fixture().await;
        chat.queue_turn_completed().await.unwrap();
        let waiting = chat.clone();
        let task = tokio::spawn(async move {
            waiting
                // Native watcher startup can exceed two seconds on a busy host.
                .wait_completed(Duration::from_secs(10), CancellationToken::new())
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let summary = chat.owner_summary().await.unwrap();
        assert!(summary.agent_waiting_until_ms.is_none());
        assert!(summary.agent_idle_until_ms.is_some());
        assert!(
            chat.widget_page(None, None)
                .await
                .unwrap()
                .agent_waiting_until_ms
                .is_none()
        );
        assert!(!chat.queue_input_required().await.unwrap());
        enqueue(&chat, "first").await;
        enqueue(&chat, "second").await;
        match task.await.unwrap() {
            crate::markdown_chat::WaitOutcome::Message(s) => {
                assert!(s.text.contains("private first"));
                assert!(!s.text.contains("private second"));
            }
            _ => panic!("waiting completed turn did not receive request"),
        }
        assert_eq!(chat.queued_messages().await.unwrap().len(), 1);
    }
    #[tokio::test]
    async fn monitor_releases_one_after_idle_boundary_without_an_open_widget() {
        let (_root, chat) = fixture().await;
        chat.queue_monitor_started.store(false, Ordering::Release);
        chat.record_agent_call(crate::markdown_chat::now_ms() + 1)
            .await
            .unwrap();
        enqueue(&chat, "first").await;
        enqueue(&chat, "second").await;
        chat.queue_turn_completed().await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if chat.queued_messages().await.unwrap().len() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        assert!(text(&chat).await.contains("private first"));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(chat.queued_messages().await.unwrap().len(), 1);
    }
}
