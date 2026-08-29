use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use std::future::Future;

use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::project_bindings::{ProjectBindingScope, ProjectRootSelection};
use crate::tool::{Tool, ToolBehavior, schema_for};
use crate::types::{AppConfig, ToolResult};

pub struct SetProjectRoot;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetProjectRootArgs {
    /// Existing project selector/path or supported Git repository URL.
    #[schemars(length(min = 1))]
    path: String,
}

impl SetProjectRoot {
    pub const NAME: &'static str = "set_project_root";
}

pub async fn select_and_render<F, Fut>(args: &Value, select: F) -> ToolResult
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<ProjectRootSelection, String>>,
{
    let SetProjectRootArgs { path } = match serde_json::from_value(args.clone()) {
        Ok(args) => args,
        Err(error) => return ToolResult::error(format!("Invalid tool arguments: {error}")),
    };

    let selection = match select(path).await {
        Ok(selection) => selection,
        Err(error) => return ToolResult::error(error),
    };

    let state = if selection.cloned {
        "Git repository cloned and project root selected"
    } else if selection.newly_selected {
        "Project root selected"
    } else {
        "Project root was already selected"
    };
    let persistence = match selection.scope {
        ProjectBindingScope::ChatGptConversation => {
            "This ChatGPT conversation is permanently bound to that source project and active checkout. The binding survives MCP reconnects and server restarts; start a new chat for another project."
        }
        ProjectBindingScope::McpTransportSession => {
            "This MCP transport session is permanently bound to that source project and active checkout. Clients that do not provide a stable conversation identifier must select again after reconnecting."
        }
    };
    let placement = if selection.managed_worktree {
        format!(
            "Active project root: {}\nSource project root: {}\nManaged detached worktree Git root: {}\nManaged-worktree location: {}",
            selection.project_root.display(),
            selection.source_project_root.display(),
            selection
                .worktree_git_root
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unknown>".to_string()),
            selection
                .worktrees_root
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unknown>".to_string())
        )
    } else {
        format!(
            "Active project root: {}\nSource project root: {}\nThe source checkout is used directly under worktree mode `{}`.",
            selection.project_root.display(),
            selection.source_project_root.display(),
            selection.worktree_mode.as_str()
        )
    };
    let warning_text = if selection.warnings.is_empty() {
        String::new()
    } else {
        format!("\nWarnings:\n- {}", selection.warnings.join("\n- "))
    };
    let repository_text = selection
        .repository_url
        .as_deref()
        .map(|url| format!("\nGit repository: {url}"))
        .unwrap_or_default();
    let content = format!(
        "{state}\n{placement}\nAccess root: {}{repository_text}\n{persistence}{warning_text}\nCall `get_agent_brief` now so the environment, saved state, skills, and project instructions are loaded from the active root.",
        selection.access_root.display()
    );

    ToolResult::text(content.clone()).with_structured(json!({
        "access_root": selection.access_root.to_string_lossy(),
        "source_project_root": selection.source_project_root.to_string_lossy(),
        "project_root": selection.project_root.to_string_lossy(),
        "repository_url": selection.repository_url,
        "cloned": selection.cloned,
        "managed_worktree": selection.managed_worktree,
        "worktree_git_root": selection.worktree_git_root.as_ref().map(|path| path.to_string_lossy()),
        "worktrees_root": selection.worktrees_root.as_ref().map(|path| path.to_string_lossy()),
        "worktree_mode": selection.worktree_mode.as_str(),
        "warnings": selection.warnings,
        "newly_selected": selection.newly_selected,
        "binding_scope": selection.scope.as_str(),
        "content": content
    }))
}

#[async_trait]
impl Tool for SetProjectRoot {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn title(&self) -> String {
        "Set project root".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            false,
            true,
            true,
            "Persists a project binding and may clone or fetch an external Git repository without overwriting an existing source checkout.",
        )
    }

    fn description(&self) -> String {
        "Bind the current ChatGPT conversation to one source project beneath the server's configured access root. The path may instead be an HTTPS or SSH Git repository URL ending in `.git`, including non-GitHub hosts such as GitLab, or a supported GitHub repository-root URL. GitHub HTTPS branch URLs ending in /tree/<branch>, pull-request URLs ending in /pull/<number>, and commit URLs ending in /commit/<sha> with a full 40-character hexadecimal commit ID also select exact targets. Codexify reuses an unambiguous matching local checkout, or runs non-interactive git clone in the configured project clone directory before binding. Targeted GitHub URLs fetch the exact requested target; when an existing source checkout is on another commit, Codexify leaves it unchanged and selects a detached managed worktree at the requested commit. Worktree mode `never` therefore requires that the source checkout already be at the requested target. Local/file, insecure, credential-bearing HTTPS, and other arbitrary Git transports are rejected. ChatGPT bindings survive MCP reconnects and server restarts and cannot be changed; start a new chat for another project. Clients without ChatGPT's stable conversation metadata fall back to binding the current MCP transport session. When only a project name or purpose is known, call list_projects first and pass one unambiguous result's selector as path. Do not guess among plausible projects. In multi-project mode, bind a new conversation before any filesystem, search, edit, command, git, project-instruction, skill, memory, or plan tool, then call get_agent_brief.".into()
    }

    fn describe(&self, config: &AppConfig) -> String {
        if config.multi_project {
            format!(
                "{} The access root is `{}` and Git clones are placed in `{}`. A filesystem path may be relative to the access root or absolute inside it.",
                self.description(),
                config.work_dir.display(),
                config.project_clone_dir.display()
            )
        } else {
            "Project-root selection is disabled on this server. Start codexify with --multi-project or set multiProject to true to enable it.".into()
        }
    }

    fn input_schema(&self) -> Value {
        schema_for::<SetProjectRootArgs>()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "access_root": { "type": "string" },
                "source_project_root": { "type": "string" },
                "project_root": { "type": "string" },
                "repository_url": { "type": ["string", "null"] },
                "cloned": { "type": "boolean" },
                "managed_worktree": { "type": "boolean" },
                "worktree_git_root": { "type": ["string", "null"] },
                "worktrees_root": { "type": ["string", "null"] },
                "worktree_mode": { "type": "string", "enum": ["auto", "always", "never"] },
                "warnings": { "type": "array", "items": { "type": "string" } },
                "newly_selected": { "type": "boolean" },
                "binding_scope": { "type": "string", "enum": ["chatgpt_conversation", "mcp_transport_session"] },
                "content": { "type": "string" }
            },
            "required": ["access_root", "source_project_root", "project_root", "repository_url", "cloned", "managed_worktree", "worktree_git_root", "worktrees_root", "worktree_mode", "warnings", "newly_selected", "binding_scope", "content"],
            "additionalProperties": false
        }))
    }

    fn fills_structured_content(&self) -> bool {
        false
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, config: &AppConfig, session: &SessionState) -> ToolResult {
        select_and_render(&args, |path| async move {
            session.select_project_root(config, &path).await
        })
        .await
    }
}
