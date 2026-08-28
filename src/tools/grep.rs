use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use globset::{GlobBuilder, GlobMatcher};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::ignore_rules::{IgnoreMatcher, build_ignore, to_rel_posix};
use crate::output_budget::{
    approx_bytes_for_tokens, entry_budget, tool_output_token_budget, truncate_text,
};
use crate::safe_path::resolve_safe_path;
use crate::tool::{Tool, ToolBehavior, parse_tool_args, text_output_schema};
use crate::types::{AppConfig, ToolResult};

pub struct Grep;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    include: Option<String>,
    context: Option<u64>,
    #[serde(default)]
    ignore_case: bool,
    max_results: Option<u64>,
    #[serde(default)]
    files_only: bool,
}

const DEFAULT_MAX_RESULTS: usize = 500;
const MAX_CONTEXT_LINES: usize = 20;
const MAX_RENDERED_LINE_BYTES: usize = 4_096;
const OUTPUT_NOTICE_RESERVE_BYTES: usize = 256;

/// File extensions treated as binary and never searched.
const BINARY_EXTENSIONS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".ico", ".svg", ".woff", ".woff2", ".ttf", ".eot",
    ".zip", ".tar", ".gz", ".br", ".7z", ".exe", ".dll", ".so", ".dylib", ".pdf", ".doc", ".docx",
    ".mp3", ".mp4", ".avi", ".mov", ".wasm", ".o", ".a", ".lib",
];

/// Recursively collect searchable files under `dir`, skipping binary
/// extensions, an optional basename or relative-path include glob, pruned directories, and any
/// path the ignore policy hides.
fn collect_files(
    dir: &Path,
    root: &Path,
    include: Option<&IncludeMatcher>,
    matcher: &IgnoreMatcher,
    out: &mut Vec<PathBuf>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = match entry.file_type() {
            Ok(ft) => ft.is_dir(),
            Err(_) => continue,
        };
        if is_dir {
            if !matcher.should_prune(&name, &path) {
                collect_files(&path, root, include, matcher, out);
            }
        } else {
            let ext = match name.rfind('.') {
                Some(i) => &name[i..],
                None => "",
            };
            if BINARY_EXTENSIONS.contains(&ext) {
                continue;
            }
            if include.is_some_and(|include| !include.is_match(&path, root)) {
                continue;
            }
            if matcher.is_ignored(&path, false) {
                continue;
            }
            out.push(path);
        }
    }
}

struct IncludeMatcher {
    matcher: GlobMatcher,
    basename_only: bool,
}

