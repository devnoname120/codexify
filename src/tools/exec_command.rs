use async_trait::async_trait;
use serde_json::{Value, json};

use crate::exec_policy::{assert_exec_allowed, effective_allowlist};
use crate::exec_sessions::{
    DEFAULT_MAX_OUTPUT_TOKENS, EXEC_DEFAULT_YIELD_MS, EXEC_MAX_YIELD_MS, EXEC_MIN_YIELD_MS,
    SessionState, ShellType, UnifiedExecOutput, clamp, generate_chunk_id, resolve_shell,
    shell_type_of, start_exec_session, truncate_output,
};
use crate::safe_path::resolve_safe_path;
use crate::tool::{Tool, arg_str, arg_u64};
use crate::types::{AppConfig, ExecMode, ToolResult};

/// The output schema shared by `exec_command` and `write_stdin`.
pub fn unified_exec_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "chunk_id": { "type": "string", "description": "Chunk identifier included when the response reports one." },
            "wall_time_seconds": { "type": "number", "description": "Elapsed wall time spent waiting for output in seconds." },
            "exit_code": { "type": "number", "description": "Process exit code when the command finished during this call." },
            "session_id": { "type": "number", "description": "Session identifier to pass to write_stdin when the process is still running." },
            "original_token_count": { "type": "number", "description": "Approximate token count before output truncation." },
            "output": { "type": "string", "description": "Command output text, possibly truncated." }
        },
        "required": ["wall_time_seconds", "output"],
        "additionalProperties": false
    })
}

pub fn render_unified_exec_output(result: &UnifiedExecOutput) -> String {
    serde_json::to_string_pretty(result).unwrap_or_else(|_| "{}".to_string())
}

const BASE_DESCRIPTION: &str = "Runs a command in a shell and returns its output. Use this for anything the structured tools do not cover: build pipelines, test runners, package managers, interactive REPLs.\n\nIf the command finishes within yield_time_ms the full result is returned with its exit_code. If it is still running, a session_id comes back instead — pass that to write_stdin to send input, poll for more output, or wait for it to finish. That makes long-running and interactive processes (dev servers, REPLs, prompts asking for confirmation) workable in a single session.\n\nCommands run through the platform shell, so pipes, redirection and && chains work. Note this bridge runs commands with plain pipes, not a PTY.";

const WINDOWS_SHELL_GUIDANCE: &str = "Windows safety rules:\n- Do not compose destructive filesystem commands across shells. Do not enumerate paths in PowerShell and then pass them to `cmd /c`, batch builtins, or another shell for deletion or moving. Use one shell end-to-end, prefer native PowerShell cmdlets such as `Remove-Item` / `Move-Item` with `-LiteralPath`, and avoid string-built shell commands for file operations.\n- Before any recursive delete or move on Windows, verify the resolved absolute target paths stay within the intended workspace or explicitly named target directory. Never issue a recursive delete or move against a computed path if the final target has not been checked.\n- When using `Start-Process` to launch a background helper or service, pass `-WindowStyle Hidden` unless the user explicitly asked for a visible interactive window. Use visible windows only for interactive tools the user needs to see or control.";

pub fn exec_command_description() -> String {
    if cfg!(windows) {
        format!("{BASE_DESCRIPTION}\n\n{WINDOWS_SHELL_GUIDANCE}")
    } else {
        BASE_DESCRIPTION.to_string()
    }
}

fn syntax_hint(shell: ShellType) -> &'static str {
    match shell {
        ShellType::PowerShell => {
            "Commands are PowerShell, so `ls`, `cat` and `rm` are cmdlet aliases with different flags — prefer `Get-ChildItem`, `Get-Content`, `Remove-Item`, and `$env:FOO='bar'` for environment variables."
        }
        ShellType::Cmd => {
            "Commands are cmd.exe, so use `dir`, `%VAR%` and `copy` rather than POSIX equivalents."
        }
        ShellType::Posix => "Commands are POSIX shell, so the usual utilities and syntax apply.",
    }
}

