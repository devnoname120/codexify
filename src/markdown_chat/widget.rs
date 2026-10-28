use std::io::{BufRead, BufReader};

use super::*;

const USER_START: &str = "\n\n<!-- codexify-user-message:v1:start id=\"";
const PAGE_MESSAGES: usize = 50;
const PAGE_BYTES: usize = 512 * 1024;
const MAX_CLEAN_TRANSCRIPT_BYTES: usize = 64 * 1024 * 1024;
const MAX_GOAL_EVENTS_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
struct GoalEvent {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report: Option<String>,
    #[serde(default)]
    blocked: bool,
    markdown: String,
    offset: u64,
    created_at_ms: u64,
    tool_call_count: Option<u64>,
}

fn goal_events(chat: &ChatFile) -> Result<Vec<GoalEvent>, String> {
    let path = chat.path.with_file_name("goal-events.json");
    if !path.try_exists().map_err(io_error)? {
        return Ok(Vec::new());
    }
    let file = open_regular(&path, false)?;
    if file.metadata().map_err(io_error)?.len() > MAX_GOAL_EVENTS_BYTES {
        return Err("Goal timeline is too large.".into());
    }
    serde_json::from_reader(file.take(MAX_GOAL_EVENTS_BYTES + 1))
        .map_err(|_| "Goal timeline could not be read.".into())
}

pub const AGENT_STOP_MESSAGE: &str = "Write your current progress and status using chat_write if that tool is available, then immediately finish your turn without running any further tools. The user has stopped this turn and paused the goal; do not continue the goal unless the user explicitly resumes it.";

#[derive(Debug, Serialize)]
pub struct WidgetMessage {
    pub id: String,
    pub role: String,
    pub markdown: String,
    pub start: u64,
    pub end: u64,
    pub created_at_ms: Option<u64>,
    pub tool_call_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_status: Option<crate::agent_automation::GoalStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_report: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<super::super::attachments::Attachment>,
}

#[derive(Debug, Serialize)]
pub struct WidgetPage {
    pub offline_after_ms: u64,
    pub chat_file: String,
    pub revision: String,
    pub delivered_through: u64,
    pub read_through: u64,
    pub last_agent_call_at_ms: Option<u64>,
    pub agent_presence_until_ms: Option<u64>,
    pub agent_stop_requested: bool,
    pub agent_waiting_until_ms: Option<u64>,
    pub total_tool_calls: u64,
    pub server_time_ms: u64,
    pub messages: Vec<WidgetMessage>,
    pub has_more: bool,
    pub before: Option<u64>,
    pub unchanged: bool,
}

