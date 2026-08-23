use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::exec_sessions::SessionState;
use crate::process_env::scrub_untrusted_child_env;
use crate::tool::{Tool, arg_bool, arg_u64};
use crate::types::{AppConfig, ToolResult};

pub struct GitLog;

#[async_trait]
impl Tool for GitLog {
    fn name(&self) -> &'static str {
        "git_log"
    }

    fn description(&self) -> String {
        "Show recent git commit history of the project. Returns commit hash, author, date, and message for each commit. Use oneline=true for a compact view. Useful to understand recent changes, find when a bug was introduced, or review what has been done.".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "count": { "type": "number", "description": "Number of commits to show. Default: 10" },
                "oneline": { "type": "boolean", "description": "Show compact one-line format. Default: false" }
            }
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "Formatted git log output with commit hash, author, date, and message" }
            }
        }))
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let count = arg_u64(&args, "count").unwrap_or(10);
        let max_count = format!("--max-count={count}");
        let mut log_args: Vec<&str> = vec!["log", &max_count];

        if arg_bool(&args, "oneline") {
            log_args.push("--oneline");
        } else {
            log_args.push("--format=%H %an <%ae> %ai%n  %s%n");
        }

        let mut command = Command::new("git");
        command.args(&log_args).current_dir(&config.work_dir);
        scrub_untrusted_child_env(&mut command, config);
        let output = command.output().await;

        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let stderr = String::from_utf8_lossy(&o.stderr);
                let exit_code = o.status.code().unwrap_or(-1);

                if exit_code != 0 {
                    return ToolResult::error(format!("git log failed: {stderr}"));
                }

                let trimmed = stdout.trim();
                if trimmed.is_empty() {
                    return ToolResult::text("No commits found.");
                }

                ToolResult::text(trimmed.to_string())
            }
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}
