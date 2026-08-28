//! Ceilings on model-visible tool output and semantic file/list windows.
//!
//! Tool-specific paging remains preferable because it gives the model a precise
//! continuation. The connector-wide policy is the fallback that prevents any
//! built-in or bridged result from bypassing the context bound.

use std::collections::VecDeque;
use std::io::{self, Write};

use serde_json::Value;

use crate::types::{AppConfig, ToolContent, ToolResult};

pub const DEFAULT_MAX_TOOL_OUTPUT_TOKENS: u64 = 10_000;
pub const DEFAULT_MAX_FILE_LINES: usize = 1_000;
pub const DEFAULT_MAX_FILE_BYTES: usize = 131_072;
pub const DEFAULT_MAX_ENTRIES: usize = 500;
pub const DEFAULT_MAX_TREE_NODES: usize = 1_000;
const APPROX_BYTES_PER_TOKEN: u64 = 4;

pub fn tool_output_token_budget(config: &AppConfig) -> u64 {
    config
        .output
        .max_tool_output_tokens
        .unwrap_or(DEFAULT_MAX_TOOL_OUTPUT_TOKENS)
        .max(1)
}

pub fn resolve_requested_output_tokens(config: &AppConfig, requested: Option<u64>) -> u64 {
    requested
        .filter(|tokens| *tokens > 0)
        .unwrap_or(DEFAULT_MAX_TOOL_OUTPUT_TOKENS)
        .min(tool_output_token_budget(config))
}

pub fn approx_token_count(text: &str) -> u64 {
    (text.encode_utf16().count() as u64).div_ceil(APPROX_BYTES_PER_TOKEN)
}

