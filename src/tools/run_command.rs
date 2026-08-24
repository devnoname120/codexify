use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::exec_sessions::SessionState;
use crate::process_env::scrub_untrusted_child_env;
use crate::tool::{Tool, arg_str, arg_u64};
use crate::types::{AppConfig, ToolResult};

pub struct RunCommand;

#[async_trait]
impl Tool for RunCommand {
    fn name(&self) -> &'static str {
        "run_command"
    }

    fn description(&self) -> String {
        "Execute a shell command in the project directory. Only allowlisted commands are permitted (e.g. bun, npm, node, git, python, cargo, make). Returns stdout, stderr, and exit code. Use this to run tests, install dependencies, build projects, or execute scripts. Times out after 30s by default.".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command/binary to run" },
                "args": { "type": "array", "items": { "type": "string" }, "description": "Command arguments" },
                "timeout": { "type": "number", "description": "Timeout in milliseconds. Default: 30000" }
            },
            "required": ["command"]
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "Combined stdout/stderr output followed by exit code" }
            }
        }))
    }

    fn may_modify_project(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let Some(command) = arg_str(&args, "command") else {
            return ToolResult::error("command must be a string");
        };

        let cmd_args: Vec<String> = args
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let timeout = arg_u64(&args, "timeout")
            .unwrap_or(config.command.default_timeout)
            .min(config.command.max_timeout);

        if !config.allowed_commands.iter().any(|c| c == command) {
            return ToolResult::error(format!(
                "Command not allowed: \"{}\". Allowed: {}",
                command,
                config.allowed_commands.join(", ")
            ));
        }

        let mut cmd = Command::new(command);
        cmd.args(&cmd_args)
            .current_dir(&config.work_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        scrub_untrusted_child_env(&mut cmd, config);

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolResult::error(e.to_string()),
        };

        let output =
            match tokio::time::timeout(Duration::from_millis(timeout), child.wait_with_output())
                .await
            {
                Err(_) => {
                    return ToolResult::error(format!(
                        "Command timed out after {}s",
                        timeout as f64 / 1000.0
                    ));
                }
                Ok(Err(e)) => return ToolResult::error(e.to_string()),
                Ok(Ok(o)) => o,
            };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let exit_code = output.status.code().unwrap_or(-1);

        let mut out = String::new();
        if !stdout.is_empty() {
            out.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !out.is_empty() {
                out.push_str("\n--- stderr ---\n");
            }
            out.push_str(&stderr);
        }
        if out.is_empty() {
            out.push_str("(no output)");
        }
        out.push_str(&format!("\n\nexit code: {exit_code}"));

        ToolResult {
            content: vec![crate::types::ToolContent::Text(out)],
            is_error: exit_code != 0,
            structured_content: None,
        }
    }
}
