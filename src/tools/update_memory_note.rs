use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::exec_sessions::SessionState;
use crate::memory::{memory_enabled, update_note};
use crate::tool::{Tool, ToolBehavior, parse_tool_args, schema_for, text_output_schema};
use crate::types::{AppConfig, ToolResult};

pub struct UpdateMemoryNote;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateMemoryNoteArgs {
    /// Existing note key.
    #[schemars(length(min = 1))]
    key: String,
    /// Replacement note text.
    #[schemars(length(min = 1))]
    value: String,
}

#[async_trait]
impl Tool for UpdateMemoryNote {
    fn name(&self) -> &'static str {
        "update_memory_note"
    }

    fn title(&self) -> String {
        "Update memory note".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            true,
            true,
            false,
            "Replaces an existing durable note while refusing to create a missing key.",
        )
    }

    fn description(&self) -> String {
        "Replace the value of one existing durable project-memory note. The key must already exist; use remember to create a new note or forget_memory_note to remove one. Repeating the same value leaves the note unchanged."
            .to_string()
    }

    fn input_schema(&self) -> Value {
        schema_for::<UpdateMemoryNoteArgs>()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let UpdateMemoryNoteArgs { key, value } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        if key.trim().is_empty() {
            return ToolResult::error("key must be a non-empty string");
        }
        if value.trim().is_empty() {
            return ToolResult::error("value must be a non-empty string");
        }
        if !memory_enabled(config) {
            return ToolResult::error(
                "Persistent memory is disabled on this server (memory.enabled is false), so this note was not updated.",
            );
        }

        let config = config.clone();
        let now = chrono::Utc::now().to_rfc3339();
        let result =
            tokio::task::spawn_blocking(move || update_note(&config, &key, &value, &now)).await;
        let result = match result {
            Ok(result) => result,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        if result.ok {
            ToolResult::text(result.message)
        } else {
            ToolResult::error(result.message)
        }
    }
}
