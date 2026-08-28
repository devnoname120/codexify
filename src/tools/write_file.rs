use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::exec_sessions::SessionState;
use crate::safe_path::resolve_safe_path;
use crate::tool::{Tool, ToolBehavior, parse_tool_args, schema_for, text_output_schema};
use crate::types::{AppConfig, ToolResult};

pub struct WriteFile;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteFileArgs {
    /// Project-relative destination path.
    #[schemars(length(min = 1))]
    path: String,
    /// Complete replacement file content.
    content: String,
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn title(&self) -> String {
        "Write file".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            true,
            true,
            false,
            "Creates or overwrites a local project file; overwriting may destroy prior content.",
        )
    }

    fn description(&self) -> String {
        "Write or overwrite a file with the given content. Path is relative to the project root (work-dir). Parent directories are created automatically. Use this to create new files, update existing files, or save generated code. Always read the file first before overwriting to avoid losing content.".into()
    }

    fn input_schema(&self) -> Value {
        schema_for::<WriteFileArgs>()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    fn may_modify_project(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let WriteFileArgs { path, content } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };

        let file_path = match resolve_safe_path(&path, &config.work_dir, false) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };

        if let Some(parent) = file_path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolResult::error(e.to_string());
        }

        if let Err(e) = tokio::fs::write(&file_path, &content).await {
            return ToolResult::error(e.to_string());
        }

        ToolResult::text(format!("Written {} bytes to {}", content.len(), path))
    }
}
