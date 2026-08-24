use std::path::{Path, PathBuf};

use async_trait::async_trait;
use globset::GlobBuilder;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::ignore_rules::{IgnoreMatcher, build_ignore, to_rel_posix};
use crate::output_budget::{entry_budget, limit_list};
use crate::safe_path::resolve_safe_path;
use crate::tool::{Tool, arg_str};
use crate::types::{AppConfig, ToolResult};

pub struct Glob;

/// Whether the pattern opts into matching dot-prefixed path segments. fast-glob
/// runs with `dot: false`, so wildcard segments never match a leading `.`; a
/// pattern only reaches hidden files/directories when it names them with a
/// literal dot.
fn pattern_allows_dot(pattern: &str) -> bool {
    pattern.split('/').any(|seg| seg.starts_with('.'))
}

/// Apply fast-glob's `dot: false` rule to a candidate relative path.
fn dot_ok(rel: &str, allow_dot: bool) -> bool {
    allow_dot || !rel.split('/').any(|seg| seg.starts_with('.'))
}

/// Recursively collect files under `base`, pruning ignored directories. Mirrors
/// fast-glob's `onlyFiles: true` by descending directories and keeping every
/// non-directory entry.
fn collect_files(base: &Path, matcher: &IgnoreMatcher, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(base) {
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
                collect_files(&path, matcher, out);
            }
        } else {
            out.push(path);
        }
    }
}

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> String {
        "Find files matching a glob pattern within the project. Supports patterns like **/*.ts (all TypeScript files), src/**/*.test.ts (test files in src), *.json (JSON files in root). Returns a sorted list of matching file paths. Use this to discover files before reading them, or to understand the project structure.".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern to match" },
                "path": { "type": "string", "description": "Subdirectory to search in. Default: work-dir root" }
            },
            "required": ["pattern"]
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "Newline-separated list of matched file paths relative to work-dir" }
            }
        }))
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let Some(pattern) = arg_str(&args, "pattern") else {
            return ToolResult::error("pattern must be a string");
        };

        // An empty `path` is falsy in the TS and falls back to the work dir.
        let base_path = match arg_str(&args, "path").filter(|p| !p.is_empty()) {
            Some(p) => match resolve_safe_path(p, &config.work_dir, false) {
                Ok(p) => p,
                Err(e) => return ToolResult::error(e),
            },
            None => config.work_dir.clone(),
        };

        // fast-glob treats an unparseable pattern (e.g. an unclosed bracket) as a
        // literal rather than erroring; the common outcome is no match.
        let glob = match GlobBuilder::new(pattern).literal_separator(true).build() {
            Ok(g) => g.compile_matcher(),
            Err(_) => return ToolResult::text("No files found matching pattern."),
        };
        let allow_dot = pattern_allows_dot(pattern);

        let matcher = build_ignore(config);

        let mut all_files: Vec<PathBuf> = Vec::new();
        collect_files(&base_path, &matcher, &mut all_files);

        // fast-glob does not sandbox its `cwd`: patterns like "../../etc/passwd"
        // or absolute patterns can match outside of `basePath`. Re-validate every
        // match against the boundary so glob can't be used to bypass
        // resolveSafePath. Then drop anything the ignore policy hides — the walk's
        // pruning is a perf floor only, so correctness is enforced here.
        let mut files: Vec<String> = Vec::new();
        for abs in &all_files {
            let Some(rel) = to_rel_posix(abs, &base_path) else {
                continue;
            };
            if !glob.is_match(&rel) {
                continue;
            }
            if !dot_ok(&rel, allow_dot) {
                continue;
            }
            if resolve_safe_path(&rel, &base_path, false).is_err() {
                continue;
            }
            if matcher.is_ignored(abs, false) {
                continue;
            }
            files.push(rel);
        }

        if files.is_empty() {
            return ToolResult::text("No files found matching pattern.");
        }

        files.sort();
        let (items, dropped) = limit_list(files, entry_budget(config));
        let text = if dropped > 0 {
            format!(
                "{}\n\n(showing {} of {} matches — narrow the pattern or point \"path\" at a subdirectory)",
                items.join("\n"),
                items.len(),
                items.len() + dropped
            )
        } else {
            items.join("\n")
        };
        ToolResult::text(text).with_truncation(dropped > 0)
    }
}
