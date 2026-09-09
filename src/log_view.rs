use std::io::{self, Write};

use serde_json::Value;

use crate::terminal::{
    ACCENT, EMPHASIS, FAILURE, HEADING, MUTED, SUCCESS, WARNING, paint, pretty_json, strip_ansi,
};

type PayloadField = (String, String, Option<Value>);

#[derive(Default)]
pub struct LogPresenter {
    pending: Vec<u8>,
}

impl LogPresenter {
    pub fn write(&mut self, bytes: &[u8], output: &mut dyn Write) -> io::Result<()> {
        self.pending.extend_from_slice(bytes);
        while let Some(index) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line = self.pending.drain(..=index).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]);
            output.write_all(format_line(&line).as_bytes())?;
            output.write_all(b"\n")?;
        }
        Ok(())
    }

    pub fn finish(&mut self, output: &mut dyn Write) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let line = String::from_utf8_lossy(&self.pending);
        output.write_all(format_line(&line).as_bytes())?;
        self.pending.clear();
        Ok(())
    }
}

pub fn format_line(line: &str) -> String {
    let line = strip_ansi(line).trim_end_matches('\r').to_string();
    if line.trim().is_empty() {
        return String::new();
    }
    if let Some(formatted) = format_tool_event(&line) {
        return formatted;
    }
    if let Some(formatted) = format_service_event(&line) {
        return formatted;
    }
    format_general_event(&line).unwrap_or(line)
}

fn format_tool_event(line: &str) -> Option<String> {
    let (timestamp, level, target, body) = tracing_header(line)?;
    if target != "codexify::tool_payload" {
        return None;
    }

    let (without_payload, payload) = split_payload(body);
    let (message, fields) = split_message_fields(without_payload);
    let field = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let bracketed_tool = message
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("] "))
        .map(|(tool, message)| (tool.to_string(), message.to_string()));
    let (tool, message) = bracketed_tool.unwrap_or_else(|| {
        (
            field("tool").map(unquote).unwrap_or_else(|| "tool".into()),
            message.to_string(),
        )
    });

    let mut output = format!(
        "{} {} {}: {} {}",
        paint(MUTED, timestamp),
        paint(level_style(level), format!("{level:>5}")),
        paint(MUTED, target),
        paint(HEADING, format!("[{tool}]")),
        paint(EMPHASIS, message.trim())
    );

    let mut facts = Vec::new();
    if let Some(call_id) = field("call_id") {
        facts.push(paint(MUTED, format!("call #{call_id}")));
    }
    if let Some(status) = field("status").map(unquote) {
        let style = if status == "ok" || status == "started" {
            SUCCESS
        } else {
            FAILURE
        };
        facts.push(paint(style, status));
    }
    if let Some(duration) = field("duration_ms") {
        facts.push(paint(ACCENT, format!("{duration} ms")));
    }
    if let Some(resolved) = field("resolved_tool").map(unquote)
        && resolved != tool
    {
        facts.push(paint(MUTED, format!("target {resolved}")));
    }
    if let Some((kind, _, _)) = payload.as_ref() {
        let bytes = field(&format!("{kind}_bytes"))
            .and_then(|value| value.parse::<usize>().ok())
            .map(format_bytes);
        let truncated = field(&format!("{kind}_truncated")) == Some("true");
        let failed = field(&format!("{kind}_serialization_failed")) == Some("true");
        let mut value = kind.to_string();
        if let Some(bytes) = bytes {
            value.push_str(&format!(" {bytes}"));
        }
        if truncated {
            value.push_str(" truncated");
        }
        if failed {
            value.push_str(" serialization failed");
        }
        facts.push(paint(if failed { FAILURE } else { MUTED }, value));
    }
    if !facts.is_empty() {
        output.push_str("  ");
        output.push_str(&facts.join(&paint(MUTED, "  ·  ")));
    }

    if let Some((kind, raw, value)) = payload {
        output.push('\n');
        output.push_str(&paint(ACCENT, format!("  {kind}:")));
        output.push('\n');
        let rendered = value
            .as_ref()
            .map(pretty_json)
            .unwrap_or_else(|| paint(WARNING, raw));
        for line in rendered.lines() {
            output.push_str("    ");
            output.push_str(line);
            output.push('\n');
        }
        output.pop();
    }
    Some(output)
}

fn tracing_header(line: &str) -> Option<(&str, &str, &str, &str)> {
    let line = line.trim_start();
    let timestamp_end = line.find(char::is_whitespace)?;
    let timestamp = &line[..timestamp_end];
    let rest = line[timestamp_end..].trim_start();
    let level_end = rest.find(char::is_whitespace)?;
    let level = &rest[..level_end];
    let rest = rest[level_end..].trim_start();
    let (target, body) = rest.split_once(": ")?;
    Some((timestamp, level, target, body))
}

