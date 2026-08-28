use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

use crate::exec_sessions::SessionState;
use crate::process_env::scrub_untrusted_child_env;
use crate::tool::{Tool, ToolBehavior, empty_object_schema, text_output_schema};
use crate::types::{AppConfig, ToolResult};

pub struct GitStatus;

#[async_trait]
impl Tool for GitStatus {
    fn name(&self) -> &'static str {
        "git_status"
    }

    fn title(&self) -> String {
        "Get Git status".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Reads the local Git working-tree status without changing repository state.",
        )
    }

    fn description(&self) -> String {
        "Show the current git status of the project. Returns a list of modified, added, deleted, and untracked files with their status codes (M=modified, A=added, D=deleted, ??=untracked). Use this before committing to see what has changed, or to understand the current state of the working tree.".into()
    }

    fn input_schema(&self) -> Value {
        empty_object_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    async fn call(&self, _args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let mut command = Command::new("git");
        command
            .args(["status", "--porcelain"])
            .current_dir(&config.work_dir);
        scrub_untrusted_child_env(&mut command, config);
        let output = command.output().await;

        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let stderr = String::from_utf8_lossy(&o.stderr);
                let exit_code = o.status.code().unwrap_or(-1);

                if exit_code != 0 {
                    return ToolResult::error(format!("git status failed: {stderr}"));
                }

                let trimmed = stdout.trim();
                if trimmed.is_empty() {
                    return ToolResult::text("Working tree clean \u{2014} no changes.");
                }

                let line_count = trimmed.split('\n').count();
                let header = format!("{line_count} changed file(s):\n\n");
                ToolResult::text(format!("{header}{trimmed}"))
            }
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}
