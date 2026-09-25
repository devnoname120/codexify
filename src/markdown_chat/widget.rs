use std::io::{BufRead, BufReader};

use super::*;

const USER_START: &str = "\n\n<!-- codexify-user-message:v1:start id=\"";
const PAGE_MESSAGES: usize = 50;
const PAGE_BYTES: usize = 512 * 1024;

#[derive(Debug, Serialize)]
pub struct WidgetMessage {
    pub id: String,
    pub role: String,
    pub markdown: String,
    pub start: u64,
    pub end: u64,
    pub created_at_ms: Option<u64>,
    pub tool_call_count: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct WidgetPage {
    pub chat_file: String,
    pub revision: String,
    pub delivered_through: u64,
    pub read_through: u64,
    pub last_agent_call_at_ms: Option<u64>,
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
    pub last_entry_end: u64,
    pub last_entry_at_ms: u64,
    pub last_agent_call_at_ms: Option<u64>,
    pub agent_waiting_until_ms: Option<u64>,
    pub total_tool_calls: u64,
}

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

impl ChatFile {
    /// A compact owner-view listing, without advancing the agent's read cursor.
    pub async fn owner_summary(self: &Arc<Self>) -> Result<OwnerChatSummary, String> {
        self.run(|chat| {
            chat.with_cursor(|cursor, file| {
                check_cursor(file, cursor)?;
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
                        title: cached.title.clone(),
                        last_entry_end: cached.last_entry_end,
                        last_entry_at_ms: cached.last_entry_at_ms,
                        last_agent_call_at_ms: cursor.last_agent_call_at_ms,
                        agent_waiting_until_ms: chat.agent_waiting_until_ms(),
                        total_tool_calls: cursor.total_tool_calls,
                    });
                }
                let records = spans(file)?;
                let title = records
                    .iter()
                    .find(|span| span.role == "user")
                    .or_else(|| records.first())
                    .and_then(|span| {
                        read_range(
                            file,
                            span.body_start,
                            span.body_end.min(span.body_start + 512),
                            512,
                        )
                        .ok()
                    })
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|text| !text.is_empty())
                    .map(|text| text.chars().take(72).collect())
                    .unwrap_or_else(|| "Untitled conversation".into());
                let last = records.last();
                let modified_ms = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let summary = OwnerChatSummary {
                    title,
                    last_entry_end: last.map_or(0, |span| span.end),
                    last_entry_at_ms: last
                        .and_then(|span| span.created_at_ms)
                        .unwrap_or(modified_ms),
                    last_agent_call_at_ms: cursor.last_agent_call_at_ms,
                    agent_waiting_until_ms: chat.agent_waiting_until_ms(),
                    total_tool_calls: cursor.total_tool_calls,
                };
                *cache = Some(OwnerSummaryCache {
                    length: metadata.len(),
                    modified,
                    title: summary.title.clone(),
                    last_entry_end: summary.last_entry_end,
                    last_entry_at_ms: summary.last_entry_at_ms,
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
                let revision = format!(
                    "{length}-{modified}-{}-{}",
                    cursor.delivered_through, cursor.offset
                );
                let unchanged = before.is_none() && known_revision.as_deref() == Some(&revision);
                let mut page = WidgetPage {
                    chat_file: chat.path.display().to_string(),
                    revision,
                    delivered_through: cursor.delivered_through,
                    read_through: cursor.offset,
                    last_agent_call_at_ms: cursor.last_agent_call_at_ms,
                    agent_waiting_until_ms: chat.agent_waiting_until_ms(),
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
                    .filter(|span| before.is_none_or(|end| span.start < end))
                {
                    if page.messages.len() >= PAGE_MESSAGES
                        || (bytes >= PAGE_BYTES && !page.messages.is_empty())
                    {
                        page.has_more = true;
                        break;
                    }
                    let text = read_range(file, span.body_start, span.body_end, MAX_UNREAD_BYTES)?;
                    let markdown =
                        String::from_utf8(text).map_err(|_| "CHAT.md contains incomplete UTF-8")?;
                    if markdown.trim().is_empty() {
                        continue;
                    }
                    let (role, markdown) = if span.role == "agent"
                        && markdown
                            .starts_with("**Possible duplicate agent detected and blocked.**")
                    {
                        ("warning", crate::agent_tickets::WARNING.to_string())
                    } else {
                        (span.role, markdown)
                    };
                    bytes += markdown.len();
                    page.before = Some(span.start);
                    page.messages.push(WidgetMessage {
                        id: span.id,
                        role: role.into(),
                        markdown,
                        start: span.start,
                        end: span.end,
                        created_at_ms: span.created_at_ms,
                        tool_call_count: span.tool_call_count,
                    });
                }
                page.messages.reverse();
                Ok(page)
            })
        })
        .await
    }

