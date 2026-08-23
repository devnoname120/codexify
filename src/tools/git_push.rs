use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::exec_sessions::SessionState;
use crate::process_env::scrub_untrusted_child_env;
use crate::tool::{Tool, arg_str};
use crate::types::{AppConfig, ToolResult};

pub struct GitPush;

#[async_trait]
impl Tool for GitPush {
    fn name(&self) -> &'static str {
        "git_push"
    }

    fn description(&self) -> String {
        "Push local commits to a remote git repository. Defaults to pushing the current branch to 'origin'. Use this after git_commit to publish changes. Returns the push output including any remote messages.".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "remote": { "type": "string", "description": "Remote name. Default: origin" },
                "branch": { "type": "string", "description": "Branch name. Default: current branch" }
            }
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "Git push output text" }
            }
        }))
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let remote = arg_str(&args, "remote").unwrap_or("origin");
        let mut cmd_args: Vec<&str> = vec!["push", remote];

        if let Some(branch) = arg_str(&args, "branch") {
            cmd_args.push(branch);
        }

        let mut command = Command::new("git");
        command.args(&cmd_args).current_dir(&config.work_dir);
        scrub_untrusted_child_env(&mut command, config);
        let output = command.output().await;

        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let stderr = String::from_utf8_lossy(&o.stderr);
                let exit_code = o.status.code().unwrap_or(-1);

                let output_text = format!("{stdout}\n{stderr}");
                let output_text = output_text.trim();

                if exit_code != 0 {
                    return ToolResult::error(format!(
                        "git push failed (exit {exit_code}):\n{output_text}"
                    ));
                }

                if output_text.is_empty() {
                    ToolResult::text("Push successful (no output).")
                } else {
                    ToolResult::text(output_text.to_string())
                }
            }
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}
