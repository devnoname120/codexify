use async_trait::async_trait;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::safe_path::resolve_safe_path;
use crate::tool::{Tool, arg_str};
use crate::types::{AppConfig, ToolResult};

pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> String {
        "Write or overwrite a file with the given content. Path is relative to the project root (work-dir). Parent directories are created automatically. Use this to create new files, update existing files, or save generated code. Always read the file first before overwriting to avoid losing content.".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to work-dir" },
                "content": { "type": "string", "description": "Content to write" }
            },
            "required": ["path", "content"]
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "Confirmation message with bytes written and file path" }
            }
        }))
    }

    fn may_modify_project(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let Some(path) = arg_str(&args, "path") else {
            return ToolResult::error("path must be a string");
        };
        let Some(content) = arg_str(&args, "content") else {
            return ToolResult::error("content must be a string");
        };

        let file_path = match resolve_safe_path(path, &config.work_dir, false) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };

        if let Some(parent) = file_path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolResult::error(e.to_string());
        }

        if let Err(e) = tokio::fs::write(&file_path, content).await {
            return ToolResult::error(e.to_string());
        }

        ToolResult::text(format!("Written {} bytes to {}", content.len(), path))
    }
}
