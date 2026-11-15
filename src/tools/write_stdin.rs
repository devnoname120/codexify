use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::diff::DiffOwner;
use crate::exec_sessions::{
    DEFAULT_MAX_OUTPUT_TOKENS, EXEC_MAX_YIELD_MS, STDIN_POLL_DEFAULT_YIELD_MS,
    STDIN_POLL_MAX_YIELD_MS, STDIN_WRITE_DEFAULT_YIELD_MS, SessionState, UnifiedExecOutput, clamp,
    generate_chunk_id, truncate_output,
};
use crate::output_budget::resolve_requested_output_tokens;
use crate::tool::{Tool, ToolBehavior, ToolRequestContext, parse_tool_args};
use crate::tools::exec_command::{
    command_chat, render_unified_exec_output, unified_exec_output_schema, wait_for_command_output,
};
use crate::types::{AppConfig, ToolAuditMetadata, ToolContent, ToolResult};

pub struct WriteStdin;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteStdinArgs {
    session_id: u64,
    #[serde(default)]
    chars: String,
    yield_time_ms: Option<u64>,
    max_output_tokens: Option<u64>,
}

#[async_trait]
impl Tool for WriteStdin {
    fn name(&self) -> &'static str {
        "write_stdin"
    }

    fn title(&self) -> String {
        "Write to command session".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            true,
            false,
            true,
            "Input can trigger, confirm, or interrupt arbitrary local or external actions in a resident shell process.",
        )
    }

    fn description(&self) -> String {
        "Writes characters to an existing exec_command session and returns recent output. Use this to answer a prompt from an interactive command, feed input to a REPL, or simply poll a still-running process for more output.\n\nPass the session_id returned by exec_command. Leave chars empty to poll without writing. Include a trailing newline in chars when the process is waiting for a line of input. Send a lone \\u0003 (Ctrl-C) to interrupt a runaway or blocking process. When the process exits, the response carries exit_code instead of session_id and the session is discarded.".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "integer", "minimum": 1, "description": "Identifier of the running exec session, as returned by exec_command." },
                "chars": { "type": "string", "description": "Bytes to write to stdin. Defaults to empty, which polls without writing." },
                "yield_time_ms": { "type": "integer", "minimum": 0, "description": format!("Wait before yielding output. Non-empty writes default to {STDIN_WRITE_DEFAULT_YIELD_MS} ms and are clamped to 1-{EXEC_MAX_YIELD_MS} ms; empty polls default to {STDIN_POLL_DEFAULT_YIELD_MS} ms and are clamped to {STDIN_POLL_DEFAULT_YIELD_MS}-{STDIN_POLL_MAX_YIELD_MS} ms.") },
                "max_output_tokens": { "type": "integer", "minimum": 0, "description": format!("Output token budget. Zero or omission uses the {DEFAULT_MAX_OUTPUT_TOKENS}-token default; larger requests are capped by the server output policy and the middle of longer output is elided.") }
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

    fn manages_model_output_budget(&self) -> bool {
        true
    }

    fn may_modify_project(&self) -> bool {
        true
    }

    fn manages_project_checkpoint(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, config: &AppConfig, session: &SessionState) -> ToolResult {
        self.run(args, config, session, None).await
    }

    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        self.run(args, config, session, Some(context)).await
    }
}

impl WriteStdin {
    async fn run(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: Option<&ToolRequestContext>,
    ) -> ToolResult {
        let WriteStdinArgs {
            session_id,
            chars,
            yield_time_ms,
            max_output_tokens,
        } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
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

        let is_poll = chars.is_empty();
        let yield_ms = if is_poll {
            clamp(
                yield_time_ms.unwrap_or(STDIN_POLL_DEFAULT_YIELD_MS),
                STDIN_POLL_DEFAULT_YIELD_MS,
                STDIN_POLL_MAX_YIELD_MS,
            )
        } else {
            clamp(
                yield_time_ms.unwrap_or(STDIN_WRITE_DEFAULT_YIELD_MS),
                1,
                EXEC_MAX_YIELD_MS,
            )
        };
        let max_output_tokens = resolve_requested_output_tokens(config, max_output_tokens);

        let started = std::time::Instant::now();
        let cancellation = context.map(|c| c.cancellation.clone()).unwrap_or_default();

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
            let _guard = if let Some(context) = context {
                let owner = context
                    .task_conversation
                    .as_ref()
                    .or(context.conversation.as_ref())
                    .map(DiffOwner::conversation)
                    .unwrap_or_else(|| DiffOwner::transport(session.diff_state()));
                let result = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return ToolResult::error("Call cancelled before input was sent."),
                    result = crate::audit::stage("checkpoint_wait", context.diff_checkpoints.begin_mutation(config, owner)) => result,
                };
                match result {
                    Ok((_, guard)) => Some(guard),
                    Err(error) => {
                        return ToolResult::error(format!(
                            "Refusing to send input because the diff checkpoint could not be captured: {error}"
                        ));
                    }
                }
            } else {
                None
            };
            if cancellation.is_cancelled() {
                return ToolResult::error("Call cancelled before input was sent.");
            }
            if is_interrupt {
                exec_session.interrupt();
            } else {
                let result = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return ToolResult::error("Input operation cancelled; some bytes may have been sent."),
                    result = crate::audit::stage("command_input", tokio::time::timeout(std::time::Duration::from_millis(config.diff.command_timeout_ms), exec_session.write_stdin(&chars))) =>
                        result.unwrap_or_else(|_| Err(std::io::Error::new(std::io::ErrorKind::TimedOut,"Input write timed out; some bytes may have been sent."))),
                };
                if let Err(error) = result {
                    return ToolResult::error(error.to_string());
                }
            }
            // Checkpoint the input action, not the following wait for output.
        }

        let chat = command_chat(config, session, context);
        let (output, exited, buffer_truncated) =
            match wait_for_command_output(&exec_session, yield_ms, chat.as_ref(), &cancellation)
                .await
            {
                Ok(output) => output,
                Err(error) => return ToolResult::error(error),
            };
        let (text, original_token_count, truncated) = truncate_output(&output, max_output_tokens);

        let mut result = UnifiedExecOutput {
            chunk_id: Some(generate_chunk_id()),
            wall_time_seconds: started.elapsed().as_secs_f64(),
            output: text,
            original_token_count: Some(original_token_count),
            ..Default::default()
        };

        let is_error = if exited {
            let code = exec_session.exit_code().unwrap_or(-1);
            result.exit_code = Some(code);
            session.remove_exec_session(session_id);
            code != 0
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
            new_chat_message_from_user: None,
            chat_delivery_end: None,
            audit: ToolAuditMetadata {
                truncated: Some(buffer_truncated || truncated),
                original_output_tokens: truncated.then_some(original_token_count),
                exec_session_id: Some(session_id),
                process_id: exec_session.pid,
                resident: Some(!exited),
                sensitive_output: false,
            },
        }
    }
}
