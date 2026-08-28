use async_trait::async_trait;
use serde_json::Value;

use crate::exec_sessions::SessionState;
use crate::memory::{load_memory, memory_enabled, render_memory};
use crate::tool::{Tool, ToolBehavior, empty_object_schema, text_output_schema};
use crate::types::{AppConfig, ToolResult};

pub const NOTHING_REMEMBERED: &str =
    "Nothing remembered for this project yet. This is a fresh start, not a lost history.";

pub struct Recall;

#[async_trait]
impl Tool for Recall {
    fn name(&self) -> &'static str {
        "recall"
    }

    fn title(&self) -> String {
        "Recall project memory".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Reads persisted project memory without modifying it.",
        )
    }

    fn description(&self) -> String {
        "Return the plan and the notes saved for this project by earlier turns or earlier conversations. Call this when you have lost the thread — a new chat, a task you are resuming, or any point where you are about to ask the user to repeat something they may already have told you. It is cheap and returns nothing when nothing was stored.".into()
    }

    fn input_schema(&self) -> Value {
        empty_object_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    async fn call(&self, _args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        if !memory_enabled(config) {
            return ToolResult::text(
                "Persistent memory is disabled on this server (memory.enabled is false). Nothing is stored between conversations.",
            );
        }

        let config = config.clone();
        let memory = match tokio::task::spawn_blocking(move || load_memory(&config)).await {
            Ok(m) => m,
            Err(e) => return ToolResult::error(e.to_string()),
        };

        let rendered = render_memory(&memory).unwrap_or_else(|| NOTHING_REMEMBERED.to_string());
        ToolResult::text(rendered)
    }
}
