use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::exec_sessions::SessionState;
use crate::output_budget::{file_budget, window_file_lines};
use crate::safe_path::resolve_safe_path;
use crate::tool::{Tool, ToolBehavior, parse_tool_args, schema_for, text_output_schema};
use crate::types::{AppConfig, ToolResult};

pub struct ReadFile;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadFileArgs {
    /// Project-relative file path.
    #[schemars(length(min = 1))]
    path: String,
    /// Zero-based first line. Defaults to 0.
    offset: Option<u64>,
    /// Maximum lines to return before the server's own line and byte ceilings.
    limit: Option<u64>,
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn title(&self) -> String {
        "Read file".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Reads one project file without changing local or external state.",
        )
    }

    fn description(&self) -> String {
        "Read the contents of a file. Path is relative to the project root (work-dir). Output is prefixed with line numbers (e.g. '1\\tconst x = 1'). Large files come back a window at a time; when the result says so, call again with the offset it names. Use this tool to inspect source code, configs, or any text file before making changes.".into()
    }

    fn input_schema(&self) -> Value {
        schema_for::<ReadFileArgs>()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let ReadFileArgs {
            path,
            offset,
            limit,
        } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };

        let file_path = match resolve_safe_path(&path, &config.work_dir, false) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };

        if !file_path.is_file() {
            return ToolResult::error(format!("File not found: {path}"));
        }

        // Read as bytes and decode lossily, matching Bun's `file.text()` which
        // replaces invalid UTF-8 with U+FFFD rather than failing.
        let bytes = match tokio::fs::read(&file_path).await {
            Ok(b) => b,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();

        let lines: Vec<&str> = text.split('\n').collect();
        let offset = offset
            .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
            .unwrap_or(0);
        let limit = limit.map(|value| usize::try_from(value).unwrap_or(usize::MAX));
        let window = window_file_lines(&lines, offset, limit, file_budget(config));
        let truncated = window.notice.is_some();

        let numbered = window
            .lines
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}\t{}", window.start + i + 1, line))
            .collect::<Vec<_>>()
            .join("\n");

        let body = match window.notice {
            Some(notice) => format!("{numbered}\n\n{notice}"),
            None => numbered,
        };
        ToolResult::text(body).with_truncation(truncated)
    }
}