pub fn approx_bytes_for_tokens(tokens: u64) -> usize {
    usize::try_from(tokens.saturating_mul(APPROX_BYTES_PER_TOKEN)).unwrap_or(usize::MAX)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextTruncation {
    pub text: String,
    pub original_token_count: u64,
    pub truncated: bool,
}

pub fn truncate_text(text: &str, max_output_tokens: u64) -> TextTruncation {
    truncate_text_segments(&[text], max_output_tokens)
}

fn truncate_text_segments(texts: &[&str], max_output_tokens: u64) -> TextTruncation {
    let original_units = texts
        .iter()
        .map(|text| text.encode_utf16().count())
        .fold(texts.len().saturating_sub(1), usize::saturating_add);
    let original_token_count = (original_units as u64).div_ceil(APPROX_BYTES_PER_TOKEN);
    let budget_units = usize::try_from(
        max_output_tokens
            .max(1)
            .saturating_mul(APPROX_BYTES_PER_TOKEN),
    )
    .unwrap_or(usize::MAX);

    if original_units <= budget_units {
        return TextTruncation {
            text: texts.join("\n"),
            original_token_count,
            truncated: false,
        };
    }

    let detailed_marker = format!(
        "\n\n[... output truncated; original approximate token count: {original_token_count} ...]\n\n"
    );
    let marker = if detailed_marker.encode_utf16().count() + 2 <= budget_units {
        detailed_marker
    } else if 7 <= budget_units {
        "\n...\n".to_string()
    } else {
        "...".to_string()
    };
    let marker_units = marker.encode_utf16().count();

    let retained_units = budget_units - marker_units;
    let head_units = retained_units / 2;
    let tail_units = retained_units - head_units;
    let head = text_segments_prefix(texts, head_units);
    let tail = text_segments_suffix(texts, tail_units);

    TextTruncation {
        text: format!("{head}{marker}{tail}"),
        original_token_count,
        truncated: true,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolResultBudgetOutcome {
    pub text_truncated: bool,
    pub structured_content_truncated: bool,
}

pub fn enforce_tool_result_budget(
    result: &mut ToolResult,
    max_output_tokens: u64,
) -> ToolResultBudgetOutcome {
    let max_output_tokens = max_output_tokens.max(1);
    let original_text_tokens = text_content_token_count(&result.content);

    let original_structured_tokens = result
        .structured_content
        .as_ref()
        .map(serialized_token_count)
        .unwrap_or(0);
    let structured_content_truncated = original_structured_tokens > max_output_tokens;
    if structured_content_truncated {
        result.structured_content = None;
        result.is_error = true;
        result.content.insert(
            0,
            ToolContent::Text(
                "Oversized structuredContent omitted. Retry with narrower arguments.".to_string(),
            ),
        );
    }

    let text_truncated = truncate_text_content_blocks(&mut result.content, max_output_tokens);
    if text_truncated || structured_content_truncated {
        result.audit.truncated = Some(true);
        let original_tokens = original_text_tokens.saturating_add(original_structured_tokens);
        result.audit.original_output_tokens = Some(
            result
                .audit
                .original_output_tokens
                .unwrap_or(0)
                .max(original_tokens),
        );
    }

    ToolResultBudgetOutcome {
        text_truncated,
        structured_content_truncated,
    }
}

pub fn fill_text_mirror_with_budget(result: &mut ToolResult, max_output_tokens: u64) -> bool {
    let max_output_tokens = max_output_tokens.max(1);
    let original_content = result.content.clone();
    let original_text_tokens = text_content_token_count(&original_content);
    let mut candidate_budget = max_output_tokens;

    for _ in 0..16 {
        let mut candidate_content = original_content.clone();
        truncate_text_content_blocks(&mut candidate_content, candidate_budget);
        let text = joined_text(&candidate_content);
        let structured = serde_json::json!({ "content": text });
        let structured_tokens = serialized_token_count(&structured);
        if structured_tokens <= max_output_tokens {
            let truncated = candidate_budget < max_output_tokens;
            result.content = candidate_content;
            result.structured_content = Some(structured);
            if truncated {
                result.audit.truncated = Some(true);
                result.audit.original_output_tokens = Some(
                    result
                        .audit
                        .original_output_tokens
                        .unwrap_or(0)
                        .max(original_text_tokens),
                );
            }
            return truncated;
        }

        if candidate_budget == 0 {
            break;
        }
        let scaled = candidate_budget
            .saturating_mul(max_output_tokens)
            .checked_div(structured_tokens.max(1))
            .unwrap_or(0);
        candidate_budget = scaled.min(candidate_budget - 1);
    }

    result.is_error = true;
    result.structured_content = None;
    result.content = vec![ToolContent::Text(
        truncate_text(
            "The configured model-output token limit is too small to serialize this tool's required structured response.",
            max_output_tokens,
        )
        .text,
    )];
    result.audit.truncated = Some(true);
    result.audit.original_output_tokens = Some(
        result
            .audit
            .original_output_tokens
            .unwrap_or(0)
            .max(original_text_tokens),
    );
    true
}

fn truncate_text_content_blocks(content: &mut Vec<ToolContent>, max_output_tokens: u64) -> bool {
    if max_output_tokens == 0 {
        let had_text = content
            .iter()
            .any(|item| matches!(item, ToolContent::Text(_)));
        content.retain(|item| !matches!(item, ToolContent::Text(_)));
        return had_text;
    }

    let text_blocks = content
        .iter()
        .filter_map(|item| match item {
            ToolContent::Text(text) => Some(text.as_str()),
            ToolContent::Image { .. } | ToolContent::ResourceLink(_) => None,
        })
        .collect::<Vec<_>>();
    if text_blocks.is_empty() {
        return false;
    }

    let total_tokens = text_blocks
        .iter()
        .map(|text| approx_token_count(text))
        .fold(
            text_blocks.len().saturating_sub(1).div_ceil(4) as u64,
            u64::saturating_add,
        );
    if total_tokens <= max_output_tokens {
        return false;
    }

    let truncated = truncate_text_segments(&text_blocks, max_output_tokens).text;
    let mut inserted_text = false;
    let mut bounded = Vec::with_capacity(content.len());
    for item in std::mem::take(content) {
        match item {
            ToolContent::Text(_) if !inserted_text => {
                bounded.push(ToolContent::Text(truncated.clone()));
                inserted_text = true;
            }
            ToolContent::Text(_) => {}
            other => bounded.push(other),
        }
    }
    *content = bounded;
    true
}

fn text_content_token_count(content: &[ToolContent]) -> u64 {
    let mut text_blocks = 0usize;
    let text_tokens = content
        .iter()
        .filter_map(|item| match item {
            ToolContent::Text(text) => {
                text_blocks += 1;
                Some(approx_token_count(text))
            }
            ToolContent::Image { .. } | ToolContent::ResourceLink(_) => None,
        })
        .fold(0u64, u64::saturating_add);
    text_tokens.saturating_add(text_blocks.saturating_sub(1).div_ceil(4) as u64)
}

fn joined_text(content: &[ToolContent]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            ToolContent::Text(text) => Some(text.as_str()),
            ToolContent::Image { .. } | ToolContent::ResourceLink(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn serialized_token_count(value: &Value) -> u64 {
    let mut counter = ByteCounter::default();
    if serde_json::to_writer(&mut counter, value).is_err() {
        return u64::MAX;
    }
    counter.len.div_ceil(APPROX_BYTES_PER_TOKEN)
}

#[derive(Default)]
struct ByteCounter {
    len: u64,
}

impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.len = self.len.saturating_add(bytes.len() as u64);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn utf16_prefix_end(text: &str, max_units: usize) -> usize {
    let mut used = 0usize;
    let mut end = 0usize;
    for (index, character) in text.char_indices() {
        let units = character.len_utf16();
        if used.saturating_add(units) > max_units {
            break;
        }
        used += units;
        end = index + character.len_utf8();
    }
    end
}

fn text_segments_prefix(texts: &[&str], max_units: usize) -> String {
    let mut remaining = max_units;
    let mut output = String::new();
    for (index, text) in texts.iter().enumerate() {
        if index > 0 {
            if remaining == 0 {
                break;
            }
            output.push('\n');
            remaining -= 1;
        }

        let end = utf16_prefix_end(text, remaining);
        output.push_str(&text[..end]);
        remaining = remaining.saturating_sub(text[..end].encode_utf16().count());
        if end < text.len() {
            break;
        }
    }
    output
}

fn text_segments_suffix(texts: &[&str], max_units: usize) -> String {
    let mut remaining = max_units;
    let mut parts = Vec::new();
    for index in (0..texts.len()).rev() {
        let text = texts[index];
        let start = utf16_suffix_start(text, remaining);
        parts.push(text[start..].to_string());
        remaining = remaining.saturating_sub(text[start..].encode_utf16().count());
        if start > 0 || index == 0 || remaining == 0 {
            break;
        }
        parts.push("\n".to_string());
        remaining -= 1;
    }
    parts.reverse();
    parts.concat()
}

fn utf16_suffix_start(text: &str, max_units: usize) -> usize {
    let mut used = 0usize;
    let mut start = text.len();
    for (index, character) in text.char_indices().rev() {
        let units = character.len_utf16();
        if used.saturating_add(units) > max_units {
            break;
        }
        used += units;
        start = index;
    }
    start
}

#[derive(Debug, Default)]
pub(crate) struct BoundedTextBuffer<const MAX_BYTES: usize> {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundedTextOutput {
    pub text: String,
    pub truncated: bool,
    pub omitted_bytes: u64,
}

impl<const MAX_BYTES: usize> BoundedTextBuffer<MAX_BYTES> {
    const HEAD_BYTES: usize = MAX_BYTES / 2;
    const TAIL_BYTES: usize = MAX_BYTES - Self::HEAD_BYTES;

    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push_bytes(&mut self, chunk: &[u8]) {
        let mut chunk = chunk;
        if self.head.len() < Self::HEAD_BYTES {
            let space = Self::HEAD_BYTES - self.head.len();
            if chunk.len() <= space {
                self.head.extend_from_slice(chunk);
                return;
            }
            self.head.extend_from_slice(&chunk[..space]);
            chunk = &chunk[space..];
        }

        let remaining_tail = Self::TAIL_BYTES.saturating_sub(self.tail.len());
        let excess = chunk.len().saturating_sub(remaining_tail);
        self.omitted_bytes = self.omitted_bytes.saturating_add(excess as u64);
        if excess <= self.tail.len() {
            self.tail.drain(..excess);
            self.tail.extend(chunk);
        } else {
            let skip = excess - self.tail.len();
            self.tail.clear();
            self.tail.extend(&chunk[skip..]);
        }
    }

    #[cfg(test)]
    pub(crate) fn push_str(&mut self, chunk: &str) {
        self.push_bytes(chunk.as_bytes());
    }

    pub(crate) fn take(&mut self, reason: &str) -> BoundedTextOutput {
        let head = std::mem::take(&mut self.head);
        let mut tail = std::mem::take(&mut self.tail);
        let omitted_bytes = std::mem::replace(&mut self.omitted_bytes, 0);
        if omitted_bytes == 0 {
            let mut bytes = head;
            bytes.extend(tail);
            BoundedTextOutput {
                text: String::from_utf8_lossy(&bytes).into_owned(),
                truncated: false,
                omitted_bytes,
            }
        } else {
            let head = String::from_utf8_lossy(&head);
            let tail = String::from_utf8_lossy(tail.make_contiguous());
            BoundedTextOutput {
                text: format!(
                    "{head}\n\n[... {omitted_bytes} bytes elided ({reason}) ...]\n\n{tail}"
                ),
                truncated: true,
                omitted_bytes,
            }
        }
    }

    #[cfg(test)]
    fn retained_bytes(&self) -> usize {
        self.head.len().saturating_add(self.tail.len())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FileBudget {
    pub max_lines: usize,
    pub max_bytes: usize,
}

pub fn file_budget(config: &AppConfig) -> FileBudget {
    FileBudget {
        max_lines: config
            .output
            .max_file_lines
            .unwrap_or(DEFAULT_MAX_FILE_LINES),
        max_bytes: config
            .output
            .max_file_bytes
            .unwrap_or(DEFAULT_MAX_FILE_BYTES),
    }
}

pub fn entry_budget(config: &AppConfig) -> usize {
    config.output.max_entries.unwrap_or(DEFAULT_MAX_ENTRIES)
}

pub fn tree_node_budget(config: &AppConfig) -> usize {
    config
        .output
        .max_tree_nodes
        .unwrap_or(DEFAULT_MAX_TREE_NODES)
}

#[derive(Debug, Clone)]
pub struct FileWindow {
    /// The lines actually returned.
    pub lines: Vec<String>,
    /// 0-based index of the first returned line.
    pub start: usize,
    /// Total lines in the file, before any windowing.
    pub total: usize,
    /// Human-readable note about what was cut, or `None` when nothing was.
    pub notice: Option<String>,
}

/// Take the first `max_bytes` bytes of `s`, decoding lossily so a multibyte
/// character straddling the cut becomes U+FFFD — matching Node's
/// `Buffer.subarray(0, n).toString("utf8")`.
fn byte_prefix(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    String::from_utf8_lossy(&s.as_bytes()[..max_bytes]).into_owned()
}

/// Slice a file to a window that fits both budgets.
///
/// The byte cap is not redundant with the line cap: a minified bundle is often
/// one line several megabytes long, which a line cap alone would return in full.
pub fn window_file_lines(
    lines: &[&str],
    offset: usize,
    requested_limit: Option<usize>,
    budget: FileBudget,
) -> FileWindow {
    let total = lines.len();
    let start = offset.min(total);
    let limit = requested_limit
        .unwrap_or(budget.max_lines)
        .min(budget.max_lines);

    let mut kept: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    let mut cut_on_bytes = false;

    let slice_end = start.saturating_add(limit).min(total);
    for line in &lines[start..slice_end] {
        let size = line.len() + 1;
        if bytes + size > budget.max_bytes {
            if kept.is_empty() {
                // A single line larger than the whole budget: return a prefix so
                // the caller sees something rather than an empty window.
                kept.push(byte_prefix(line, budget.max_bytes));
            }
            cut_on_bytes = true;
            break;
        }
        kept.push((*line).to_string());
        bytes += size;
    }

    let end = start + kept.len();
    if start == 0 && end == total && !cut_on_bytes {
        return FileWindow {
            lines: kept,
            start,
            total,
            notice: None,
        };
    }

    let reason = if cut_on_bytes {
        ", cut at the byte budget"
    } else {
        ""
    };
    let more = if end < total {
        format!(" — call again with offset={end} for the rest")
    } else {
        String::new()
    };
    let notice = format!(
        "(showing lines {}-{} of {}{}{})",
        start + 1,
        end,
        total,
        reason,
        more
    );
    FileWindow {
        lines: kept,
        start,
        total,
        notice: Some(notice),
    }
}

/// Cut a list to `max`, reporting how many entries were dropped.
pub fn limit_list<T>(mut items: Vec<T>, max: usize) -> (Vec<T>, usize) {
    if max == 0 || items.len() <= max {
        return (items, 0);
    }
    let dropped = items.len() - max;
    items.truncate(max);
    (items, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(max_lines: usize, max_bytes: usize) -> FileBudget {
        FileBudget {
            max_lines,
            max_bytes,
        }
    }

    #[test]
    fn whole_small_file_has_no_notice() {
        let lines = vec!["a", "b", "c"];
        let w = window_file_lines(&lines, 0, None, budget(1000, 1000));
        assert_eq!(w.lines, vec!["a", "b", "c"]);
        assert!(w.notice.is_none());
    }

    #[test]
    fn line_cap_windows_and_notices() {
        let lines = vec!["a", "b", "c", "d"];
        let w = window_file_lines(&lines, 0, None, budget(2, 1000));
        assert_eq!(w.lines, vec!["a", "b"]);
        let n = w.notice.unwrap();
        assert!(n.contains("offset=2"), "{n}");
    }

    #[test]
    fn offset_windows() {
        let lines = vec!["a", "b", "c", "d"];
        let w = window_file_lines(&lines, 2, None, budget(1000, 1000));
        assert_eq!(w.lines, vec!["c", "d"]);
    }

    #[test]
    fn byte_cap_on_single_huge_line() {
        let huge = "x".repeat(100);
        let lines = vec![huge.as_str()];
        let w = window_file_lines(&lines, 0, None, budget(1000, 10));
        assert_eq!(w.lines[0].len(), 10);
        assert!(w.notice.unwrap().contains("byte budget"));
    }

    #[test]
    fn limit_list_reports_dropped() {
        let (items, dropped) = limit_list(vec![1, 2, 3, 4, 5], 3);
        assert_eq!(items, vec![1, 2, 3]);
        assert_eq!(dropped, 2);
    }

    #[test]
    fn text_truncation_includes_marker_inside_budget() {
        let truncated = truncate_text(&"a".repeat(1_000), 20);
        assert!(truncated.truncated);
        assert_eq!(truncated.original_token_count, 250);
        assert!(truncated.text.starts_with('a'));
        assert!(truncated.text.ends_with('a'));
        assert!(truncated.text.contains("output truncated"));
        assert!(approx_token_count(&truncated.text) <= 20);
    }

    #[test]
    fn text_truncation_keeps_utf8_boundaries() {
        let truncated = truncate_text(&"é😀".repeat(100), 12);
        assert!(truncated.truncated);
        assert!(approx_token_count(&truncated.text) <= 12);
        assert!(std::str::from_utf8(truncated.text.as_bytes()).is_ok());
    }

    #[test]
    fn result_budget_bounds_text_content() {
        let mut result = ToolResult::text("prefix".to_string() + &"x".repeat(1_000));
        let outcome = enforce_tool_result_budget(&mut result, 25);
        assert!(outcome.text_truncated);
        assert!(!outcome.structured_content_truncated);
        assert_eq!(result.audit.truncated, Some(true));
        assert!(approx_token_count(&result.joined_text()) <= 25);
    }

    #[test]
    fn oversized_structured_content_becomes_bounded_error() {
        let mut result = ToolResult::text("useful textual fallback")
            .with_structured(serde_json::json!({ "payload": "x".repeat(10_000) }));
        let outcome = enforce_tool_result_budget(&mut result, 50);
        assert!(outcome.structured_content_truncated);
        assert!(result.is_error);
        assert!(result.structured_content.is_none());
        assert!(
            result
                .joined_text()
                .contains("Retry with narrower arguments")
        );
        assert!(approx_token_count(&result.joined_text()) <= 50);
    }

    #[test]
    fn component_only_metadata_is_not_part_of_the_model_output_budget() {
        let mut meta = rmcp::model::MetaObject::new();
        meta.0
            .insert("widget".to_string(), Value::String("x".repeat(10_000)));
        let mut result = ToolResult {
            content: vec![ToolContent::Text("ok".to_string())],
            is_error: false,
            structured_content: None,
            meta: Some(meta),
            audit: Default::default(),
        };

        let outcome = enforce_tool_result_budget(&mut result, 5);
        assert_eq!(outcome, ToolResultBudgetOutcome::default());
        assert_eq!(result.joined_text(), "ok");
        assert_eq!(
            result
                .meta
                .as_ref()
                .and_then(|meta| meta.0.get("widget"))
                .and_then(Value::as_str)
                .map(str::len),
            Some(10_000)
        );
    }

    #[test]
    fn bounded_text_buffer_keeps_head_and_tail() {
        let mut buffer = BoundedTextBuffer::<16>::new();
        buffer.push_str("abcdefghijklmnopqrstuvwx");
        assert_eq!(buffer.retained_bytes(), 16);
        let output = buffer.take("test limit");
        assert!(output.truncated);
        assert_eq!(output.omitted_bytes, 8);
        assert!(output.text.starts_with("abcdefgh"));
        assert!(output.text.ends_with("qrstuvwx"));
    }

    #[test]
    fn bounded_text_buffer_decodes_utf8_after_chunk_reassembly() {
        let mut buffer = BoundedTextBuffer::<32>::new();
        let bytes = "a😀b".as_bytes();
        buffer.push_bytes(&bytes[..3]);
        buffer.push_bytes(&bytes[3..]);
        let output = buffer.take("test limit");
        assert_eq!(output.text, "a😀b");
        assert!(!output.truncated);
    }
}
