use async_trait::async_trait;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::output_budget::{file_budget, window_file_lines};
use crate::safe_path::resolve_safe_path;
use crate::tool::{Tool, arg_f64, arg_str};
use crate::types::{AppConfig, ToolResult};

pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> String {
        "Read the contents of a file. Path is relative to the project root (work-dir). Output is prefixed with line numbers (e.g. '1\\tconst x = 1'). Large files come back a window at a time; when the result says so, call again with the offset it names. Use this tool to inspect source code, configs, or any text file before making changes.".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to work-dir" },
                "offset": { "type": "number", "description": "Start reading from this line (0-based). Default: 0" },
                "limit": { "type": "number", "description": "Maximum number of lines to return. Capped by the server's own line and byte budget." }
            },
            "required": ["path"]
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "File content with line numbers prefixed (e.g. '1\\tline text')" }
            }
        }))
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let Some(path) = arg_str(&args, "path") else {
            return ToolResult::error("path must be a string");
        };

        let file_path = match resolve_safe_path(path, &config.work_dir, false) {
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
        // `offset`/`limit` accept any JSON number: truncated toward zero, and a
        // negative value floors to zero, matching the TS `Math.trunc` / clamp.
        let offset = arg_f64(&args, "offset")
            .map(|f| f.trunc())
            .filter(|f| *f >= 0.0)
            .map(|f| f as usize)
            .unwrap_or(0);
        let limit = arg_f64(&args, "limit").map(|f| if f <= 0.0 { 0 } else { f.trunc() as usize });
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
