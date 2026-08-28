use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;

use crate::exec_sessions::SessionState;
use crate::process_env::scrub_untrusted_child_env;
use crate::tool::{Tool, ToolBehavior, parse_tool_args, schema_for, text_output_schema};
use crate::types::{AppConfig, ToolResult};

pub struct GitPush;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GitPushArgs {
    /// Configured remote name. Defaults to origin.
    #[schemars(length(min = 1))]
    remote: Option<String>,
    /// Existing local branch to push to the same branch name remotely. Defaults to the current branch.
    #[schemars(length(min = 1))]
    branch: Option<String>,
}

async fn git_output(config: &AppConfig, args: &[&str]) -> Result<std::process::Output, String> {
    let mut command = Command::new("git");
    command.args(args).current_dir(&config.work_dir);
    scrub_untrusted_child_env(&mut command, config);
    command.output().await.map_err(|error| error.to_string())
}

async fn current_branch(config: &AppConfig) -> Result<String, String> {
    let output = git_output(config, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    if !output.status.success() {
        return Err("git_push requires a named current branch; HEAD is detached".to_string());
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        Err("git_push could not determine the current branch".to_string())
    } else {
        Ok(branch)
    }
}

async fn validate_remote(config: &AppConfig, remote: &str) -> Result<(), String> {
    if remote.starts_with('-') {
        return Err("remote must be a configured remote name, not a Git option".to_string());
    }
    let output = git_output(config, &["remote"]).await?;
    if !output.status.success() {
        return Err(format!(
            "git remote failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let remotes = String::from_utf8_lossy(&output.stdout);
    if remotes.lines().any(|candidate| candidate == remote) {
        Ok(())
    } else {
        Err(format!("unknown Git remote {remote:?}"))
    }
}

async fn validate_branch(config: &AppConfig, branch: &str) -> Result<(), String> {
    if branch.starts_with('-') || branch.contains(':') || branch.starts_with('+') {
        return Err("branch must be a local branch name, not an option or refspec".to_string());
    }
    let format = git_output(config, &["check-ref-format", "--branch", branch]).await?;
    if !format.status.success() {
        return Err(format!("invalid Git branch name {branch:?}"));
    }
    let local_ref = format!("refs/heads/{branch}");
    let exists = git_output(config, &["show-ref", "--verify", "--quiet", &local_ref]).await?;
    if exists.status.success() {
        Ok(())
    } else {
        Err(format!("local branch {branch:?} does not exist"))
    }
}

#[async_trait]
impl Tool for GitPush {
    fn name(&self) -> &'static str {
        "git_push"
    }

    fn title(&self) -> String {
        "Push Git branch".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            true,
            false,
            true,
            "Publishes commits to an external Git remote and preserves push hooks, which may have additional side effects.",
        )
    }

    fn description(&self) -> String {
        "Push one existing local branch to the same branch name on a configured Git remote. Defaults to the current branch and the 'origin' remote. This tool deliberately does not accept arbitrary refspecs, force options, or deletion syntax; use exec_command for Git operations outside this narrow contract. Normal Git pre-push hooks are preserved.".into()
    }

    fn input_schema(&self) -> Value {
        schema_for::<GitPushArgs>()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let GitPushArgs { remote, branch } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        let remote = remote.as_deref().unwrap_or("origin");
        if let Err(error) = validate_remote(config, remote).await {
            return ToolResult::error(error);
        }
        let branch = match branch {
            Some(branch) => branch,
            None => match current_branch(config).await {
                Ok(branch) => branch,
                Err(error) => return ToolResult::error(error),
            },
        };
        if let Err(error) = validate_branch(config, &branch).await {
            return ToolResult::error(error);
        }

        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        let output = git_output(config, &["push", remote, &refspec]).await;

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

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command as StdCommand;

    use serde_json::json;

    use super::*;

    fn git(dir: &Path, args: &[&str]) -> std::process::Output {
        StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git must be installed")
    }

    fn initialized_repository() -> (tempfile::TempDir, tempfile::TempDir) {
        let repo = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        assert!(git(repo.path(), &["init", "--quiet"]).status.success());
        assert!(
            git(repo.path(), &["config", "user.name", "Test"])
                .status
                .success()
        );
        assert!(
            git(repo.path(), &["config", "user.email", "test@example.com"])
                .status
                .success()
        );
        assert!(
            git(remote.path(), &["init", "--bare", "--quiet"])
                .status
                .success()
        );
        std::fs::write(repo.path().join("file.txt"), "seed\n").unwrap();
        assert!(git(repo.path(), &["add", "file.txt"]).status.success());
        assert!(
            git(repo.path(), &["commit", "--quiet", "-m", "seed"])
                .status
                .success()
        );
        assert!(
            git(
                repo.path(),
                &["remote", "add", "origin", remote.path().to_str().unwrap()]
            )
            .status
            .success()
        );
        (repo, remote)
    }

    #[tokio::test]
    async fn rejects_deletion_refspecs_without_touching_the_remote() {
        let (repo, remote) = initialized_repository();
        assert!(git(repo.path(), &["branch", "victim"]).status.success());
        assert!(
            git(
                repo.path(),
                &["push", "origin", "refs/heads/victim:refs/heads/victim"]
            )
            .status
            .success()
        );
        let config = crate::config::default_config(repo.path().to_path_buf());
        let result = GitPush
            .call(
                json!({ "remote": "origin", "branch": ":refs/heads/victim" }),
                &config,
                &SessionState::new(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.joined_text().contains("not an option or refspec"));
        assert!(
            git(
                remote.path(),
                &["show-ref", "--verify", "--quiet", "refs/heads/victim"]
            )
            .status
            .success()
        );
    }

    #[tokio::test]
    async fn rejects_force_options_and_destination_refspecs() {
        let (repo, _remote) = initialized_repository();
        assert!(git(repo.path(), &["branch", "victim"]).status.success());
        let config = crate::config::default_config(repo.path().to_path_buf());

        for branch in ["--force", "+victim", "victim:other"] {
            let result = GitPush
                .call(
                    json!({ "remote": "origin", "branch": branch }),
                    &config,
                    &SessionState::new(),
                )
                .await;
            assert!(result.is_error, "{branch}: {}", result.joined_text());
            assert!(
                result.joined_text().contains("not an option or refspec"),
                "{branch}: {}",
                result.joined_text()
            );
        }
    }

    #[tokio::test]
    async fn pushes_only_the_named_local_branch_to_the_same_remote_name() {
        let (repo, remote) = initialized_repository();
        assert!(
            git(repo.path(), &["switch", "-c", "feature"])
                .status
                .success()
        );
        let config = crate::config::default_config(repo.path().to_path_buf());
        let result = GitPush
            .call(
                json!({ "remote": "origin", "branch": "feature" }),
                &config,
                &SessionState::new(),
            )
            .await;
        assert!(!result.is_error, "{}", result.joined_text());
        assert!(
            git(
                remote.path(),
                &["show-ref", "--verify", "--quiet", "refs/heads/feature"]
            )
            .status
            .success()
        );
    }
}
