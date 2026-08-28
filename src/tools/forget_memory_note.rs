use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::exec_sessions::SessionState;
use crate::memory::{delete_note, memory_enabled};
use crate::tool::{Tool, ToolBehavior, parse_tool_args, schema_for, text_output_schema};
use crate::types::{AppConfig, ToolResult};

pub struct ForgetMemoryNote;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ForgetMemoryNoteArgs {
    /// Existing note key to remove.
    #[schemars(length(min = 1))]
    key: String,
}

#[async_trait]
impl Tool for ForgetMemoryNote {
    fn name(&self) -> &'static str {
        "forget_memory_note"
    }

    fn title(&self) -> String {
        "Forget memory note".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            true,
            true,
            false,
            "Deletes one durable note from project memory.",
        )
    }

    fn description(&self) -> String {
        "Delete one existing durable project-memory note by key. Use recall first when the exact key is unknown. This does not modify project files."
            .to_string()
    }

    fn input_schema(&self) -> Value {
        schema_for::<ForgetMemoryNoteArgs>()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let ForgetMemoryNoteArgs { key } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        if key.trim().is_empty() {
            return ToolResult::error("key must be a non-empty string");
        }
        if !memory_enabled(config) {
            return ToolResult::error(
                "Persistent memory is disabled on this server (memory.enabled is false), so this note was not removed.",
            );
        }

        let config = config.clone();
        let result = tokio::task::spawn_blocking(move || delete_note(&config, &key)).await;
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
