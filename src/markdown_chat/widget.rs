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
}

#[derive(Debug, Serialize)]
pub struct WidgetPage {
    pub chat_file: String,
    pub revision: String,
    pub delivered_through: u64,
    pub messages: Vec<WidgetMessage>,
    pub has_more: bool,
    pub before: Option<u64>,
    pub unchanged: bool,
}

#[derive(Debug, Serialize)]
pub struct UserSendReceipt {
    pub id: String,
    pub end: u64,
}

struct Span {
    id: String,
    role: &'static str,
    start: u64,
    body_start: u64,
    body_end: u64,
    end: u64,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn start_marker(line: &str) -> Option<(&'static str, String)> {
    for role in ["agent", "user"] {
        if let Some(id) = line
            .strip_prefix(&format!("<!-- codexify-{role}-message:v1:start id=\""))
            .and_then(|line| line.strip_suffix("\" -->\n"))
            .filter(|id| valid_id(id))
        {
            return Some((role, id.to_string()));
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
        if let Some((role, id)) = start_marker(&line) {
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
                let revision = format!("{length}-{modified}-{}", cursor.delivered_through);
                let unchanged = before.is_none() && known_revision.as_deref() == Some(&revision);
                let mut page = WidgetPage {
                    chat_file: chat.path.display().to_string(),
                    revision,
                    delivered_through: cursor.delivered_through,
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
                    bytes += markdown.len();
                    page.before = Some(span.start);
                    page.messages.push(WidgetMessage {
                        id: span.id,
                        role: span.role.into(),
                        markdown,
                        start: span.start,
                        end: span.end,
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
                    return Ok(UserSendReceipt { id, end: span.end });
                }
            }
            let block = format!("{USER_START}{id}\" -->\n\n## User\n\n{message}\n\n<!-- codexify-user-message:v1:end id=\"{id}\" -->\n");
            file.write_all(block.as_bytes()).map_err(io_error)?;
            let end = file.stream_position().map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
            let same = same_file::Handle::from_file(file.try_clone().map_err(io_error)?)
                .and_then(|handle| same_file::Handle::from_path(&chat.path).map(|current| current == handle))
                .map_err(io_error)?;
            if !same { return Err("CHAT.md changed during send; reload and retry with the same request ID.".into()); }
            Ok(UserSendReceipt { id, end })
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

pub(super) fn user_text(text: &str) -> Result<String, String> {
    let mut output = String::new();
    let mut remaining = text;
    loop {
        let candidate = [("agent", AGENT_START), ("user", USER_START)]
            .into_iter()
            .filter_map(|(role, prefix)| remaining.find(prefix).map(|start| (start, role, prefix)))
            .min_by_key(|entry| entry.0);
        let Some((start, role, prefix)) = candidate else {
            break;
        };
        output.push_str(&remaining[..start]);
        let marker = &remaining[start + prefix.len()..];
        let heading = if role == "user" { "User" } else { "Agent" };
        let opening = format!("\" -->\n\n## {heading}\n\n");
        let Some((id, body)) = marker.split_once(&opening) else {
            return Err("CHAT.md has an incomplete message; finish saving before retrying.".into());
        };
        if !valid_id(id) {
            return Err("CHAT.md has a malformed message marker.".into());
        }
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