/// Names the shell that will actually run, saving the model a `get_environment`
/// call and a wrong first guess.
pub fn describe_exec_command(config: &AppConfig) -> String {
    let parts = resolve_shell(config.exec.default_shell.as_deref());
    let bin = &parts[0];
    let shell = shell_type_of(bin);
    format!(
        "{}\n\nThis server runs commands with: {} ({}). {}",
        exec_command_description(),
        bin,
        shell.as_str(),
        syntax_hint(shell)
    )
}

pub struct ExecCommand;

#[async_trait]
impl Tool for ExecCommand {
    fn name(&self) -> &'static str {
        "exec_command"
    }

    fn description(&self) -> String {
        exec_command_description()
    }

    fn describe(&self, config: &AppConfig) -> String {
        describe_exec_command(config)
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string", "description": "Shell command to execute." },
                "workdir": { "type": "string", "description": "Working directory for the command, relative to the project root. Defaults to the project root." },
                "tty": { "type": "boolean", "description": "Unsupported by this bridge — commands always run with plain pipes. Passing true returns an error." },
                "yield_time_ms": { "type": "number", "description": format!("Wait before yielding output. Defaults to {EXEC_DEFAULT_YIELD_MS} ms; effective range is {EXEC_MIN_YIELD_MS}-{EXEC_MAX_YIELD_MS} ms.") },
                "max_output_tokens": { "type": "number", "description": format!("Output token budget. Defaults to {DEFAULT_MAX_OUTPUT_TOKENS} tokens; the middle of longer output is elided.") },
                "shell": { "type": "string", "description": "Shell binary to launch. Defaults to the platform shell." }
            },
            "required": ["cmd"],
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

    async fn call(&self, args: Value, config: &AppConfig, session: &SessionState) -> ToolResult {
        let cmd = arg_str(&args, "cmd").unwrap_or("");
        if cmd.trim().is_empty() {
            return ToolResult::error("cmd must be a non-empty string");
        }
        if args.get("tty").and_then(|v| v.as_bool()) == Some(true) {
            return ToolResult::error(
                "tty is not supported by this bridge; commands run with plain pipes. Omit tty or pass false.",
            );
        }

        let yield_ms = clamp(
            arg_u64(&args, "yield_time_ms").unwrap_or(EXEC_DEFAULT_YIELD_MS),
            EXEC_MIN_YIELD_MS,
            EXEC_MAX_YIELD_MS,
        );
        let max_output_tokens = match arg_u64(&args, "max_output_tokens") {
            Some(n) if n > 0 => n,
            _ => DEFAULT_MAX_OUTPUT_TOKENS,
        };

        let cwd = match resolve_safe_path(
            arg_str(&args, "workdir").unwrap_or(""),
            &config.work_dir,
            true,
        ) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };

        if let Err(e) = assert_exec_allowed(cmd, config) {
            return ToolResult::error(e.to_string());
        }

        let started = std::time::Instant::now();
        let exec_session =
            match start_exec_session(session, config, cmd, &cwd, arg_str(&args, "shell")) {
                Ok(s) => s,
                Err(e) => {
                    let hint = if config.exec.mode == ExecMode::Allowlist {
                        format!(
                            "\nAllowed commands: {}",
                            effective_allowlist(config).join(", ")
                        )
                    } else {
                        String::new()
                    };
                    return ToolResult::error(format!("{e}{hint}"));
                }
            };

        let (output, exited) = exec_session.yield_output(yield_ms).await;
        let (text, original_token_count) = truncate_output(&output, max_output_tokens);

        let mut result = UnifiedExecOutput {
            chunk_id: Some(generate_chunk_id()),
            wall_time_seconds: started.elapsed().as_secs_f64(),
            output: text,
            original_token_count,
            ..Default::default()
        };

        let is_error = if exited {
            let code = exec_session.exit_code();
            result.exit_code = code;
            session.remove_exec_session(exec_session.id);
            code.unwrap_or(0) != 0
        } else {
            result.session_id = Some(exec_session.id);
            false
        };

        let structured = serde_json::to_value(&result).unwrap_or(Value::Null);
        ToolResult {
            content: vec![crate::types::ToolContent::Text(render_unified_exec_output(
                &result,
            ))],
            is_error,
            structured_content: Some(structured),
        }
    }
}
