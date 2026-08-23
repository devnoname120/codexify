use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::tool::Tool;
use crate::types::{AppConfig, ToolResult};

/// Codex allows up to 12 hours here. This bridge caps at 5 minutes: the MCP
/// transport is an HTTP request through a tunnel, and anything longer dies to an
/// idle timeout rather than returning. Over-long requests are rejected instead of
/// silently shortened so the caller is never told it slept longer than it did.
const MAX_SLEEP_DURATION_MS: u64 = 5 * 60 * 1000;

pub struct ClockSleep;

#[async_trait]
impl Tool for ClockSleep {
    fn name(&self) -> &'static str {
        "clock_sleep"
    }

    fn description(&self) -> String {
        format!(
            "Pause execution for a specified duration, then return the elapsed wall-clock time. Use this to wait between polls of a long-running exec_command session or an external job. Must be between 1 and {MAX_SLEEP_DURATION_MS} ms."
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "duration_ms": {
                    "type": "number",
                    "description": format!("How long to sleep in milliseconds. Must be between 1 and {MAX_SLEEP_DURATION_MS}.")
                }
            },
            "required": ["duration_ms"],
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "Elapsed wall-clock time and a completion message" }
            }
        }))
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        let Some(duration_ms) = args.get("duration_ms").and_then(|v| v.as_f64()) else {
            return ToolResult::error("duration_ms must be a number");
        };

        if duration_ms < 1.0 || duration_ms > MAX_SLEEP_DURATION_MS as f64 {
            return ToolResult::error(format!(
                "duration_ms must be between 1 and {MAX_SLEEP_DURATION_MS}"
            ));
        }

        let started = Instant::now();
        tokio::time::sleep(Duration::from_secs_f64(duration_ms / 1000.0)).await;
        let wall_time_seconds = started.elapsed().as_secs_f64();

        ToolResult::text(format!(
            "Wall time: {wall_time_seconds:.4} seconds\nSleep completed."
        ))
    }
}
