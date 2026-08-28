use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolBehavior, empty_object_schema};
use crate::types::{AppConfig, ToolResult};

pub struct ClockCurrTime;

/// Formats the current time as Codex does: `YYYY-MM-DD HH:MM:SS UTC`.
fn format_utc_now() -> String {
    Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

#[async_trait]
impl Tool for ClockCurrTime {
    fn name(&self) -> &'static str {
        "clock_curr_time"
    }

    fn title(&self) -> String {
        "Get current time".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Reads the local clock without changing local or external state.",
        )
    }

    fn description(&self) -> String {
        "Return the current time in UTC. Use this to timestamp work, measure how long something took, or reason about deadlines — the conversation itself carries no reliable clock.".into()
    }

    fn input_schema(&self) -> Value {
        empty_object_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "current_time": {
                    "type": "string",
                    "description": "Current UTC time formatted as YYYY-MM-DD HH:MM:SS UTC."
                }
            },
            "required": ["current_time"],
            "additionalProperties": false
        }))
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, _args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        let current_time = format_utc_now();
        ToolResult::text(current_time.clone())
            .with_structured(json!({ "current_time": current_time }))
    }
}
