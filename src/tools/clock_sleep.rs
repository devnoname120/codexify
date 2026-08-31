use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolBehavior, ToolRequestContext, parse_tool_args};
use crate::types::{AppConfig, ToolResult};

/// Codex allows up to 12 hours here. This bridge caps at 5 minutes: the MCP
/// transport is an HTTP request through a tunnel, and anything longer dies to an
/// idle timeout rather than returning. Over-long requests are rejected instead of
/// silently shortened so the caller is never told it slept longer than it did.
const MAX_SLEEP_DURATION_MS: u64 = 5 * 60 * 1000;

pub struct ClockSleep;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClockSleepArgs {
    duration_ms: u64,
}

impl ClockSleep {
    async fn run(
        &self,
        args: Value,
        cancellation: Option<&tokio_util::sync::CancellationToken>,
    ) -> ToolResult {
        let ClockSleepArgs { duration_ms } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };

        if !(1..=MAX_SLEEP_DURATION_MS).contains(&duration_ms) {
            return ToolResult::error(format!(
                "duration_ms must be between 1 and {MAX_SLEEP_DURATION_MS}"
            ));
        }

        let started = Instant::now();
        let interrupted = if let Some(cancellation) = cancellation {
            let sleep = tokio::time::sleep(Duration::from_millis(duration_ms));
            tokio::pin!(sleep);
            tokio::select! {
                _ = &mut sleep => false,
                _ = cancellation.cancelled() => true,
            }
        } else {
            tokio::time::sleep(Duration::from_millis(duration_ms)).await;
            false
        };
        let wall_time_seconds = started.elapsed().as_secs_f64();
        let message = if interrupted {
            "Sleep interrupted by new input."
        } else {
            "Sleep completed."
        };

        ToolResult::text(format!(
            "Wall time: {wall_time_seconds:.4} seconds\n{message}"
        ))
    }
}

#[async_trait]
impl Tool for ClockSleep {
    fn name(&self) -> &'static str {
        "clock_sleep"
    }

    fn title(&self) -> String {
        "Sleep".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Waits locally without changing local or external state.",
        )
    }

    fn description(&self) -> String {
        "Pause execution for a specified duration. The sleep ends early when new input arrives for the active turn. Returns the elapsed wall-clock time.".to_string()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "duration_ms": {
                    "type": "number",
                    "minimum": 1,
                    "maximum": MAX_SLEEP_DURATION_MS,
                    "description": format!("How long to sleep in milliseconds. Must be between 1 and {MAX_SLEEP_DURATION_MS}.")
                }
            },
            "required": ["duration_ms"],
            "additionalProperties": false
        })
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        self.run(args, None).await
    }

    async fn call_with_context(
        &self,
        args: Value,
        _config: &AppConfig,
        _session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        self.run(args, Some(&context.cancellation)).await
    }
}
