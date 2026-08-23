use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::exec_sessions::SessionState;
use crate::process_env::scrub_untrusted_child_env;
use crate::tool::{Tool, arg_bool, arg_str};
use crate::types::{AppConfig, ToolResult};

pub struct GitCommit;

#[async_trait]
impl Tool for GitCommit {
    fn name(&self) -> &'static str {
        "git_commit"
    }

    fn description(&self) -> String {
        "Create a git commit with the given message. Set all=true to automatically stage all tracked modified files before committing (equivalent to git commit -a). Without all=true, only previously staged files (via git add) will be committed. Use git_status first to see what will be committed.".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "Commit message" },
                "all": { "type": "boolean", "description": "Stage all tracked changes before committing (git commit -a). Default: false" }
            },
            "required": ["message"]
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "Git commit output text including branch, hash, and summary" }
            }
        }))
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let message = arg_str(&args, "message").unwrap_or("");
        let mut commit_args: Vec<&str> = vec!["commit"];

        if arg_bool(&args, "all") {
            commit_args.push("-a");
        }

        commit_args.push("-m");
        commit_args.push(message);

        let mut command = Command::new("git");
        command.args(&commit_args).current_dir(&config.work_dir);
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
                        "git commit failed (exit {exit_code}):\n{output_text}"
                    ));
                }

                ToolResult::text(output_text.to_string())
            }
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}