    pub async fn append_user(
        self: &Arc<Self>,
        id: String,
        message: String,
    ) -> Result<UserSendReceipt, String> {
        if !valid_id(&id) {
            return Err("Invalid chat message request ID.".into());
        }
        if message.trim().is_empty() {
            return Err("User message must not be blank.".into());
        }
        if message.len() > MAX_UNREAD_BYTES {
            return Err(
                "User message exceeds the 16 MiB safety ceiling; nothing was appended.".into(),
            );
        }
        self.run(move |chat| chat.with_cursor(|cursor, file| {
            check_cursor(file, cursor)?;
            for span in spans(file)? {
                if span.role == "user" && span.id == id {
                    if read_range(file, span.body_start, span.body_end, MAX_UNREAD_BYTES)? != message.as_bytes() {
                        return Err("This request ID already belongs to a different message.".into());
                    }
                    if span.end > cursor.offset { chat.clear_agent_waiting(); }
                    return Ok(UserSendReceipt { id, end: span.end, created_at_ms: span.created_at_ms, tool_call_count: span.tool_call_count });
                }
            }
            let created_at_ms = super::super::now_ms();
            let tool_call_count = cursor.total_tool_calls;
            let block = format!("{USER_START}{id}\" created_at_ms=\"{created_at_ms}\" tool_call_count=\"{tool_call_count}\" -->\n\n## User\n\n{message}\n\n<!-- codexify-user-message:v1:end id=\"{id}\" -->\n");
            file.write_all(block.as_bytes()).map_err(io_error)?;
            let end = file.stream_position().map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
            let same = same_file::Handle::from_file(file.try_clone().map_err(io_error)?)
                .and_then(|handle| same_file::Handle::from_path(&chat.path).map(|current| current == handle))
                .map_err(io_error)?;
            if !same { return Err("CHAT.md changed during send; reload and retry with the same request ID.".into()); }
            chat.clear_agent_waiting();
            Ok(UserSendReceipt { id, end, created_at_ms: Some(created_at_ms), tool_call_count: Some(tool_call_count) })
        })).await
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

fn persist_agent_activity(
    chat: &ChatFile,
    cursor: &mut Cursor,
    activity: super::super::AgentActivity,
) -> Result<(), String> {
    let mut next = cursor.clone();
    next.last_agent_call_at_ms = Some(
        next.last_agent_call_at_ms
            .unwrap_or_default()
            .max(activity.at_ms),
    );
    let sequence_delta = if next.tool_call_epoch.as_deref() == Some(activity.epoch.as_str()) {
        activity.sequence.saturating_sub(next.tool_call_sequence)
    } else {
        activity.sequence
    };
    next.total_tool_calls = next.total_tool_calls.saturating_add(sequence_delta);
    if next.tool_call_epoch.as_deref() != Some(activity.epoch.as_str())
        || activity.sequence > next.tool_call_sequence
    {
        next.tool_call_epoch = Some(activity.epoch);
        next.tool_call_sequence = activity.sequence;
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

pub(super) fn user_text(text: &str) -> Result<String, String> {
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
        let Some((id, _, _)) = marker_fields(role, fields) else {
            return Err("CHAT.md has a malformed message marker.".into());
        };
        let ending = format!("\n\n<!-- codexify-{role}-message:v1:end id=\"{id}\" -->\n");
        let Some((message, tail)) = body.split_once(&ending) else {
            return Err("CHAT.md has an incomplete message; finish saving before retrying.".into());
        };
        if role == "user" {
            output.push_str(message);
            output.push_str("\n\n");
        }
        remaining = tail;
    }
    output.push_str(remaining);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
