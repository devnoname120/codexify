use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::ignore_rules::{IgnoreMatcher, build_ignore, to_rel_posix};
use crate::safe_path::resolve_safe_path;
use crate::tool::{Tool, arg_bool, arg_str};
use crate::types::{AppConfig, ToolResult};

pub struct Grep;

/// File extensions treated as binary and never searched.
const BINARY_EXTENSIONS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".ico", ".svg", ".woff", ".woff2", ".ttf", ".eot",
    ".zip", ".tar", ".gz", ".br", ".7z", ".exe", ".dll", ".so", ".dylib", ".pdf", ".doc", ".docx",
    ".mp3", ".mp4", ".avi", ".mov", ".wasm", ".o", ".a", ".lib",
];

/// Recursively collect searchable files under `dir`, skipping binary
/// extensions, an optional `*.ext` include filter, pruned directories, and any
/// path the ignore policy hides.
fn collect_files(
    dir: &Path,
    ext_match: Option<&str>,
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
                collect_files(&path, ext_match, matcher, out);
            }
        } else {
            let ext = match name.rfind('.') {
                Some(i) => &name[i..],
                None => "",
            };
            if BINARY_EXTENSIONS.contains(&ext) {
                continue;
            }
            if let Some(em) = ext_match
                && ext != em
            {
                continue;
            }
            if matcher.is_ignored(&path, false) {
                continue;
            }
            out.push(path);
        }
    }
}

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> String {
        "Search file contents across the project using a regex pattern. Returns matching lines with file paths and line numbers (e.g. 'src/app.ts:42:const server = ...'). Recursively searches all text files, skipping binary files and common directories (node_modules, .git, dist). Use this to find function definitions, usages, error messages, or any text across the codebase.".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern to search for" },
                "path": { "type": "string", "description": "Subdirectory to search in. Default: work-dir root" },
                "include": { "type": "string", "description": "Only search files matching this glob (e.g. *.ts)" },
                "context": { "type": "number", "description": "Number of context lines before and after each match" },
                "ignoreCase": { "type": "boolean", "description": "Case-insensitive search. Default: false" },
                "maxResults": { "type": "number", "description": "Max number of matching lines to return. Default: 500" },
                "filesOnly": { "type": "boolean", "description": "Only return file paths that contain matches, not the matching lines" }
            },
            "required": ["pattern"]
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "Grep results as 'file:line:match' lines with optional context, or file paths if filesOnly is true" }
            }
        }))
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        // An empty `path` is falsy in the TS and falls back to the work dir.
        let search_path = match arg_str(&args, "path").filter(|p| !p.is_empty()) {
            Some(p) => match resolve_safe_path(p, &config.work_dir, false) {
                Ok(p) => p,
                Err(e) => return ToolResult::error(e),
            },
            None => config.work_dir.clone(),
        };

        let context_lines = args
            .get("context")
            .and_then(|v| v.as_i64())
            .filter(|n| *n >= 0)
            .unwrap_or(0);
        let include_pattern = arg_str(&args, "include");
        let ignore_case = arg_bool(&args, "ignoreCase");
        let max_results = args
            .get("maxResults")
            .and_then(|v| v.as_i64())
            .unwrap_or(500);
        let files_only = arg_bool(&args, "filesOnly");

        let pattern = arg_str(&args, "pattern").unwrap_or("");
        let regex: Regex = match RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .build()
        {
            Ok(r) => r,
            Err(_) => return ToolResult::error(format!("Invalid regex: {pattern}")),
        };

        // include of the form "*.ext" is honoured as an extension filter, as in
        // the TS collectFiles.
        let ext_match: Option<String> = include_pattern
            .filter(|g| g.starts_with("*."))
            .map(|g| g[1..].to_string());

        let matcher = build_ignore(config);
        let mut files: Vec<PathBuf> = Vec::new();
        collect_files(&search_path, ext_match.as_deref(), &matcher, &mut files);

        let mut results: Vec<String> = Vec::new();
        let mut match_count: i64 = 0;
        let mut truncated = false;

        for file_path in &files {
            if truncated {
                break;
            }

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
                        results.push(rel_path.clone());
                        match_count += 1;
                        if match_count >= max_results {
                            truncated = true;
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
                        truncated = true;
                        break;
                    }
                    let start = (i as i64 - context_lines).max(0) as usize;
                    let end = i.saturating_add(context_lines as usize).min(last);
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
                if context_lines > 0 && (idx as i64) - prev_idx > 1 && prev_idx >= 0 {
                    results.push("--".to_string());
                }
                let marker = if regex.is_match(lines[idx]) { ":" } else { "-" };
                results.push(format!("{}:{}{}{}", rel_path, idx + 1, marker, lines[idx]));
                prev_idx = idx as i64;
            }
        }

        if results.is_empty() {
            return ToolResult::text("No matches found.");
        }

        let mut output = results.join("\n");
        if truncated {
            output.push_str(&format!("\n\n(truncated at {max_results} matches)"));
        }
        ToolResult::text(output).with_truncation(truncated)
    }
}