#[derive(Debug, Serialize)]
pub struct UserSendReceipt {
    pub id: String,
    pub end: u64,
    pub created_at_ms: Option<u64>,
    pub tool_call_count: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct OwnerChatSummary {
    pub title: String,
    pub custom_title: Option<String>,
    pub pinned_at_ms: Option<u64>,
    pub archived_at_ms: Option<u64>,
    pub owner_read_through: u64,
    pub last_entry_end: u64,
    pub last_incoming_entry_end: u64,
    pub last_entry_at_ms: u64,
    pub last_user_entry_end: u64,
    pub last_user_entry_at_ms: Option<u64>,
    pub last_agent_call_at_ms: Option<u64>,
    pub agent_presence_until_ms: Option<u64>,
    pub agent_stop_requested: bool,
    pub agent_waiting_until_ms: Option<u64>,
    pub agent_idle_until_ms: Option<u64>,
    pub total_tool_calls: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerChatOrganization {
    pub pinned_at_ms: Option<u64>,
    pub archived_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UnreadMessage {
    pub id: String,
    pub markdown: String,
    pub start: u64,
    pub end: u64,
    pub created_at_ms: Option<u64>,
    pub tool_call_count: Option<u64>,
}

impl UnreadMessage {
    pub(crate) fn model_markdown(&self) -> String {
        super::super::output::timestamped_user_message(&self.markdown, self.created_at_ms)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UnreadSnapshot {
    pub revision: String,
    pub read_through: u64,
    pub end: u64,
    pub messages: Vec<UnreadMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TranscriptCheckpoint {
    pub revision: String,
    pub end: u64,
    anchor: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrowserAppendReceipt {
    pub id: String,
    pub end: u64,
    pub appended: bool,
}

#[derive(Clone)]
struct Span {
    id: String,
    role: &'static str,
    start: u64,
    body_start: u64,
    body_end: u64,
    end: u64,
    created_at_ms: Option<u64>,
    tool_call_count: Option<u64>,
}

impl Span {
    fn is_goal_update(&self) -> bool {
        // Owner goal edits use this reserved ID with the SHA-256 of the request ID.
        // Keep these control notices in CHAT.md for agent delivery, including old
        // notices, but omit them from the displayed conversation. Do not match text.
        self.role == "user"
            && self.id.strip_prefix("goal-update-").is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            })
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn marker_fields<'a>(role: &str, fields: &'a str) -> Option<(&'a str, Option<u64>, Option<u64>)> {
    let (fields, tool_call_count) = match fields.rsplit_once("\" tool_call_count=\"") {
        Some((fields, count)) => (fields, Some(count.parse::<u64>().ok()?)),
        None => (fields, None),
    };
    let (id, time) = match fields.split_once("\" created_at_ms=\"") {
        Some((id, timestamp)) => {
            let timestamp = timestamp.parse::<u64>().ok()?;
            if timestamp > 8_640_000_000_000_000 {
                return None;
            }
            (id, Some(timestamp))
        }
        None => (fields, None),
    };
    if !valid_id(id) {
        return None;
    }
    // Earlier agent IDs already encode UTC microseconds; user IDs do not.
    let legacy_time = || {
        if role != "agent" {
            return None;
        }
        let (micros, counter) = id.split_once('-')?;
        if micros.len() < 16 {
            return None;
        }
        counter.parse::<u64>().ok()?;
        let timestamp = micros.parse::<u64>().ok()? / 1000;
        (timestamp <= 8_640_000_000_000_000).then_some(timestamp)
    };
    Some((id, time.or_else(legacy_time), tool_call_count))
}

fn start_marker(line: &str) -> Option<(&'static str, String, Option<u64>, Option<u64>)> {
    for role in ["agent", "user", "warning"] {
        if let Some((id, time, tool_call_count)) = line
            .strip_prefix(&format!("<!-- codexify-{role}-message:v1:start id=\""))
            .and_then(|line| line.strip_suffix("\" -->\n"))
            .and_then(|fields| marker_fields(role, fields))
        {
            return Some((role, id.to_string(), time, tool_call_count));
        }
    }
    None
}

fn raw_span(start: u64, end: u64) -> Span {
    Span {
        id: format!("manual-{start}"),
        role: "user",
        start,
        body_start: start,
        body_end: end,
        end,
        created_at_ms: None,
        tool_call_count: None,
    }
}

fn spans(file: &mut File) -> Result<Vec<Span>, String> {
    let length = file.metadata().map_err(io_error)?.len();
    let prefix = read_range(file, 0, length.min(HEADER.len() as u64), HEADER.len())?;
    let mut offset = if prefix == HEADER.as_bytes() {
        HEADER.len() as u64
    } else {
        0
    };
    file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
    let mut reader = BufReader::new(file.take(length - offset));
    let mut result = Vec::new();
    let mut raw_start = offset;
    let mut active: Option<(Span, String, usize)> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).map_err(io_error)?;
        if read == 0 {
            break;
        }
        let start = offset;
        offset += read as u64;
        if let Some((span, ending, header_lines)) = active.as_mut() {
            if *header_lines > 0 {
                *header_lines -= 1;
                span.body_start = offset;
            } else if &line == ending {
                let (mut span, _, _) = active.take().expect("active message");
                span.body_end = start.saturating_sub(2).max(span.body_start);
                span.end = offset;
                result.push(span);
                raw_start = offset;
            }
            continue;
        }
        if let Some((role, id, created_at_ms, tool_call_count)) = start_marker(&line) {
            let block_start = start.saturating_sub(2).max(raw_start);
            if raw_start < block_start {
                result.push(raw_span(raw_start, block_start));
            }
            let ending = format!("<!-- codexify-{role}-message:v1:end id=\"{id}\" -->\n");
            active = Some((
                Span {
                    id,
                    role,
                    start: block_start,
                    body_start: offset,
                    body_end: offset,
                    end: offset,
                    created_at_ms,
                    tool_call_count,
                },
                ending,
                3,
            ));
        }
    }
    if active.is_some() {
        return Err("CHAT.md has an incomplete message; finish saving before retrying.".into());
    }
    if raw_start < offset {
        result.push(raw_span(raw_start, offset));
    }
    Ok(result)
}

fn widget_message(
    file: &mut File,
    span: Span,
    store: &super::super::attachments::AttachmentStore,
) -> Result<Option<WidgetMessage>, String> {
    if span.is_goal_update() {
        return Ok(None);
    }
    let text = read_range(file, span.body_start, span.body_end, MAX_UNREAD_BYTES)?;
    let markdown = String::from_utf8(text).map_err(|_| "CHAT.md contains incomplete UTF-8")?;
    if markdown.trim().is_empty() {
        return Ok(None);
    }
    let legacy_warning = (span.role == "agent"
        && markdown.starts_with("**Possible duplicate agent detected and blocked.**"))
        || (span.role == "warning"
            && markdown.trim() == "Duplicate agent detected. Its tool call was terminated.");
    let (role, mut markdown) = if legacy_warning {
        ("warning", crate::agent_tickets::WARNING.to_string())
    } else {
        (span.role, markdown)
    };
    let mut attachments = Vec::new();
    if role == "user"
        && let Some(plan) = store.message(&span.id)?
        && plan.full_markdown == markdown
    {
        markdown = plan.text;
        attachments = plan
            .attachments
            .iter()
            .map(|a| store.observe(&a.id))
            .collect::<Result<Vec<_>, _>>()?;
    }
    Ok(Some(WidgetMessage {
        id: span.id,
        role: role.into(),
        markdown,
        start: span.start,
        end: span.end,
        created_at_ms: span.created_at_ms,
        tool_call_count: span.tool_call_count,
        goal_status: None,
        goal_report: None,
        attachments,
    }))
}

fn transcript_revision(file: &File, cursor: &Cursor) -> Result<String, String> {
    let metadata = file.metadata().map_err(io_error)?;
    let modified = metadata
        .modified()
        .map_err(io_error)?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(format!("{}-{modified}-{}", metadata.len(), cursor.offset))
}

fn validate_checkpoint(file: &mut File, checkpoint: &TranscriptCheckpoint) -> Result<(), String> {
    let length = file.metadata().map_err(io_error)?.len();
    if checkpoint.anchor.len() > ANCHOR_BYTES
        || checkpoint.anchor.len() as u64 > checkpoint.end
        || checkpoint.end > length
        || read_range(
            file,
            checkpoint.end - checkpoint.anchor.len() as u64,
            checkpoint.end,
            ANCHOR_BYTES,
        )? != checkpoint.anchor
    {
        return Err(
            "CHAT.md changed before the browser-action transcript checkpoint; no fallback answer was persisted."
                .into(),
        );
    }
    Ok(())
}

impl ChatFile {
    /// A compact owner-view listing, without advancing the agent's read cursor.
    pub async fn owner_summary(self: &Arc<Self>) -> Result<OwnerChatSummary, String> {
        self.run(|chat| {
            chat.with_cursor(|cursor, file| {
                check_cursor(file, cursor)?;
                let custom_title = cursor
                    .custom_title
                    .as_deref()
                    .map(|title| super::super::title::plain_text(title, 200))
                    .filter(|title| !title.is_empty());
                let metadata = file.metadata().map_err(io_error)?;
                let modified = metadata.modified().map_err(io_error)?;
                let mut cache = chat
                    .owner_summary_cache
                    .lock()
                    .map_err(|_| "Owner chat summary is unavailable")?;
                if let Some(cached) = cache.as_ref()
                    && cached.length == metadata.len()
                    && cached.modified == modified
                {
                    return Ok(OwnerChatSummary {
                        title: custom_title.clone().unwrap_or_else(|| cached.title.clone()),
                        custom_title,
                        pinned_at_ms: cursor.pinned_at_ms,
                        archived_at_ms: cursor.archived_at_ms,
                        owner_read_through: cursor.owner_read_through,
                        last_entry_end: cached.last_entry_end,
                        last_incoming_entry_end: cached.last_incoming_entry_end,
                        last_entry_at_ms: cached.last_entry_at_ms,
                        last_user_entry_end: cached.last_user_entry_end,
                        last_user_entry_at_ms: cached.last_user_entry_at_ms,
                        last_agent_call_at_ms: (!cursor.agent_stop_requested)
                            .then_some(cursor.last_agent_call_at_ms)
                            .flatten(),
                        agent_stop_requested: cursor.agent_stop_requested,
                        agent_waiting_until_ms: (!cursor.agent_stop_requested)
                            .then(|| chat.agent_waiting_until_ms())
                            .flatten(),
                        agent_idle_until_ms: (!cursor.agent_stop_requested)
                            .then(|| chat.agent_idle_until_ms())
                            .flatten(),
                        agent_presence_until_ms: (!cursor.agent_stop_requested)
                            .then_some(cursor.agent_presence_until_ms)
                            .flatten(),
                        total_tool_calls: cursor.total_tool_calls,
                    });
                }
                let records: Vec<_> = spans(file)?
                    .into_iter()
                    .filter(|span| !span.is_goal_update())
                    .collect();
                let title: String = records
                    .iter()
                    .rev()
                    .find(|span| matches!(span.role, "user" | "agent"))
                    .and_then(|span| {
                        if span.role == "user"
                            && let Ok(store) =
                                super::super::attachments::AttachmentStore::for_chat(&chat.path)
                            && let Ok(Some(plan)) = store.message(&span.id)
                        {
                            let is_markdown = !plan.text.trim().is_empty();
                            let title = if !is_markdown {
                                plan.attachments
                                    .iter()
                                    .map(|a| a.original_name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            } else {
                                plan.text
                            };
                            return Some((title.into_bytes(), is_markdown));
                        }
                        let prefix = read_range(
                            file,
                            span.body_start,
                            span.body_end.min(span.body_start + 512),
                            512,
                        )
                        .ok()?;
                        // Formatting may close beyond the cheap preview. Read the
                        // bounded body only when inline Markdown needs that context.
                        if span.body_end > span.body_start + 512
                            && prefix.iter().any(|byte| b"*_`[~".contains(byte))
                        {
                            read_range(file, span.body_start, span.body_end, MAX_UNREAD_BYTES)
                                .ok()
                                .or(Some(prefix))
                        } else {
                            Some(prefix)
                        }
                        .map(|bytes| (bytes, true))
                    })
                    .map(|(bytes, is_markdown)| {
                        let text = String::from_utf8_lossy(&bytes);
                        if is_markdown {
                            super::super::title::plain_text(&text, 72)
                        } else {
                            text.split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ")
                                .chars()
                                .take(72)
                                .collect()
                        }
                    })
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| "Untitled conversation".into());
                let last = records.last();
                let last_user = records.iter().rev().find(|span| span.role == "user");
                let modified_ms = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let summary = OwnerChatSummary {
                    title: custom_title.clone().unwrap_or_else(|| title.clone()),
                    custom_title,
                    pinned_at_ms: cursor.pinned_at_ms,
                    archived_at_ms: cursor.archived_at_ms,
                    owner_read_through: cursor.owner_read_through,
                    last_entry_end: last.map_or(0, |span| span.end),
                    last_incoming_entry_end: records
                        .iter()
                        .rev()
                        .find(|span| matches!(span.role, "agent" | "warning"))
                        .map_or(0, |span| span.end),
                    last_entry_at_ms: last
                        .and_then(|span| span.created_at_ms)
                        .unwrap_or(modified_ms),
                    last_user_entry_end: last_user.map_or(0, |span| span.end),
                    // Manual appends lack timestamps. Keep the last known user timestamp
                    // so a legacy completed badge cannot reappear after such an append.
                    last_user_entry_at_ms: records
                        .iter()
                        .rev()
                        .filter(|span| span.role == "user")
                        .find_map(|span| span.created_at_ms),
                    last_agent_call_at_ms: (!cursor.agent_stop_requested)
                        .then_some(cursor.last_agent_call_at_ms)
                        .flatten(),
                    agent_stop_requested: cursor.agent_stop_requested,
                    agent_waiting_until_ms: (!cursor.agent_stop_requested)
                        .then(|| chat.agent_waiting_until_ms())
                        .flatten(),
                    agent_idle_until_ms: (!cursor.agent_stop_requested)
                        .then(|| chat.agent_idle_until_ms())
                        .flatten(),
                    agent_presence_until_ms: (!cursor.agent_stop_requested)
                        .then_some(cursor.agent_presence_until_ms)
                        .flatten(),
                    total_tool_calls: cursor.total_tool_calls,
                };
                *cache = Some(OwnerSummaryCache {
                    length: metadata.len(),
                    modified,
                    title,
                    last_entry_end: summary.last_entry_end,
                    last_incoming_entry_end: summary.last_incoming_entry_end,
                    last_entry_at_ms: summary.last_entry_at_ms,
                    last_user_entry_end: summary.last_user_entry_end,
                    last_user_entry_at_ms: summary.last_user_entry_at_ms,
                });
                Ok(summary)
            })
        })
        .await
    }

    #[doc(hidden)]
    pub async fn record_agent_call(self: &Arc<Self>, at_ms: u64) -> Result<(), String> {
        self.run(move |chat| {
            chat.with_cursor(|cursor, _| {
                let sequence = if cursor.tool_call_epoch.as_deref() == Some("direct") {
                    cursor.tool_call_sequence.saturating_add(1)
                } else {
                    1
                };
                let activity = super::super::AgentActivity {
                    at_ms,
                    epoch: "direct".into(),
                    sequence,
                };
                persist_agent_activity(chat, cursor, activity)
            })
        })
        .await
    }

    #[doc(hidden)]
    pub async fn finish_pairing_verification(
        self: &Arc<Self>,
        epoch: &str,
        sequence: u64,
        verified_at_ms: u64,
    ) -> Result<bool, String> {
        let epoch = epoch.to_string();
        self.run(move |chat| {
            chat.with_cursor(|cursor, _| {
                if cursor.agent_stop_requested
                    || cursor.tool_call_epoch.as_deref() != Some(epoch.as_str())
                    || cursor.tool_call_sequence != sequence
                    || cursor.last_agent_call_at_ms.is_none()
                {
                    return Ok(false);
                }
                let mut next = cursor.clone();
                next.agent_presence_until_ms =
                    Some(verified_at_ms.saturating_add(super::super::PAIRING_PRESENCE_GRACE_MS));
                chat.save_cursor(&next)?;
                *cursor = next;
                Ok(true)
            })
        })
        .await
    }

    pub(crate) async fn sync_agent_activity(
        self: &Arc<Self>,
        activity: super::super::AgentActivity,
    ) -> Result<(), String> {
        self.run(move |chat| {
            chat.with_cursor(|cursor, _| persist_agent_activity(chat, cursor, activity))
        })
        .await
    }

    pub async fn widget_page(
        self: &Arc<Self>,
        before: Option<u64>,
        known_revision: Option<String>,
    ) -> Result<WidgetPage, String> {
        self.run(move |chat| {
            chat.with_cursor(|cursor, file| {
                check_cursor(file, cursor)?;
                let metadata = file.metadata().map_err(io_error)?;
                let length = metadata.len();
                let modified = metadata
                    .modified()
                    .map_err(io_error)?
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let events_stamp = std::fs::metadata(chat.path.with_file_name("goal-events.json"))
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |time| time.as_nanos());
                let revision = format!(
                    "{length}-{modified}-{}-{}-{events_stamp}",
                    cursor.delivered_through, cursor.offset
                );
                let unchanged = before.is_none() && known_revision.as_deref() == Some(&revision);
                let mut page = WidgetPage {
                    offline_after_ms: chat.offline_after_ms(),
                    chat_file: chat.path.display().to_string(),
                    revision,
                    delivered_through: cursor.delivered_through,
                    read_through: cursor.offset,
                    last_agent_call_at_ms: (!cursor.agent_stop_requested)
                        .then_some(cursor.last_agent_call_at_ms)
                        .flatten(),
                    agent_stop_requested: cursor.agent_stop_requested,
                    agent_waiting_until_ms: (!cursor.agent_stop_requested)
                        .then(|| chat.agent_waiting_until_ms())
                        .flatten(),
                    agent_presence_until_ms: (!cursor.agent_stop_requested)
                        .then_some(cursor.agent_presence_until_ms)
                        .flatten(),
                    total_tool_calls: cursor.total_tool_calls,
                    server_time_ms: super::super::now_ms(),
                    messages: Vec::new(),
                    has_more: false,
                    before: None,
                    unchanged,
                };
                if unchanged {
                    return Ok(page);
                }
                if before.is_some_and(|offset| offset > length) {
                    return Err("Chat history position is beyond the current transcript.".into());
                }
                let records = spans(file)?;
                let mut bytes = 0;
                for span in records
                    .into_iter()
                    .rev()
                    .filter(|span| !span.is_goal_update())
                    .filter(|span| before.is_none_or(|end| span.start < end))
                {
                    if page.messages.len() >= PAGE_MESSAGES
                        || (bytes >= PAGE_BYTES && !page.messages.is_empty())
                    {
                        page.has_more = true;
                        break;
                    }
                    let Some(message) = widget_message(
                        file,
                        span,
                        &super::super::attachments::AttachmentStore::for_chat(&chat.path)?,
                    )?
                    else {
                        continue;
                    };
                    bytes += message.markdown.len();
                    page.before = Some(message.start);
                    page.messages.push(message);
                }
                // An event can precede the first transcript message (including an
                // otherwise silent goal finish). Include that boundary on the
                // oldest page rather than dropping the report after a later send.
                let first = if page.has_more {
                    page.messages.last().map_or(0, |message| message.start)
                } else {
                    0
                };
                for event in goal_events(chat)?.into_iter().filter(|event| {
                    event.offset >= first
                        && event.offset <= length
                        && before.is_none_or(|end| event.offset < end)
                }) {
                    page.messages.push(WidgetMessage {
                        id: event.id,
                        role: "goal".into(),
                        markdown: event.markdown,
                        start: event.offset,
                        end: event.offset,
                        created_at_ms: Some(event.created_at_ms),
                        tool_call_count: event.tool_call_count,
                        goal_status: Some(if event.blocked {
                            crate::agent_automation::GoalStatus::Blocked
                        } else {
                            crate::agent_automation::GoalStatus::Completed
                        }),
                        goal_report: event.report,
                        attachments: Vec::new(),
                    });
                }
                page.messages
                    .sort_by_key(|message| (message.start, message.role != "goal"));
                Ok(page)
            })
        })
        .await
    }

    pub async fn record_goal_finish(
        self: &Arc<Self>,
        goal: &crate::agent_automation::GoalRecord,
    ) -> Result<(), String> {
        use crate::agent_automation::GoalStatus;
        if !matches!(goal.status, GoalStatus::Completed | GoalStatus::Blocked) {
            return Ok(());
        }
        let Some(created_at_ms) = goal.finished_at_ms else {
            return Ok(());
        };
        let blocked = goal.status == GoalStatus::Blocked;
        let report = if blocked {
            &goal.blocked_reason
        } else {
            &goal.summary
        }
        .clone();
        let goal_created_at = goal.created_at_ms;
        let revision = goal.revision;
        let markdown = format!(
            "Goal {}: {}",
            if blocked { "blocked" } else { "completed" },
            goal.markdown
        );
        let offset = goal.completed_chat_end;
        let tool_call_count = goal.completed_tool_call_count;
        self.run(move |chat| {
            chat.with_cursor(|_, file| {
                // Older blocked receipts have no transcript boundary. Backfill them
                // once at the current end; new finishes persist the exact boundary.
                let offset = match offset {
                    Some(offset) => offset,
                    None if blocked => file.metadata().map_err(io_error)?.len(),
                    None => return Ok(()),
                };
                let event = GoalEvent {
                    id: if blocked {
                        format!("goal-blocked-{goal_created_at}-{revision}-{created_at_ms}")
                    } else {
                        format!("goal-completed-{goal_created_at}-{revision}")
                    },
                    markdown,
                    report,
                    blocked,
                    offset,
                    created_at_ms,
                    tool_call_count,
                };
                let mut events = goal_events(chat)?;
                if let Some(saved) = events.iter_mut().find(|saved| saved.id == event.id) {
                    // A polling reader may have backfilled a legacy receipt before
                    // goal_finish publishes its report and exact counter snapshot.
                    let mut changed = false;
                    if saved.tool_call_count.is_none() && event.tool_call_count.is_some() {
                        saved.tool_call_count = event.tool_call_count;
                        changed = true;
                    }
                    if saved.report.is_none() && event.report.is_some() {
                        saved.report = event.report;
                        changed = true;
                    }
                    if !changed {
                        return Ok(());
                    }
                } else {
                    events.push(event);
                }
                if offset > file.metadata().map_err(io_error)?.len() {
                    return Err("Goal report is outside this chat.".into());
                }
                let bytes = serde_json::to_vec(&events).map_err(|error| error.to_string())?;
                if bytes.len() as u64 > MAX_GOAL_EVENTS_BYTES {
                    return Err("Goal timeline is too large.".into());
                }
                let parent = chat.path.parent().ok_or("Chat has no directory")?;
                let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(io_error)?;
                temporary
                    .write_all(&bytes)
                    .and_then(|_| temporary.as_file().sync_all())
                    .map_err(io_error)?;
                temporary
                    .persist(parent.join("goal-events.json"))
                    .map_err(|error| io_error(error.error))?;
                Ok(())
            })
        })
        .await
    }

    pub async fn clean_markdown_transcript(self: &Arc<Self>) -> Result<String, String> {
        self.run(|chat| {
            chat.with_cursor(|cursor, file| {
                check_cursor(file, cursor)?;
                let mut sections = Vec::new();
                let mut total = 0usize;
                for span in spans(file)? {
                    let Some(message) = widget_message(
                        file,
                        span,
                        &super::super::attachments::AttachmentStore::for_chat(&chat.path)?,
                    )?
                    else {
                        continue;
                    };
                    let label = match message.role.as_str() {
                        "user" => "You",
                        "agent" => "Agent",
                        "warning" => "Warning",
                        _ => return Err("CHAT.md contains an unsupported message role".into()),
                    };
                    let files = message
                        .attachments
                        .iter()
                        .map(|a| format!("\n\nAttachment: {} ({})", a.original_name, a.path))
                        .collect::<String>();
                    let section = format!("## {label}\n\n{}{files}", message.markdown);
                    total = total
                        .saturating_add(section.len())
                        .saturating_add(usize::from(!sections.is_empty()) * 2);
                    if total > MAX_CLEAN_TRANSCRIPT_BYTES {
                        return Err(
                            "Clean Markdown transcript exceeds the 64 MiB safety ceiling.".into(),
                        );
                    }
                    sections.push(section);
                }
                Ok(sections.join("\n\n"))
            })
        })
        .await
    }

    pub(crate) async fn user_messages_read(
        self: &Arc<Self>,
        message_ids: &[String],
    ) -> Result<bool, String> {
        if message_ids.is_empty() {
            return Ok(false);
        }
        let message_ids = message_ids.to_vec();
        self.run(move |chat| {
            chat.with_cursor(|cursor, file| {
                check_cursor(file, cursor)?;
                let mut remaining = message_ids
                    .into_iter()
                    .collect::<std::collections::HashSet<_>>();
                for span in spans(file)? {
                    if span.role != "user" || span.end > cursor.offset {
                        continue;
                    }
                    let id = if span.id.starts_with("manual-") {
                        format!("manual-{}-{}", span.body_start, span.body_end)
                    } else {
                        span.id
                    };
                    remaining.remove(&id);
                }
                Ok(remaining.is_empty())
            })
        })
        .await
    }

    pub async fn unread_snapshot(self: &Arc<Self>) -> Result<UnreadSnapshot, String> {
        self.run(|chat| {
            chat.with_cursor(|cursor, file| {
                check_cursor(file, cursor)?;
                let end = file.metadata().map_err(io_error)?.len();
                let pending = read_range(file, cursor.offset, end, MAX_UNREAD_BYTES)?;
                std::str::from_utf8(&pending).map_err(|_| {
                    "CHAT.md contains incomplete UTF-8; finish saving before browser delivery."
                        .to_string()
                })?;
                let mut messages = Vec::new();
                for span in spans(file)? {
                    if span.role != "user" || span.end <= cursor.offset {
                        continue;
                    }
                    if span.start < cursor.offset {
                        return Err(
                            "The Markdown chat read cursor intersects a user message boundary."
                                .into(),
                        );
                    }
                    let markdown = String::from_utf8(read_range(
                        file,
                        span.body_start,
                        span.body_end,
                        MAX_UNREAD_BYTES,
                    )?)
                    .map_err(|_| "CHAT.md contains incomplete UTF-8".to_string())?;
                    if markdown.trim().is_empty() {
                        continue;
                    }
                    let id = if span.id.starts_with("manual-") {
                        format!("manual-{}-{}", span.body_start, span.body_end)
                    } else {
                        span.id
                    };
                    let markdown =
                        super::super::attachments::AttachmentStore::for_chat(&chat.path)?
                            .model_message(&id, &markdown);
                    messages.push(UnreadMessage {
                        id,
                        markdown,
                        start: span.start,
                        end: span.end,
                        created_at_ms: span.created_at_ms,
                        tool_call_count: span.tool_call_count,
                    });
                }
                Ok(UnreadSnapshot {
                    revision: transcript_revision(file, cursor)?,
                    read_through: cursor.offset,
                    end,
                    messages,
                })
            })
        })
        .await
    }

    pub async fn transcript_checkpoint(self: &Arc<Self>) -> Result<TranscriptCheckpoint, String> {
        self.run(|chat| {
            chat.with_cursor(|cursor, file| {
                check_cursor(file, cursor)?;
                let end = file.metadata().map_err(io_error)?.len();
                let anchor = read_range(
                    file,
                    end.saturating_sub(ANCHOR_BYTES as u64),
                    end,
                    ANCHOR_BYTES,
                )?;
                Ok(TranscriptCheckpoint {
                    revision: transcript_revision(file, cursor)?,
                    end,
                    anchor,
                })
            })
        })
        .await
    }

    pub async fn has_agent_message_after(
        self: &Arc<Self>,
        checkpoint: &TranscriptCheckpoint,
    ) -> Result<bool, String> {
        let checkpoint = checkpoint.clone();
        self.run(move |chat| {
            chat.with_cursor(|cursor, file| {
                check_cursor(file, cursor)?;
                validate_checkpoint(file, &checkpoint)?;
                Ok(spans(file)?
                    .into_iter()
                    .any(|span| span.role == "agent" && span.start >= checkpoint.end))
            })
        })
        .await
    }

    pub async fn append_browser_answer(
        self: &Arc<Self>,
        action_id: &str,
        message: &str,
    ) -> Result<BrowserAppendReceipt, String> {
        let id = format!("browser-{action_id}");
        if !valid_id(action_id) || !valid_id(&id) {
            return Err("Invalid browser action ID.".into());
        }
        if message.trim().is_empty() {
            return Err("Browser answer must not be blank.".into());
        }
        if message.len() > MAX_UNREAD_BYTES {
            return Err(
                "Browser answer exceeds the 16 MiB safety ceiling; nothing was appended.".into(),
            );
        }
        let message = message.to_string();
        self.run(move |chat| {
            chat.with_cursor(|cursor, file| {
                check_cursor(file, cursor)?;
                for span in spans(file)? {
                    if span.id != id {
                        continue;
                    }
                    let existing = read_range(
                        file,
                        span.body_start,
                        span.body_end,
                        MAX_UNREAD_BYTES,
                    )?;
                    if span.role == "agent" && existing == message.as_bytes() {
                        return Ok(BrowserAppendReceipt {
                            id,
                            end: span.end,
                            appended: false,
                        });
                    }
                    return Err(
                        "This browser action ID already belongs to different transcript content."
                            .into(),
                    );
                }

                let block = agent_block_with_id(
                    &id,
                    &message,
                    cursor.total_tool_calls,
                    super::super::now_ms(),
                );
                file.write_all(block.as_bytes()).map_err(io_error)?;
                let end = file.stream_position().map_err(io_error)?;
                file.sync_all().map_err(io_error)?;
                let same = same_file::Handle::from_file(file.try_clone().map_err(io_error)?)
                    .and_then(|handle| {
                        same_file::Handle::from_path(&chat.path)
                            .map(|current| current == handle)
                    })
                    .map_err(io_error)?;
                if !same {
                    return Err(
                        "CHAT.md changed during browser-answer persistence; reload before retrying with the same action ID."
                            .into(),
                    );
                }
                Ok(BrowserAppendReceipt {
                    id,
                    end,
                    appended: true,
                })
            })
        })
        .await
    }

    pub async fn append_user(
        self: &Arc<Self>,
        id: String,
        message: String,
    ) -> Result<UserSendReceipt, String> {
        self.append_user_with_attachments(id, message, Vec::new())
            .await
    }

    pub async fn append_user_with_attachments(
        self: &Arc<Self>,
        id: String,
        message: String,
        attachment_ids: Vec<String>,
    ) -> Result<UserSendReceipt, String> {
        self.run(move |chat| super::queue::append_steering(chat, id, message, attachment_ids))
            .await
    }

    pub async fn list_attachments(
        self: &Arc<Self>,
    ) -> Result<Vec<super::super::attachments::Attachment>, String> {
        self.run(|chat| {
            chat.with_cursor(|_, file| {
                let store = super::super::attachments::AttachmentStore::for_chat(&chat.path)?;
                let mut result = Vec::new();
                for span in spans(file)?.into_iter().filter(|s| s.role == "user") {
                    if let Some(plan) = store.message(&span.id)? {
                        let body =
                            read_range(file, span.body_start, span.body_end, MAX_UNREAD_BYTES)?;
                        if body != plan.full_markdown.as_bytes() {
                            continue;
                        }
                        for a in plan.attachments {
                            let current = store.observe(&a.id)?;
                            if current.message_id.as_deref() == Some(span.id.as_str()) {
                                result.push(current);
                            }
                        }
                    }
                }
                result.sort_by(|a, b| {
                    b.submitted_at_ms
                        .cmp(&a.submitted_at_ms)
                        .then_with(|| b.created_at_ms.cmp(&a.created_at_ms))
                        .then_with(|| b.id.cmp(&a.id))
                });
                Ok(result)
            })
        })
        .await
    }

    pub async fn get_attachment(
        self: &Arc<Self>,
        id: &str,
    ) -> Result<super::super::attachments::Attachment, String> {
        self.list_attachments()
            .await?
            .into_iter()
            .find(|a| a.id == id || a.unique_name == id)
            .ok_or_else(|| "Submitted attachment not found in this conversation".into())
    }
    pub async fn uploaded_attachment(
        self: &Arc<Self>,
        id: String,
    ) -> Result<super::super::attachments::Attachment, String> {
        self.run(move |chat| {
            chat.with_cursor(|_, _| {
                super::super::attachments::AttachmentStore::for_chat(&chat.path)?.observe(&id)
            })
        })
        .await
    }

    pub async fn mark_owner_read(self: &Arc<Self>, through: u64) -> Result<u64, String> {
        self.run(move |chat| {
            chat.with_cursor(|cursor, file| {
                check_cursor(file, cursor)?;
                if through > file.metadata().map_err(io_error)?.len() {
                    return Err("Read position is beyond the current transcript.".into());
                }
                if through > cursor.owner_read_through {
                    let mut next = cursor.clone();
                    next.owner_read_through = through;
                    chat.save_cursor(&next)?;
                    *cursor = next;
                }
                Ok(cursor.owner_read_through)
            })
        })
        .await
    }

    pub async fn set_owner_organization(
        self: &Arc<Self>,
        pinned: Option<bool>,
        archived: Option<bool>,
    ) -> Result<OwnerChatOrganization, String> {
        if pinned.is_some() == archived.is_some() {
            return Err("Change either pinning or archiving, one at a time.".into());
        }
        self.run(move |chat| {
            chat.with_cursor(|cursor, _| {
                let mut next = cursor.clone();
                if let Some(pinned) = pinned {
                    if pinned && next.archived_at_ms.is_some() {
                        return Err("Restore the archived chat before pinning it.".into());
                    }
                    next.pinned_at_ms = if pinned {
                        Some(next.pinned_at_ms.unwrap_or_else(super::super::now_ms))
                    } else {
                        None
                    };
                }
                if let Some(archived) = archived {
                    next.archived_at_ms = if archived {
                        Some(next.archived_at_ms.unwrap_or_else(super::super::now_ms))
                    } else {
                        None
                    };
                    if archived {
                        next.pinned_at_ms = None;
                    }
                }
                if next.pinned_at_ms != cursor.pinned_at_ms
                    || next.archived_at_ms != cursor.archived_at_ms
                {
                    chat.save_cursor(&next)?;
                    *cursor = next;
                }
                Ok(OwnerChatOrganization {
                    pinned_at_ms: cursor.pinned_at_ms,
                    archived_at_ms: cursor.archived_at_ms,
                })
            })
        })
        .await
    }

    pub async fn request_agent_stop(self: &Arc<Self>, request_id: String) -> Result<(), String> {
        if !valid_id(&request_id) {
            return Err("Invalid agent stop request ID.".into());
        }
        self.run(move |chat| {
            chat.with_cursor(|cursor, _| {
                if cursor.agent_stop_requests.contains(&request_id) {
                    return Ok(());
                }
                let mut next = cursor.clone();
                next.agent_stop_requested = true;
                next.agent_stop_requests.push(request_id);
                if next.agent_stop_requests.len() > 32 {
                    next.agent_stop_requests.remove(0);
                }
                chat.save_cursor(&next)?;
                *cursor = next;
                chat.clear_agent_waiting();
                Ok(())
            })
        })
        .await
    }

    pub async fn agent_stop_requested(self: &Arc<Self>) -> Result<bool, String> {
        self.run(|chat| chat.with_cursor(|cursor, _| Ok(cursor.agent_stop_requested)))
            .await
    }

    pub async fn resume_agent(self: &Arc<Self>) -> Result<(), String> {
        self.run(|chat| chat.with_cursor(|cursor, _| resume_agent(chat, cursor)))
            .await
    }

    pub async fn mark_delivered(self: &Arc<Self>, end: u64) -> Result<(), String> {
        self.run(move |chat| {
            chat.with_cursor(|cursor, file| {
                check_cursor(file, cursor)?;
                if end > file.metadata().map_err(io_error)?.len() {
                    return Err("Cannot acknowledge a message beyond CHAT.md.".into());
                }
                if end > cursor.delivered_through {
                    let mut next = cursor.clone();
                    next.delivered_through = end;
                    chat.save_cursor(&next)?;
                    *cursor = next;
                }
                Ok(())
            })
        })
        .await
    }
}

fn resume_agent(chat: &ChatFile, cursor: &mut Cursor) -> Result<(), String> {
    if cursor.agent_stop_requested {
        let mut next = cursor.clone();
        next.agent_stop_requested = false;
        next.last_agent_call_at_ms = None;
        next.agent_presence_until_ms = None;
        chat.save_cursor(&next)?;
        *cursor = next;
    }
    Ok(())
}

fn persist_agent_activity(
    chat: &ChatFile,
    cursor: &mut Cursor,
    activity: super::super::AgentActivity,
) -> Result<(), String> {
    let mut next = cursor.clone();
    let sequence_delta = if next.tool_call_epoch.as_deref() == Some(activity.epoch.as_str()) {
        activity.sequence.saturating_sub(next.tool_call_sequence)
    } else {
        activity.sequence
    };
    next.total_tool_calls = next.total_tool_calls.saturating_add(sequence_delta);
    if next.tool_call_epoch.as_deref() != Some(activity.epoch.as_str())
        || activity.sequence > next.tool_call_sequence
    {
        next.last_agent_call_at_ms = Some(
            next.last_agent_call_at_ms
                .unwrap_or_default()
                .max(activity.at_ms),
        );
        next.tool_call_epoch = Some(activity.epoch);
        next.tool_call_sequence = activity.sequence;
        next.agent_presence_until_ms = None;
    }
    if next.last_agent_call_at_ms != cursor.last_agent_call_at_ms
        || next.total_tool_calls != cursor.total_tool_calls
        || next.tool_call_epoch != cursor.tool_call_epoch
        || next.tool_call_sequence != cursor.tool_call_sequence
    {
        chat.save_cursor(&next)?;
        *cursor = next;
    }
    Ok(())
}

pub(super) fn user_text(text: &str, chat_path: &Path) -> Result<String, String> {
    let attachments = super::super::attachments::AttachmentStore::for_chat(chat_path)?;
    let mut output = String::new();
    let mut remaining = text;
    loop {
        let candidate = [
            ("agent", AGENT_START),
            ("user", USER_START),
            ("warning", WARNING_START),
        ]
        .into_iter()
        .filter_map(|(role, prefix)| remaining.find(prefix).map(|start| (start, role, prefix)))
        .min_by_key(|entry| entry.0);
        let Some((start, role, prefix)) = candidate else {
            break;
        };
        output.push_str(&remaining[..start]);
        let marker = &remaining[start + prefix.len()..];
        let heading = match role {
            "user" => "User",
            "warning" => "Warning",
            _ => "Agent",
        };
        let opening = format!("\" -->\n\n## {heading}\n\n");
        let Some((fields, body)) = marker.split_once(&opening) else {
            return Err("CHAT.md has an incomplete message; finish saving before retrying.".into());
        };
        let Some((id, created_at_ms, _)) = marker_fields(role, fields) else {
            return Err("CHAT.md has a malformed message marker.".into());
        };
        let ending = format!("\n\n<!-- codexify-{role}-message:v1:end id=\"{id}\" -->\n");
        let Some((message, tail)) = body.split_once(&ending) else {
            return Err("CHAT.md has an incomplete message; finish saving before retrying.".into());
        };
        if role == "user" {
            let message = attachments.model_message(id, message);
            output.push_str(&super::super::output::timestamped_user_message(
                &message,
                created_at_ms,
            ));
            output.push_str("\n\n");
        }
        remaining = tail;
    }
    output.push_str(remaining);
    Ok(output)
}

pub(super) fn contains_user_message(file: &mut File, id: &str) -> Result<bool, String> {
    Ok(spans(file)?
        .iter()
        .any(|span| span.role == "user" && span.id == id))
}

pub(super) fn user_receipt(file: &mut File, id: &str) -> Result<Option<UserSendReceipt>, String> {
    Ok(spans(file)?
        .into_iter()
        .find(|span| span.role == "user" && span.id == id)
        .map(|span| UserSendReceipt {
            id: span.id,
            end: span.end,
            created_at_ms: span.created_at_ms,
            tool_call_count: span.tool_call_count,
        }))
}

pub(super) fn append_user_locked(
    chat: &ChatFile,
    cursor: &mut Cursor,
    file: &mut File,
    id: String,
    message: String,
    attachment_ids: Vec<String>,
) -> Result<UserSendReceipt, String> {
    if !valid_id(&id) {
        return Err("Invalid chat message request ID.".into());
    }
    if message.trim().is_empty() && attachment_ids.is_empty() {
        return Err("User message must not be blank.".into());
    }
    if message.len() > MAX_UNREAD_BYTES {
        return Err("User message exceeds the 16 MiB safety ceiling; nothing was appended.".into());
    }
    check_cursor(file, cursor)?;
    let created_at_ms = super::super::now_ms();
    let message = if attachment_ids.is_empty() {
        message
    } else {
        let committed = spans(file)?
            .into_iter()
            .filter(|s| s.role == "user")
            .map(|s| s.id)
            .collect();
        super::super::attachments::AttachmentStore::for_chat(&chat.path)?
            .prepare_message(&id, &message, &attachment_ids, &committed, created_at_ms)?
            .full_markdown
    };
    if message.len() > MAX_UNREAD_BYTES {
        return Err("Message and attachment metadata exceed the chat size limit".into());
    }
    for span in spans(file)? {
        if span.role == "user" && span.id == id {
            if read_range(file, span.body_start, span.body_end, MAX_UNREAD_BYTES)?
                != message.as_bytes()
            {
                return Err("This request ID already belongs to a different message.".into());
            }
            if span.end > cursor.offset {
                chat.clear_agent_waiting();
            }
            return Ok(UserSendReceipt {
                id,
                end: span.end,
                created_at_ms: span.created_at_ms,
                tool_call_count: span.tool_call_count,
            });
        }
    }
    let tool_call_count = cursor.total_tool_calls;
    let block = format!(
        "{USER_START}{id}\" created_at_ms=\"{created_at_ms}\" tool_call_count=\"{tool_call_count}\" -->\n\n## User\n\n{message}\n\n<!-- codexify-user-message:v1:end id=\"{id}\" -->\n"
    );
    file.write_all(block.as_bytes()).map_err(io_error)?;
    let end = file.stream_position().map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    let same = same_file::Handle::from_file(file.try_clone().map_err(io_error)?)
        .and_then(|handle| {
            same_file::Handle::from_path(&chat.path).map(|current| current == handle)
        })
        .map_err(io_error)?;
    if !same {
        return Err(
            "CHAT.md changed during send; reload and retry with the same request ID.".into(),
        );
    }
    resume_agent(chat, cursor)?;
    chat.clear_agent_waiting();
    Ok(UserSendReceipt {
        id,
        end,
        created_at_ms: Some(created_at_ms),
        tool_call_count: Some(tool_call_count),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn owner_titles_strip_markdown_before_shortening_and_keep_messages_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("CHAT.md");
        let chat = Arc::new(ChatFile::new(path.clone(), true));
        let wording = "It should not require a major redesign. ".repeat(20);
        let markdown = format!(
            "**{}** Use `session.sh` and [the guide](https://example.com).",
            wording.trim_end()
        );
        let expected: String = wording.chars().take(72).collect();
        let expected = expected.trim_end().to_owned();
        chat.append(markdown.clone()).await.unwrap();
        assert_eq!(chat.owner_summary().await.unwrap().title, expected);
        chat.append_user("pending-input".into(), markdown.clone())
            .await
            .unwrap();
        let original = std::fs::read(&path).unwrap();
        let unread = chat.unread_snapshot().await.unwrap();
        assert!(!unread.messages.is_empty());
        for current in [
            chat.clone(),
            chat.clone(),
            Arc::new(ChatFile::new(path.clone(), true)),
        ] {
            assert_eq!(current.owner_summary().await.unwrap().title, expected);
        }
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(chat.unread_snapshot().await.unwrap(), unread);
        assert_eq!(
            chat.widget_page(None, None)
                .await
                .unwrap()
                .messages
                .last()
                .unwrap()
                .markdown,
            markdown
        );
    }

    #[tokio::test]
    async fn owner_titles_keep_attachment_filenames_literal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("CHAT.md");
        let chat = Arc::new(ChatFile::new(path.clone(), true));
        chat.ensure().await.unwrap();
        let store = super::super::super::attachments::AttachmentStore::for_chat(&path).unwrap();
        let file = store
            .upload(
                "filename-title",
                "__init__.py",
                "text/plain",
                Some(1),
                false,
                b"x".to_vec(),
            )
            .unwrap();
        chat.append_user_with_attachments("attachment-message".into(), "".into(), vec![file.id])
            .await
            .unwrap();
        assert_eq!(chat.owner_summary().await.unwrap().title, "__init__.py");
    }

    #[tokio::test]
    async fn owner_titles_normalize_new_names_and_legacy_names_on_both_cache_paths() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("CHAT.md");
        let chat = Arc::new(ChatFile::new(path.clone(), true));
        chat.append("Automatic title".into()).await.unwrap();
        assert_eq!(chat.owner_summary().await.unwrap().title, "Automatic title");
        // An existing saved name can contain Markdown without a new rename.
        chat.run(|chat| {
            chat.with_cursor(|cursor, _| {
                cursor.custom_title = Some("**Existing** `chat`".into());
                chat.save_cursor(cursor)
            })
        })
        .await
        .unwrap();
        for current in [chat.clone(), Arc::new(ChatFile::new(path, true))] {
            let summary = current.owner_summary().await.unwrap();
            assert_eq!(summary.title, "Existing chat");
            assert_eq!(summary.custom_title.as_deref(), Some("Existing chat"));
        }
        let saved = chat
            .run(|chat| chat.with_cursor(|cursor, _| Ok(cursor.custom_title.clone())))
            .await
            .unwrap();
        assert_eq!(
            saved.as_deref(),
            Some("**Existing** `chat`"),
            "display normalization must not rewrite existing metadata"
        );
        chat.set_owner_title("## My **plain** `chat`".into())
            .await
            .unwrap();
        let summary = chat.owner_summary().await.unwrap();
        assert_eq!(summary.title, "My plain chat");
        assert_eq!(summary.custom_title.as_deref(), Some("My plain chat"));
        for (raw, shown) in [
            ("`__init__.py`", "__init__.py"),
            (r"\*literal\*", "*literal*"),
        ] {
            chat.set_owner_title(raw.into()).await.unwrap();
            assert_eq!(chat.owner_summary().await.unwrap().title, shown);
            chat.set_owner_title(shown.into()).await.unwrap();
            assert_eq!(chat.owner_summary().await.unwrap().title, shown);
        }
        chat.set_owner_title("---".into()).await.unwrap();
        let summary = chat.owner_summary().await.unwrap();
        assert_eq!(summary.title, "Automatic title");
        assert!(summary.custom_title.is_none());
    }

    #[tokio::test]
    async fn goal_completion_is_durable_exactly_once_and_does_not_consume_user_messages() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("chats/task/CHAT.md");
        let chat = Arc::new(ChatFile::new(path.clone(), true));
        chat.record_agent_call(1000).await.unwrap();
        chat.append("Before completion".into()).await.unwrap();
        chat.append_user("pending-goal-event".into(), "Keep this unread".into())
            .await
            .unwrap();
        let unread = chat.unread_snapshot().await.unwrap();
        for call in 2..=11 {
            chat.record_agent_call(1000 + call).await.unwrap();
        }
        let offset = std::fs::metadata(&path).unwrap().len();
        let goal: crate::agent_automation::GoalRecord = serde_json::from_value(serde_json::json!({
            "revision":2,"markdown":"Review and analyze this PR","status":"completed",
            "createdAtMs":1000,"updatedAtMs":2000,"finishedAtMs":2000,
            "summary":null,"blockedReason":null,"completedChatEnd":offset,"completedToolCallCount":11
        })).unwrap();
        let old_page = chat.widget_page(None, None).await.unwrap();
        let mut legacy_receipt = goal.clone();
        legacy_receipt.completed_tool_call_count = None;
        chat.record_goal_finish(&legacy_receipt).await.unwrap();
        chat.record_goal_finish(&goal).await.unwrap();
        assert_eq!(chat.unread_snapshot().await.unwrap(), unread);
        chat.record_agent_call(3000).await.unwrap();
        chat.record_goal_finish(&goal).await.unwrap();
        let reopened = Arc::new(ChatFile::new(path, true));
        let page = reopened
            .widget_page(None, Some(old_page.revision))
            .await
            .unwrap();
        assert!(!page.unchanged);
        let events = page
            .messages
            .iter()
            .filter(|message| message.role == "goal")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].start, offset);
        assert_eq!(events[0].tool_call_count, Some(11));
        assert_eq!(events[0].created_at_ms, Some(2000));
        assert!(events[0].markdown.contains("Review and analyze this PR"));
        assert!(
            serde_json::to_value(&goal)
                .unwrap()
                .get("completedToolCallCount")
                .is_none()
        );
        assert_eq!(
            reopened.unread_snapshot().await.unwrap().messages,
            unread.messages
        );
    }

    #[tokio::test]
    async fn legacy_blocker_is_backfilled_once_and_legacy_completion_stays_readable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("CHAT.md");
        let chat = Arc::new(ChatFile::new(path.clone(), true));
        chat.append("Prior progress".into()).await.unwrap();
        let offset = std::fs::metadata(&path).unwrap().len();
        let legacy = serde_json::json!([{
            "id":"goal-completed-1-1", "markdown":"Goal completed: Old objective",
            "offset":0, "created_at_ms":2, "tool_call_count":null
        }]);
        std::fs::write(
            path.with_file_name("goal-events.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        let goal = serde_json::from_value(serde_json::json!({
            "revision":2,"markdown":"Current objective","status":"blocked", "createdAtMs":3,
            "updatedAtMs":4,"finishedAtMs":4,"summary":null,"blockedReason":"Need device access"
        }))
        .unwrap();
        chat.record_goal_finish(&goal).await.unwrap();
        chat.append("Later progress".into()).await.unwrap();
        let reopened = Arc::new(ChatFile::new(path, true));
        reopened.record_goal_finish(&goal).await.unwrap();
        let page = reopened.widget_page(None, None).await.unwrap();
        let events = page
            .messages
            .iter()
            .filter(|m| m.role == "goal")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        assert!(events[0].goal_report.is_none());
        assert_eq!(
            events[1].start, offset,
            "backfill position must remain fixed"
        );
        assert_eq!(events[1].goal_report.as_deref(), Some("Need device access"));
        assert_eq!(
            events[1].goal_status,
            Some(crate::agent_automation::GoalStatus::Blocked)
        );
    }

    #[tokio::test]
    async fn pairing_presence_lease_survives_reopen_and_expires_without_waiting_five_minutes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("chats/pair/CHAT.md");
        let chat = Arc::new(ChatFile::new(path.clone(), true));
        chat.record_agent_call(1000).await.unwrap();
        assert!(
            chat.finish_pairing_verification("direct", 1, 1000)
                .await
                .unwrap()
        );
        let reopened = Arc::new(ChatFile::new(path, true));
        let activity = reopened.offline_activity().await.unwrap();
        assert!(activity.is_online(5999, super::super::super::AUTOMATION_OFFLINE_AFTER_MS));
        assert!(!activity.is_online(6000, super::super::super::AUTOMATION_OFFLINE_AFTER_MS));
        assert!(activity.last_call_for_presence(6000).is_none());
        let summary = reopened.owner_summary().await.unwrap();
        assert_eq!(summary.last_agent_call_at_ms, Some(1000));
        assert_eq!(summary.agent_presence_until_ms, Some(6000));
        assert_eq!(summary.total_tool_calls, 1);
        assert!(
            !reopened
                .claim_offline_automation_goal(5999, 240000)
                .await
                .unwrap()
        );
        assert!(
            reopened
                .claim_offline_automation_goal(6000, 240000)
                .await
                .unwrap()
        );
        assert!(
            !reopened
                .claim_offline_automation_goal(6001, 240000)
                .await
                .unwrap()
        );
        assert!(
            !reopened
                .claim_offline_notification(6000, 300000)
                .await
                .unwrap()
        );
    }
    #[tokio::test]
    async fn normal_activity_cancels_pairing_expiry_and_a_late_verification_cannot_replace_it() {
        let root = tempfile::tempdir().unwrap();
        let chat = Arc::new(ChatFile::new(root.path().join("chats/pair/CHAT.md"), true));
        chat.record_agent_call(1000).await.unwrap();
        assert!(
            chat.finish_pairing_verification("direct", 1, 1000)
                .await
                .unwrap()
        );
        // Same-millisecond calls still have distinct activity sequences.
        chat.record_agent_call(1000).await.unwrap();
        assert!(
            !chat
                .finish_pairing_verification("direct", 1, 1000)
                .await
                .unwrap()
        );
        let summary = chat.owner_summary().await.unwrap();
        assert!(summary.agent_presence_until_ms.is_none());
        assert_eq!(summary.total_tool_calls, 2);
        assert!(
            chat.offline_activity()
                .await
                .unwrap()
                .is_online(6000, 240000)
        );
    }

    fn activity(epoch: &str, sequence: u64, at_ms: u64) -> super::super::super::AgentActivity {
        super::super::super::AgentActivity {
            at_ms,
            epoch: epoch.into(),
            sequence,
        }
    }

    #[tokio::test]
    async fn activity_sequences_are_idempotent_out_of_order_and_continue_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat/CHAT.md");
        let chat = Arc::new(ChatFile::new(path.clone(), true));

        chat.sync_agent_activity(activity("first", 2, 2000))
            .await
            .unwrap();
        chat.sync_agent_activity(activity("first", 1, 1000))
            .await
            .unwrap();
        chat.sync_agent_activity(activity("first", 4, 3000))
            .await
            .unwrap();
        chat.sync_agent_activity(activity("second", 1, 4000))
            .await
            .unwrap();
        chat.append("Counted response".into()).await.unwrap();

        let reopened = Arc::new(ChatFile::new(path, true));
        let page = reopened.widget_page(None, None).await.unwrap();
        assert_eq!(page.total_tool_calls, 5);
        assert_eq!(page.last_agent_call_at_ms, Some(4000));
        assert_eq!(page.messages[0].tool_call_count, Some(5));
    }

    #[tokio::test]
    async fn old_duplicate_warning_is_displayed_as_a_warning() {
        let directory = tempfile::tempdir().unwrap();
        let chat = Arc::new(ChatFile::new(directory.path().join("CHAT.md"), true));
        chat.ensure().await.unwrap();
        chat.append("**Possible duplicate agent detected and blocked.** Old explanation.".into())
            .await
            .unwrap();
        let page = chat.widget_page(None, None).await.unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].role, "warning");
        assert_eq!(page.messages[0].markdown, crate::agent_tickets::WARNING);
    }

    #[tokio::test]
    async fn old_ticket_warning_uses_current_copy_without_rewriting_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("CHAT.md");
        let chat = Arc::new(ChatFile::new(path.clone(), true));
        chat.ensure().await.unwrap();
        chat.warn_ticket_rejection().await.unwrap();
        let historical = std::fs::read_to_string(&path).unwrap().replace(
            crate::agent_tickets::WARNING,
            "Duplicate agent detected. Its tool call was terminated.",
        );
        std::fs::write(&path, &historical).unwrap();
        let reopened = Arc::new(ChatFile::new(path.clone(), true));
        let page = reopened.widget_page(None, None).await.unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].role, "warning");
        assert_eq!(page.messages[0].markdown, crate::agent_tickets::WARNING);
        assert!(page.messages[0].created_at_ms.is_some());
        assert_eq!(std::fs::read_to_string(path).unwrap(), historical);
    }

    #[tokio::test]
    async fn clean_markdown_transcript_preserves_source_and_removes_storage_markers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("CHAT.md");
        let chat = Arc::new(ChatFile::new(path.clone(), true));
        chat.ensure().await.unwrap();
        chat.append_user(
            "source-user".into(),
            "Paragraph  \n\n```rust\nlet x = 1;\n```".into(),
        )
        .await
        .unwrap();
        chat.append("Agent *source*".into()).await.unwrap();
        chat.warn_ticket_rejection().await.unwrap();

        let transcript = chat.clean_markdown_transcript().await.unwrap();
        assert_eq!(
            transcript,
            format!(
                "## You\n\nParagraph  \n\n```rust\nlet x = 1;\n```\n\n## Agent\n\nAgent *source*\n\n## Warning\n\n{}",
                crate::agent_tickets::WARNING
            )
        );
        assert!(!transcript.contains("codexify-user-message"));
        assert!(!transcript.contains("# Codexify Chat"));
    }

    #[tokio::test]
    async fn empty_clean_markdown_transcript_is_empty() {
        let directory = tempfile::tempdir().unwrap();
        let chat = Arc::new(ChatFile::new(directory.path().join("CHAT.md"), true));
        chat.ensure().await.unwrap();

        assert_eq!(chat.clean_markdown_transcript().await.unwrap(), "");
    }

    #[tokio::test]
    async fn owner_unread_tracks_incoming_messages_and_not_own_sends_or_agent_receipts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("CHAT.md");
        let chat = Arc::new(ChatFile::new(path.clone(), true));
        chat.append_user("own-first".into(), "My first message".into())
            .await
            .unwrap();
        for _ in 0..2 {
            let summary = chat.owner_summary().await.unwrap();
            assert!(summary.last_entry_end > 0);
            assert_eq!(summary.last_incoming_entry_end, 0);
        }
        chat.append("First incoming response".into()).await.unwrap();
        let incoming = chat.owner_summary().await.unwrap().last_incoming_entry_end;
        assert!(incoming > 0);
        chat.mark_owner_read(incoming).await.unwrap();
        chat.append_user("own-reply".into(), "My own reply".into())
            .await
            .unwrap();
        let outgoing = chat.owner_summary().await.unwrap().last_entry_end;
        let transcript = std::fs::read(&path).unwrap();
        chat.mark_delivered(outgoing).await.unwrap();
        for consume in [false, true] {
            chat.read(consume).await.unwrap();
            chat.record_agent_call(super::super::super::now_ms())
                .await
                .unwrap();
            for _ in 0..2 {
                let summary = chat.owner_summary().await.unwrap();
                assert!(summary.last_entry_end > incoming);
                assert_eq!(summary.last_incoming_entry_end, incoming);
                assert_eq!(summary.owner_read_through, incoming);
            }
        }
        assert_eq!(std::fs::read(&path).unwrap(), transcript);
        chat.append("Unread incoming response".into())
            .await
            .unwrap();
        let unseen = chat.owner_summary().await.unwrap().last_incoming_entry_end;
        chat.append_user("own-later".into(), "My later message".into())
            .await
            .unwrap();
        let reopened = Arc::new(ChatFile::new(path, true));
        for _ in 0..2 {
            let summary = reopened.owner_summary().await.unwrap();
            assert!(summary.last_entry_end > unseen);
            assert_eq!(summary.last_incoming_entry_end, unseen);
            assert!(summary.last_incoming_entry_end > summary.owner_read_through);
        }
        reopened.mark_owner_read(unseen).await.unwrap();
        reopened.warn_ticket_rejection().await.unwrap();
        let warning = reopened.owner_summary().await.unwrap();
        assert_eq!(warning.last_incoming_entry_end, warning.last_entry_end);
        assert!(warning.last_incoming_entry_end > warning.owner_read_through);
    }

    #[tokio::test]
    async fn owner_read_position_is_durable_monotonic_and_independent_of_agent_receipts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("CHAT.md");
        let chat = Arc::new(ChatFile::new(path.clone(), true));
        chat.append_user("unread-input".into(), "Continue".into())
            .await
            .unwrap();
        let first = chat.owner_summary().await.unwrap().last_entry_end;
        chat.append("First response".into()).await.unwrap();
        let second = chat.owner_summary().await.unwrap().last_entry_end;
        chat.append_user("pending-input".into(), "New input for the agent".into())
            .await
            .unwrap();
        let transcript = std::fs::read(&path).unwrap();
        let unread = chat.unread_snapshot().await.unwrap();
        assert!(!unread.messages.is_empty());
        let before: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.with_file_name("cursor.json")).unwrap())
                .unwrap();
        assert_eq!(chat.owner_summary().await.unwrap().owner_read_through, 0);
        let (older, newer) =
            tokio::join!(chat.mark_owner_read(first), chat.mark_owner_read(second));
        assert!(older.unwrap() >= first);
        assert_eq!(newer.unwrap(), second);
        assert_eq!(chat.mark_owner_read(first).await.unwrap(), second);
        assert_eq!(chat.mark_owner_read(second).await.unwrap(), second);
        assert_eq!(
            chat.owner_summary().await.unwrap().owner_read_through,
            second
        );
        assert!(
            chat.mark_owner_read(transcript.len() as u64 + 1)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), transcript);
        assert_eq!(chat.unread_snapshot().await.unwrap(), unread);
        let mut after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.with_file_name("cursor.json")).unwrap())
                .unwrap();
        after["ownerReadThrough"] = before["ownerReadThrough"].clone();
        assert_eq!(after, before, "only the owner's read position changes");

        let reopened = Arc::new(ChatFile::new(path.clone(), true));
        assert_eq!(
            reopened.owner_summary().await.unwrap().owner_read_through,
            second
        );
        reopened.append("New response".into()).await.unwrap();
        let summary = reopened.owner_summary().await.unwrap();
        assert!(summary.last_entry_end > summary.owner_read_through);
        let other_directory = directory.path().join("other");
        std::fs::create_dir(&other_directory).unwrap();
        let elsewhere = Arc::new(ChatFile::new(other_directory.join("CHAT.md"), true));
        elsewhere.ensure().await.unwrap();
        assert_eq!(
            elsewhere.owner_summary().await.unwrap().owner_read_through,
            0
        );

        let cursor_path = path.with_file_name("cursor.json");
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&cursor_path).unwrap()).unwrap();
        legacy.as_object_mut().unwrap().remove("ownerReadThrough");
        std::fs::write(&cursor_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let legacy_chat = Arc::new(ChatFile::new(path, true));
        assert_eq!(
            legacy_chat
                .owner_summary()
                .await
                .unwrap()
                .owner_read_through,
            0
        );
    }

    #[tokio::test]
    async fn owner_pin_and_archive_are_durable_and_leave_agent_state_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("CHAT.md");
        let chat = Arc::new(ChatFile::new(path.clone(), true));
        chat.append_user("pending-user".into(), "Keep working".into())
            .await
            .unwrap();
        chat.record_agent_call(1000).await.unwrap();
        let transcript = std::fs::read(&path).unwrap();
        let unread = chat.unread_snapshot().await.unwrap();
        let before = chat.owner_summary().await.unwrap();
        let pin = chat.set_owner_organization(Some(true), None).await.unwrap();
        assert!(pin.pinned_at_ms.is_some());
        assert!(pin.archived_at_ms.is_none());
        assert_eq!(
            chat.set_owner_organization(Some(true), None)
                .await
                .unwrap()
                .pinned_at_ms,
            pin.pinned_at_ms
        );
        assert_eq!(
            chat.owner_summary().await.unwrap().pinned_at_ms,
            pin.pinned_at_ms,
            "cached summaries include changed organization"
        );
        let archived = chat.set_owner_organization(None, Some(true)).await.unwrap();
        assert!(archived.pinned_at_ms.is_none());
        assert!(archived.archived_at_ms.is_some());
        assert_eq!(
            chat.set_owner_organization(None, Some(true))
                .await
                .unwrap()
                .archived_at_ms,
            archived.archived_at_ms
        );
        assert!(chat.set_owner_organization(Some(true), None).await.is_err());
        assert!(
            chat.set_owner_organization(Some(true), Some(true))
                .await
                .is_err()
        );
        assert!(chat.set_owner_organization(None, None).await.is_err());
        let reopened = Arc::new(ChatFile::new(path.clone(), true));
        let after = reopened.owner_summary().await.unwrap();
        assert_eq!(after.archived_at_ms, archived.archived_at_ms);
        assert!(after.pinned_at_ms.is_none());
        assert_eq!(after.last_agent_call_at_ms, before.last_agent_call_at_ms);
        assert_eq!(after.total_tool_calls, before.total_tool_calls);
        assert_eq!(after.last_entry_end, before.last_entry_end);
        assert_eq!(reopened.unread_snapshot().await.unwrap(), unread);
        assert_eq!(std::fs::read(&path).unwrap(), transcript);
        let restored = reopened
            .set_owner_organization(None, Some(false))
            .await
            .unwrap();
        assert!(restored.pinned_at_ms.is_none());
        assert!(restored.archived_at_ms.is_none());
        reopened
            .set_owner_organization(Some(true), None)
            .await
            .unwrap();
        reopened
            .set_owner_organization(Some(false), None)
            .await
            .unwrap();
        assert!(
            reopened
                .owner_summary()
                .await
                .unwrap()
                .pinned_at_ms
                .is_none()
        );
        assert_eq!(reopened.unread_snapshot().await.unwrap(), unread);
    }

    #[tokio::test]
    async fn goal_update_notices_are_hidden_without_rewriting_or_consuming_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("CHAT.md");
        let chat = Arc::new(ChatFile::new(path.clone(), true));
        chat.append_user("actual-user-message".into(), "Keep working".into())
            .await
            .unwrap();
        let before = chat.owner_summary().await.unwrap();
        let notice = "The goal has changed (revision 2). The new goal is:\n\nUpdated goal.\n\nCall `goal_read` for the current revision and continue toward the updated goal.";
        let notice_id = format!("goal-update-{:064x}", 1);
        let receipt = chat
            .append_user(notice_id.clone(), notice.into())
            .await
            .unwrap();
        let original = std::fs::read(&path).unwrap();

        // Existing notices are hidden on reopen without changing CHAT.md or its cursor.
        let reopened = Arc::new(ChatFile::new(path.clone(), true));
        let page = reopened.widget_page(None, None).await.unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].id, "actual-user-message");
        let summary = reopened.owner_summary().await.unwrap();
        assert_eq!(summary.title, before.title);
        assert_eq!(summary.last_entry_end, before.last_entry_end);
        assert_eq!(summary.last_entry_at_ms, before.last_entry_at_ms);
        assert_eq!(summary.last_user_entry_end, before.last_user_entry_end);
        assert_eq!(summary.last_user_entry_at_ms, before.last_user_entry_at_ms);
        assert_eq!(
            reopened.clean_markdown_transcript().await.unwrap(),
            "## You\n\nKeep working"
        );
        let pending = reopened.unread_snapshot().await.unwrap();
        assert_eq!(pending.messages.len(), 2);
        assert_eq!(pending.messages[1].id, notice_id);
        assert_eq!(pending.messages[1].markdown, notice);
        assert_eq!(pending.messages[1].end, receipt.end);
        assert_eq!(page.read_through, pending.read_through);
        assert!(reopened.read(false).await.unwrap().text.contains(notice));
        assert_eq!(std::fs::read(&path).unwrap(), original);

        // A user can type the exact same text. Similar IDs are also ordinary messages.
        reopened
            .append_user("goal-update-user-text".into(), notice.into())
            .await
            .unwrap();
        let visible = reopened.widget_page(None, None).await.unwrap();
        assert_eq!(visible.messages.len(), 2);
        assert_eq!(visible.messages[1].markdown, notice);
        assert!(
            reopened
                .clean_markdown_transcript()
                .await
                .unwrap()
                .contains(notice)
        );
        reopened.read(true).await.unwrap();
        assert!(
            reopened
                .unread_snapshot()
                .await
                .unwrap()
                .messages
                .is_empty()
        );
    }

    #[tokio::test]
    async fn goal_update_notices_do_not_take_up_history_pages() {
        let directory = tempfile::tempdir().unwrap();
        let chat = Arc::new(ChatFile::new(directory.path().join("CHAT.md"), true));
        chat.append_user(format!("goal-update-{:064x}", 1), "Old goal update".into())
            .await
            .unwrap();
        let only_notice = chat.widget_page(None, None).await.unwrap();
        assert!(only_notice.messages.is_empty());
        assert!(!only_notice.has_more);
        assert_eq!(
            chat.owner_summary().await.unwrap().title,
            "Untitled conversation"
        );
        for number in 0..50 {
            chat.append(format!("Reply {number}")).await.unwrap();
            chat.append_user(
                format!("goal-update-{:064x}", number + 2),
                "Updated goal".into(),
            )
            .await
            .unwrap();
        }
        let page = chat.widget_page(None, None).await.unwrap();
        assert_eq!(page.messages.len(), 50);
        assert!(
            !page.has_more,
            "hidden notices must not create an empty older page"
        );
        chat.append("Reply 50".into()).await.unwrap();
        let latest = chat.widget_page(None, None).await.unwrap();
        assert_eq!(latest.messages.len(), 50);
        assert_eq!(latest.messages.first().unwrap().markdown, "Reply 1");
        assert!(latest.has_more);
        let older = chat.widget_page(latest.before, None).await.unwrap();
        assert_eq!(older.messages.len(), 1);
        assert_eq!(older.messages[0].markdown, "Reply 0");
        assert!(!older.has_more);
    }

    #[tokio::test]
    async fn user_message_read_proof_requires_every_exact_message_to_be_acknowledged() {
        let directory = tempfile::tempdir().unwrap();
        let chat = Arc::new(ChatFile::new(directory.path().join("CHAT.md"), true));
        let first = chat
            .append_user("first".into(), "First instruction".into())
            .await
            .unwrap();
        let ids = vec!["first".into()];
        assert!(!chat.user_messages_read(&ids).await.unwrap());
        chat.widget_page(None, None).await.unwrap();
        chat.read(false).await.unwrap();
        chat.mark_delivered(first.end).await.unwrap();
        assert!(!chat.user_messages_read(&ids).await.unwrap());
        chat.read(true).await.unwrap();
        assert!(chat.user_messages_read(&ids).await.unwrap());
        chat.append_user("second".into(), "Next instruction".into())
            .await
            .unwrap();
        let both = vec!["first".into(), "second".into()];
        assert!(!chat.user_messages_read(&both).await.unwrap());
        chat.read(true).await.unwrap();
        assert!(chat.user_messages_read(&both).await.unwrap());
        assert!(!chat.user_messages_read(&["missing".into()]).await.unwrap());
        assert!(!chat.user_messages_read(&[]).await.unwrap());
    }

    #[tokio::test]
    async fn unread_snapshots_are_structured_stable_and_non_consuming() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat/CHAT.md");
        let chat = Arc::new(ChatFile::new(path.clone(), true));
        chat.ensure().await.unwrap();
        let read_through = chat.widget_page(None, None).await.unwrap().read_through;
        chat.append_user("structured-one".into(), "First **message**".into())
            .await
            .unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"manual follow-up\n")
            .unwrap();

        let first = chat.unread_snapshot().await.unwrap();
        let second = chat.unread_snapshot().await.unwrap();
        assert_eq!(first, second);
        assert_eq!(first.read_through, read_through);
        assert_eq!(first.messages.len(), 2);
        assert_eq!(first.messages[0].id, "structured-one");
        assert_eq!(first.messages[0].markdown, "First **message**");
        assert!(first.messages[1].id.starts_with("manual-"));
        assert_eq!(first.messages[1].markdown, "manual follow-up\n");
        assert_eq!(
            chat.widget_page(None, None).await.unwrap().read_through,
            read_through
        );

        chat.read(true).await.unwrap();
        assert!(chat.unread_snapshot().await.unwrap().messages.is_empty());
    }

    #[tokio::test]
    async fn browser_answers_are_idempotent_and_do_not_acknowledge_or_record_activity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat/CHAT.md");
        let chat = Arc::new(ChatFile::new(path, true));
        chat.ensure().await.unwrap();
        chat.record_agent_call(1000).await.unwrap();
        chat.append_user("pending-user".into(), "Please continue".into())
            .await
            .unwrap();
        let before = chat.widget_page(None, None).await.unwrap();
        let checkpoint = chat.transcript_checkpoint().await.unwrap();

        let appended = chat
            .append_browser_answer("browser-action-one", "Visible browser answer")
            .await
            .unwrap();
        assert!(appended.appended);
        let repeated = chat
            .append_browser_answer("browser-action-one", "Visible browser answer")
            .await
            .unwrap();
        assert!(!repeated.appended);
        assert!(
            chat.append_browser_answer("browser-action-one", "Conflicting answer")
                .await
                .is_err()
        );

        let after = chat.widget_page(None, None).await.unwrap();
        assert_eq!(after.read_through, before.read_through);
        assert_eq!(after.delivered_through, before.delivered_through);
        assert_eq!(after.last_agent_call_at_ms, before.last_agent_call_at_ms);
        assert_eq!(after.total_tool_calls, before.total_tool_calls);
        assert_eq!(after.messages.last().unwrap().role, "agent");
        assert_eq!(
            after.messages.last().unwrap().markdown,
            "Visible browser answer"
        );
        assert_eq!(
            after
                .messages
                .iter()
                .filter(|message| message.id == appended.id)
                .count(),
            1
        );
        assert!(chat.has_agent_message_after(&checkpoint).await.unwrap());
        assert_eq!(
            chat.read(false).await.unwrap().text,
            format!(
                "[{}]\nPlease continue\n\n",
                crate::markdown_chat::attachments::date(before.messages[0].created_at_ms.unwrap())
            )
        );
    }

    #[tokio::test]
    async fn transcript_checkpoints_detect_tool_written_agent_messages() {
        let directory = tempfile::tempdir().unwrap();
        let chat = Arc::new(ChatFile::new(directory.path().join("chat/CHAT.md"), true));
        chat.ensure().await.unwrap();
        let checkpoint = chat.transcript_checkpoint().await.unwrap();
        assert!(!chat.has_agent_message_after(&checkpoint).await.unwrap());
        chat.append("Tool-written answer".into()).await.unwrap();
        assert!(chat.has_agent_message_after(&checkpoint).await.unwrap());
    }
}