impl IncludeMatcher {
    fn compile(pattern: &str) -> Result<Self, String> {
        let matcher = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|error| format!("Invalid include glob {pattern:?}: {error}"))?
            .compile_matcher();
        Ok(Self {
            matcher,
            basename_only: !pattern.contains(['/', '\\']),
        })
    }

    fn is_match(&self, path: &Path, root: &Path) -> bool {
        let Some(relative) = to_rel_posix(path, root) else {
            return false;
        };
        if self.basename_only {
            relative
                .rsplit('/')
                .next()
                .is_some_and(|name| self.matcher.is_match(name))
        } else {
            self.matcher.is_match(relative)
        }
    }
}

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn title(&self) -> String {
        "Search file contents".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Searches local project files without modifying them or contacting external systems.",
        )
    }

    fn description(&self) -> String {
        "Search file contents across the project using a regex pattern. Returns bounded matching lines with file paths and line numbers (e.g. 'src/app.ts:42:const server = ...'); long lines are elided around the actual match. Recursively searches text files while respecting ignore rules. Use this to find definitions, usages, error messages, or other codebase text.".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern to search for" },
                "path": { "type": "string", "description": "Subdirectory to search in. Default: work-dir root" },
                "include": { "type": "string", "description": "Only search files matching this glob (e.g. *.ts)" },
                "context": { "type": "integer", "minimum": 0, "description": format!("Number of context lines before and after each match. Values above {MAX_CONTEXT_LINES} are capped.") },
                "ignoreCase": { "type": "boolean", "description": "Case-insensitive search. Default: false" },
                "maxResults": { "type": "integer", "minimum": 1, "description": format!("Max number of matching lines to return. Defaults to {DEFAULT_MAX_RESULTS} and cannot exceed the configured output.maxEntries limit.") },
                "filesOnly": { "type": "boolean", "description": "Only return file paths that contain matches, not the matching lines" }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let GrepArgs {
            pattern,
            path,
            include,
            context,
            ignore_case,
            max_results,
            files_only,
        } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        // An empty `path` is falsy in the TS and falls back to the work dir.
        let search_path = match path.as_deref().filter(|path| !path.is_empty()) {
            Some(p) => match resolve_safe_path(p, &config.work_dir, false) {
                Ok(p) => p,
                Err(e) => return ToolResult::error(e),
            },
            None => config.work_dir.clone(),
        };

        let requested_context = context
            .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
            .unwrap_or(0);
        let context_lines = requested_context.min(MAX_CONTEXT_LINES);
        let context_clamped = requested_context > context_lines;
        let include = match include.as_deref() {
            Some(pattern) => match IncludeMatcher::compile(pattern) {
                Ok(matcher) => Some(matcher),
                Err(error) => return ToolResult::error(error),
            },
            None => None,
        };
        let requested_max_results = max_results;
        if requested_max_results == Some(0) {
            return ToolResult::error("maxResults must be positive");
        }
        let configured_max_results = entry_budget(config);
        let requested_max_results = requested_max_results
            .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
            .unwrap_or(DEFAULT_MAX_RESULTS);
        let max_results = if configured_max_results == 0 {
            requested_max_results
        } else {
            requested_max_results.min(configured_max_results)
        };
        let max_results_clamped = requested_max_results > max_results;
        let regex: Regex = match RegexBuilder::new(&pattern)
            .case_insensitive(ignore_case)
            .build()
        {
            Ok(r) => r,
            Err(_) => return ToolResult::error(format!("Invalid regex: {pattern}")),
        };

        let matcher = build_ignore(config);
        let mut files: Vec<PathBuf> = Vec::new();
        collect_files(
            &search_path,
            &search_path,
            include.as_ref(),
            &matcher,
            &mut files,
        );

        let output_tokens = tool_output_token_budget(config);
        let output_bytes = approx_bytes_for_tokens(output_tokens);
        let mut results =
            OutputCollector::new(output_bytes.saturating_sub(OUTPUT_NOTICE_RESERVE_BYTES));
        let mut match_count = 0usize;
        let mut match_limit_reached = false;
        let mut output_limit_reached = false;
        let mut line_content_truncated = false;

        'files: for file_path in &files {
            let bytes = match std::fs::read(file_path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let content = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = content.split('\n').collect();
            let rel_path = to_rel_posix(file_path, &config.work_dir)
                .unwrap_or_else(|| file_path.to_string_lossy().replace('\\', "/"));

            if files_only {
                for line in &lines {
                    if regex.is_match(line) {
                        match_count += 1;
                        if match_count > max_results {
                            match_limit_reached = true;
                            break 'files;
                        }
                        if !results.push_line(&rel_path) {
                            output_limit_reached = true;
                            break 'files;
                        }
                        break;
                    }
                }
                continue;
            }

            let mut match_indices: BTreeSet<usize> = BTreeSet::new();
            let last = lines.len().saturating_sub(1);

            for (i, line) in lines.iter().enumerate() {
                if regex.is_match(line) {
                    match_count += 1;
                    if match_count > max_results {
                        match_limit_reached = true;
                        break;
                    }
                    let start = i.saturating_sub(context_lines);
                    let end = i.saturating_add(context_lines).min(last);
                    for j in start..=end {
                        match_indices.insert(j);
                    }
                }
            }

            if match_indices.is_empty() {
                continue;
            }

            let mut prev_idx: i64 = -2;
            for idx in &match_indices {
                let idx = *idx;
                if context_lines > 0
                    && (idx as i64) - prev_idx > 1
                    && prev_idx >= 0
                    && !results.push_line("--")
                {
                    output_limit_reached = true;
                    break 'files;
                }
                let matched = regex.is_match(lines[idx]);
                let marker = if matched { ":" } else { "-" };
                let (rendered, line_truncated) = render_result_line(
                    &rel_path,
                    idx + 1,
                    marker,
                    lines[idx],
                    matched.then(|| regex.find(lines[idx])).flatten(),
                    results.remaining_line_bytes().min(MAX_RENDERED_LINE_BYTES),
                );
                line_content_truncated |= line_truncated;
                if !results.push_line(&rendered) {
                    output_limit_reached = true;
                    break 'files;
                }
                prev_idx = idx as i64;
            }
            if match_limit_reached {
                break 'files;
            }
        }

        if results.is_empty() && match_count == 0 {
            return ToolResult::text("No matches found.");
        }

        let mut reasons = Vec::new();
        if match_limit_reached {
            reasons.push(format!("stopped at {max_results} matches"));
        }
        if output_limit_reached {
            reasons.push(format!(
                "stopped at the configured {output_tokens}-token model-output limit"
            ));
        }
        if context_clamped {
            reasons.push(format!("context capped at {MAX_CONTEXT_LINES} lines"));
        }
        if line_content_truncated {
            reasons.push("long result lines were elided".to_string());
        }
        if match_limit_reached && max_results_clamped {
            reasons.push(format!("maxResults capped at {max_results}"));
        }

        let mut output = results.finish();
        if !reasons.is_empty() {
            if !output.is_empty() {
                output.push_str("\n\n");
            }
            output.push_str(&format!("(truncated: {})", reasons.join("; ")));
        }
        let bounded = truncate_text(&output, output_tokens);
        ToolResult::text(bounded.text)
            .with_truncation(!reasons.is_empty() || results.truncated || bounded.truncated)
    }
}