fn split_payload(body: &str) -> (&str, Option<PayloadField>) {
    for kind in ["response", "request"] {
        let marker = format!(" {kind}=");
        if let Some(index) = body.rfind(&marker) {
            let raw = body[index + marker.len()..].trim().to_string();
            let value = serde_json::from_str(&raw).ok();
            return (&body[..index], Some((kind.to_string(), raw, value)));
        }
    }
    (body, None)
}

fn split_message_fields(body: &str) -> (&str, Vec<(String, String)>) {
    let Some(index) = find_field_start(body) else {
        return (body, Vec::new());
    };
    let message = body[..index].trim_end();
    let mut fields = Vec::new();
    let bytes = &body.as_bytes()[index..];
    let mut cursor = 0;
    while cursor < bytes.len() {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let key_start = cursor;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
        {
            cursor += 1;
        }
        if cursor == key_start || bytes.get(cursor) != Some(&b'=') {
            break;
        }
        let key = String::from_utf8_lossy(&bytes[key_start..cursor]).into_owned();
        cursor += 1;
        let value_start = cursor;
        if bytes.get(cursor) == Some(&b'"') {
            cursor += 1;
            let mut escaped = false;
            while cursor < bytes.len() {
                let byte = bytes[cursor];
                cursor += 1;
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    break;
                }
            }
        } else {
            while cursor < bytes.len() && !bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
        }
        fields.push((
            key,
            String::from_utf8_lossy(&bytes[value_start..cursor]).into_owned(),
        ));
    }
    (message, fields)
}

fn find_field_start(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    for index in 0..bytes.len() {
        if index > 0 && !bytes[index - 1].is_ascii_whitespace() {
            continue;
        }
        let mut cursor = index;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
        {
            cursor += 1;
        }
        if cursor > index && bytes.get(cursor) == Some(&b'=') {
            return Some(index);
        }
    }
    None
}

fn unquote(value: &str) -> String {
    serde_json::from_str::<String>(value).unwrap_or_else(|_| value.to_string())
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1_000 {
        format!("{bytes} B")
    } else if bytes < 1_000_000 {
        format!("{:.1} kB", bytes as f64 / 1_000.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    }
}

fn level_style(level: &str) -> anstyle::Style {
    match level {
        "TRACE" | "DEBUG" => MUTED,
        "INFO" => SUCCESS,
        "WARN" => WARNING,
        "ERROR" => FAILURE,
        _ => ACCENT,
    }
}

fn format_service_event(line: &str) -> Option<String> {
    let rest = line.strip_prefix('[')?;
    let (timestamp, rest) = rest.split_once("] [service] ")?;
    let style = if rest.contains("failed") || rest.contains("exited with exit status") {
        FAILURE
    } else if rest.contains("waiting") || rest.contains("restarting") {
        WARNING
    } else {
        SUCCESS
    };
    Some(format!(
        "{} {} {}",
        paint(MUTED, timestamp),
        paint(HEADING, "[service]"),
        paint(style, rest)
    ))
}

fn format_general_event(line: &str) -> Option<String> {
    let (timestamp, level, target, body) = tracing_header(line)?;
    Some(format!(
        "{} {} {}: {}",
        paint(MUTED, timestamp),
        paint(level_style(level), format!("{level:>5}")),
        paint(MUTED, target),
        body
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_tool_line_moves_the_tool_to_the_event_prefix_and_prettifies_json() {
        let line = "2026-09-09T13:57:54Z INFO codexify::tool_payload: tool invocation completed call_id=7 phase=\"finish\" tool=apply_patch resolved_tool=apply_patch status=\"ok\" duration_ms=12 response_bytes=24 response_truncated=false response_serialization_failed=false response={\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}";
        let plain = strip_ansi(&format_line(line));
        assert!(plain.starts_with("2026-09-09T13:57:54Z  INFO codexify::tool_payload: [apply_patch] tool invocation completed"), "{plain}");
        assert!(plain.contains("call #7"), "{plain}");
        assert!(plain.contains("12 ms"), "{plain}");
        assert!(
            plain.contains("\n  response:\n    {\n      \"content\": ["),
            "{plain}"
        );
        assert!(!plain.contains(" tool=apply_patch"), "{plain}");
    }

    #[test]
    fn presenter_reassembles_split_lines_and_removes_persisted_ansi() {
        let mut presenter = LogPresenter::default();
        let mut output = Vec::new();
        presenter
            .write(b"\x1b[32mfirst\x1b[0m\nsec", &mut output)
            .unwrap();
        presenter.write(b"ond\nthird", &mut output).unwrap();
        presenter.finish(&mut output).unwrap();
        assert_eq!(
            strip_ansi(&String::from_utf8(output).unwrap()),
            "first\nsecond\nthird"
        );
    }
}
