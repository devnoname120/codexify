use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;

use crate::exec_sessions::SessionState;
use crate::process_env::scrub_untrusted_child_env;
use crate::tool::{Tool, ToolBehavior, parse_tool_args, schema_for, text_output_schema};
use crate::types::{AppConfig, ToolResult};

pub struct GitCommit;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GitCommitArgs {
    /// Commit message.
    #[schemars(length(min = 1))]
    message: String,
    /// Stage all tracked changes before committing, equivalent to git commit -a.
    #[serde(default)]
    all: bool,
}

#[async_trait]
impl Tool for GitCommit {
    fn name(&self) -> &'static str {
        "git_commit"
    }

    fn title(&self) -> String {
        "Create Git commit".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            true,
            false,
            true,
            "Creates local Git history while preserving repository hooks, which may modify local state or contact external systems.",
        )
    }

    fn description(&self) -> String {
        "Create a git commit with the given message. Set all=true to automatically stage all tracked modified files before committing (equivalent to git commit -a). Without all=true, only previously staged files (via git add) will be committed. Use git_status first to see what will be committed.".into()
    }

    fn input_schema(&self) -> Value {
        schema_for::<GitCommitArgs>()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    fn may_modify_project(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let GitCommitArgs { message, all } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        let mut commit_args: Vec<&str> = vec!["commit"];

        if all {
            commit_args.push("-a");
        }

        commit_args.push("-m");
        commit_args.push(&message);

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
