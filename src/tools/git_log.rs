use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;

use crate::exec_sessions::SessionState;
use crate::process_env::scrub_untrusted_child_env;
use crate::tool::{Tool, ToolBehavior, parse_tool_args, schema_for, text_output_schema};
use crate::types::{AppConfig, ToolResult};

pub struct GitLog;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GitLogArgs {
    /// Number of commits to show. Defaults to 10.
    #[schemars(range(min = 1))]
    count: Option<u64>,
    /// Use compact one-line formatting.
    #[serde(default)]
    oneline: bool,
}

#[async_trait]
impl Tool for GitLog {
    fn name(&self) -> &'static str {
        "git_log"
    }

    fn title(&self) -> String {
        "Read Git log".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Reads local Git history without changing repository or external state.",
        )
    }

    fn description(&self) -> String {
        "Show recent git commit history of the project. Returns commit hash, author, date, and message for each commit. Use oneline=true for a compact view. Useful to understand recent changes, find when a bug was introduced, or review what has been done.".into()
    }

    fn input_schema(&self) -> Value {
        schema_for::<GitLogArgs>()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let GitLogArgs { count, oneline } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        let count = count.unwrap_or(10);
        let max_count = format!("--max-count={count}");
        let mut log_args: Vec<&str> = vec!["log", &max_count];

        if oneline {
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
