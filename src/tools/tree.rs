use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::cmp::Ordering;
use std::path::Path;

use crate::exec_sessions::SessionState;
use crate::ignore_rules::{IgnoreMatcher, build_ignore};
use crate::output_budget::tree_node_budget;
use crate::safe_path::resolve_safe_path;
use crate::tool::{Tool, ToolBehavior, parse_tool_args, schema_for, text_output_schema};
use crate::types::{AppConfig, ToolResult};

pub struct Tree;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TreeArgs {
    /// Project-relative directory. Defaults to the project root.
    path: Option<String>,
    /// Maximum traversal depth. Defaults to the configured tree depth.
    depth: Option<usize>,
}

#[async_trait]
impl Tool for Tree {
    fn name(&self) -> &'static str {
        "tree"
    }

    fn title(&self) -> String {
        "Show directory tree".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Walks local project directories without modifying them or external systems.",
        )
    }

    fn description(&self) -> String {
        "Show the project directory structure as an ASCII tree. Automatically ignores common directories (node_modules, .git, dist, __pycache__). Use this first to understand the project layout before diving into specific files. Depth defaults to the server's configured tree depth and the walk stops at a node budget, so point 'path' at a subdirectory rather than asking for a deep tree of the whole repo.".into()
    }

    fn input_schema(&self) -> Value {
        schema_for::<TreeArgs>()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let TreeArgs { path, depth } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        let root_path = match path.as_deref().filter(|path| !path.is_empty()) {
            Some(p) => match resolve_safe_path(p, &config.work_dir, false) {
                Ok(resolved) => resolved,
                Err(e) => return ToolResult::error(e),
            },
            None => config.work_dir.clone(),
        };
        let max_depth = depth.unwrap_or(config.tree.default_depth);
        let ig = build_ignore(config);

        let mut state = WalkState {
            lines: vec![".".to_string()],
            remaining: tree_node_budget(config),
            stopped: false,
        };
        if let Err(e) = build_tree(&root_path, "", 0, max_depth, &ig, &mut state) {
            return ToolResult::error(e);
        }

        let text = if state.stopped {
            format!(
                "{}\n\n(stopped at {} nodes \u{2014} lower \"depth\" or point \"path\" at a subdirectory)",
                state.lines.join("\n"),
                state.lines.len() - 1
            )
        } else {
            state.lines.join("\n")
        };
        ToolResult::text(text).with_truncation(state.stopped)
    }
}

/// Walk state shared across the recursion. `remaining` is a whole-tree budget
/// rather than a per-directory one, so a single enormous directory cannot starve
/// its siblings out of the output without the result saying so.
struct WalkState {
    lines: Vec<String>,
    remaining: usize,
    stopped: bool,
}

struct Entry {
    name: String,
    is_dir: bool,
}

fn build_tree(
    dir_path: &Path,
    prefix: &str,
    depth: usize,
    max_depth: usize,
    ig: &IgnoreMatcher,
    state: &mut WalkState,
) -> Result<(), String> {
    if depth >= max_depth || state.stopped {
        return Ok(());
    }

    let rd = std::fs::read_dir(dir_path).map_err(|e| e.to_string())?;
    let mut filtered: Vec<Entry> = Vec::new();
    for entry in rd {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let abs = dir_path.join(&name);
        // Directories the walk should never descend are also never shown; a file
        // the ignore policy hides is dropped the same way.
        let keep = if is_dir {
            !ig.should_prune(&name, &abs)
        } else {
            !ig.is_ignored(&abs, false)
        };
        if keep {
            filtered.push(Entry { name, is_dir });
        }
    }

    filtered.sort_by(|a, b| {
        if a.is_dir && !b.is_dir {
            Ordering::Less
        } else if !a.is_dir && b.is_dir {
            Ordering::Greater
        } else {
            a.name.cmp(&b.name)
        }
    });

    let len = filtered.len();
    for (i, entry) in filtered.iter().enumerate() {
        if state.remaining == 0 {
            state.stopped = true;
            return Ok(());
        }

        let is_last = i == len - 1;
        let connector = if is_last {
            "\u{2514}\u{2500}\u{2500} "
        } else {
            "\u{251c}\u{2500}\u{2500} "
        };
        let child_prefix = if is_last { "    " } else { "\u{2502}   " };

        state.lines.push(format!(
            "{prefix}{connector}{}{}",
            entry.name,
            if entry.is_dir { "/" } else { "" }
        ));
        state.remaining -= 1;

        if entry.is_dir {
            let next_prefix = format!("{prefix}{child_prefix}");
            build_tree(
                &dir_path.join(&entry.name),
                &next_prefix,
                depth + 1,
                max_depth,
                ig,
                state,
            )?;
            if state.stopped {
                return Ok(());
            }
        }
    }

    Ok(())
}