struct OutputCollector {
    text: String,
    max_bytes: usize,
    truncated: bool,
}

impl OutputCollector {
    fn new(max_bytes: usize) -> Self {
        Self {
            text: String::new(),
            max_bytes,
            truncated: false,
        }
    }

    fn push_line(&mut self, line: &str) -> bool {
        let separator = usize::from(!self.text.is_empty());
        if self
            .text
            .len()
            .saturating_add(separator)
            .saturating_add(line.len())
            > self.max_bytes
        {
            self.truncated = true;
            return false;
        }
        if separator > 0 {
            self.text.push('\n');
        }
        self.text.push_str(line);
        true
    }

    fn remaining_line_bytes(&self) -> usize {
        let separator = usize::from(!self.text.is_empty());
        self.max_bytes
            .saturating_sub(self.text.len().saturating_add(separator))
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn finish(&self) -> String {
        self.text.clone()
    }
}

fn render_result_line(
    path: &str,
    line_number: usize,
    marker: &str,
    line: &str,
    matched_range: Option<regex::Match<'_>>,
    max_bytes: usize,
) -> (String, bool) {
    let prefix = format!("{path}:{line_number}{marker}");
    if prefix.len() >= max_bytes {
        return bounded_line(&prefix, None, max_bytes);
    }
    let available = max_bytes.saturating_sub(prefix.len());
    let snippet = bounded_line(
        line,
        matched_range.map(|matched| (matched.start(), matched.end())),
        available,
    );
    (format!("{prefix}{}", snippet.0), snippet.1)
}

fn bounded_line(line: &str, focus: Option<(usize, usize)>, max_bytes: usize) -> (String, bool) {
    if line.len() <= max_bytes {
        return (line.to_string(), false);
    }
    if max_bytes <= 8 {
        return (
            line[..floor_char_boundary(line, max_bytes)].to_string(),
            true,
        );
    }

    let content_bytes = max_bytes - 8;
    let (focus_start, focus_end) = focus.unwrap_or((0, 0));
    let focus_len = focus_end.saturating_sub(focus_start);
    let (mut start, mut end) = if focus_len >= content_bytes {
        (
            focus_start,
            focus_start.saturating_add(content_bytes).min(line.len()),
        )
    } else {
        let surrounding = content_bytes - focus_len;
        let before = surrounding / 2;
        let mut start = focus_start.saturating_sub(before);
        let mut end = focus_end.saturating_add(surrounding - (focus_start - start));
        if end > line.len() {
            let shift = end - line.len();
            start = start.saturating_sub(shift);
            end = line.len();
        }
        (start, end)
    };

    start = floor_char_boundary(line, start);
    end = floor_char_boundary(line, end.max(start));
    let mut snippet = String::with_capacity(max_bytes);
    if start > 0 {
        snippet.push_str("... ");
    }
    snippet.push_str(&line[start..end]);
    if end < line.len() {
        snippet.push_str(" ...");
    }
    (snippet, true)
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let mut index = index;
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}
