use async_trait::async_trait;
use serde_json::{Value, json};

use crate::exec_sessions::{
    DEFAULT_MAX_OUTPUT_TOKENS, EXEC_MAX_YIELD_MS, STDIN_POLL_DEFAULT_YIELD_MS,
    STDIN_POLL_MAX_YIELD_MS, STDIN_WRITE_DEFAULT_YIELD_MS, SessionState, UnifiedExecOutput, clamp,
    generate_chunk_id, truncate_output,
};
use crate::tool::{Tool, arg_str, arg_u64};
use crate::tools::exec_command::{render_unified_exec_output, unified_exec_output_schema};
use crate::types::{AppConfig, ToolAuditMetadata, ToolContent, ToolResult};

pub struct WriteStdin;

#[async_trait]
impl Tool for WriteStdin {
    fn name(&self) -> &'static str {
        "write_stdin"
    }

    fn description(&self) -> String {
        "Writes characters to an existing exec_command session and returns recent output. Use this to answer a prompt from an interactive command, feed input to a REPL, or simply poll a still-running process for more output.\n\nPass the session_id returned by exec_command. Leave chars empty to poll without writing. Include a trailing newline in chars when the process is waiting for a line of input. Send a lone \\u0003 (Ctrl-C) to interrupt a runaway or blocking process. When the process exits, the response carries exit_code instead of session_id and the session is discarded.".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "number", "description": "Identifier of the running exec session, as returned by exec_command." },
                "chars": { "type": "string", "description": "Bytes to write to stdin. Defaults to empty, which polls without writing." },
                "yield_time_ms": { "type": "number", "description": format!("Wait before yielding output. Non-empty writes default to {STDIN_WRITE_DEFAULT_YIELD_MS} ms and cap at {EXEC_MAX_YIELD_MS} ms; empty polls wait {STDIN_POLL_DEFAULT_YIELD_MS}-{STDIN_POLL_MAX_YIELD_MS} ms by default.") },
                "max_output_tokens": { "type": "number", "description": format!("Output token budget. Defaults to {DEFAULT_MAX_OUTPUT_TOKENS} tokens; the middle of longer output is elided.") }
            },
            "required": ["session_id"],
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(unified_exec_output_schema())
    }

    fn uses_exec_session_state(&self) -> bool {
        true
    }

    fn may_modify_project(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, _config: &AppConfig, session: &SessionState) -> ToolResult {
        let Some(session_id) = arg_u64(&args, "session_id") else {
            return ToolResult::error("session_id must be a number");
        };

        let exec_session = session.exec_session(session_id);
        let Some(exec_session) = exec_session else {
            let live: Vec<String> = session
                .exec_session_ids()
                .into_iter()
                .map(|k| k.to_string())
                .collect();
            let suffix = if live.is_empty() {
                "There are no live sessions.".to_string()
            } else {
                format!("Live sessions: {}", live.join(", "))
            };
            return ToolResult::error(format!("No such exec session: {session_id}. {suffix}"));
        };

        let chars = arg_str(&args, "chars").unwrap_or("");
        let is_poll = chars.is_empty();
        let yield_ms = if is_poll {
            clamp(
                arg_u64(&args, "yield_time_ms").unwrap_or(STDIN_POLL_DEFAULT_YIELD_MS),
                STDIN_POLL_DEFAULT_YIELD_MS,
                STDIN_POLL_MAX_YIELD_MS,
            )
        } else {
            clamp(
                arg_u64(&args, "yield_time_ms").unwrap_or(STDIN_WRITE_DEFAULT_YIELD_MS),
                1,
                EXEC_MAX_YIELD_MS,
            )
        };
        let max_output_tokens = match arg_u64(&args, "max_output_tokens") {
            Some(n) if n > 0 => n,
            _ => DEFAULT_MAX_OUTPUT_TOKENS,
        };

        let started = std::time::Instant::now();

        // A lone ETX (Ctrl-C) is an interrupt request, matching Codex's unified
        // exec: rather than writing the raw 0x03 byte into the pipe, signal the
        // process so a runaway or blocking command can be stopped.
        let is_interrupt = chars == "\u{0003}";

        if !is_poll {
            if let Some(code) = exec_session.exit_code() {
                return ToolResult::error(format!(
                    "Session {session_id} has already exited with code {code}; cannot write to stdin."
                ));
            }
            if is_interrupt {
                exec_session.interrupt();
            } else if let Err(e) = exec_session.write_stdin(chars).await {
                return ToolResult::error(e.to_string());
            }
        }

        let (output, exited, buffer_truncated) =
            exec_session.yield_output_with_metadata(yield_ms).await;
        let (text, original_token_count, truncated) = truncate_output(&output, max_output_tokens);

        let mut result = UnifiedExecOutput {
            chunk_id: Some(generate_chunk_id()),
            wall_time_seconds: started.elapsed().as_secs_f64(),
            output: text,
            original_token_count: Some(original_token_count),
            ..Default::default()
        };

        let is_error = if exited {
            let code = exec_session.exit_code();
            result.exit_code = code;
            session.remove_exec_session(session_id);
            code.unwrap_or(0) != 0
        } else {
            result.session_id = Some(session_id);
            false
        };

        let structured = serde_json::to_value(&result).unwrap_or(Value::Null);
        ToolResult {
            content: vec![ToolContent::Text(render_unified_exec_output(&result))],
            is_error,
            structured_content: Some(structured),
            meta: None,
            audit: ToolAuditMetadata {
                truncated: Some(buffer_truncated || truncated),
                original_output_tokens: truncated.then_some(original_token_count),
                exec_session_id: Some(session_id),
                process_id: exec_session.pid,
                resident: Some(!exited),
            },
        }
    }
}
