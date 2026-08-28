use async_trait::async_trait;
use std::future::Future;

use rmcp::model::ToolAnnotations;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::project_bindings::{ProjectBindingScope, ProjectRootSelection};
use crate::tool::{Tool, arg_str};
use crate::types::{AppConfig, ToolResult};

pub struct SetProjectRoot;

impl SetProjectRoot {
    pub const NAME: &'static str = "set_project_root";
}

pub async fn select_and_render<F, Fut>(args: &Value, select: F) -> ToolResult
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<ProjectRootSelection, String>>,
{
    let Some(path) = arg_str(args, "path") else {
        return ToolResult::error("path must be a string");
    };

    let selection = match select(path.to_string()).await {
        Ok(selection) => selection,
        Err(error) => return ToolResult::error(error),
    };

    let state = if selection.cloned {
        "GitHub repository cloned and project root selected"
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
        .map(|url| format!("\nGitHub repository: {url}"))
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

    fn title(&self) -> Option<String> {
        Some("Set project root".to_string())
    }

    fn annotations(&self) -> Option<ToolAnnotations> {
        Some(
            ToolAnnotations::new()
                .read_only(false)
                .destructive(false)
                .idempotent(true)
                .open_world(true),
        )
    }

    fn description(&self) -> String {
        "Bind the current ChatGPT conversation to one source project beneath the server's configured access root. The path may instead be an HTTPS or SSH GitHub repository-root URL, an HTTPS branch URL ending in /tree/<branch>, an HTTPS pull-request URL ending in /pull/<number>, or an HTTPS commit URL ending in /commit/<sha> with a full 40-character hexadecimal commit ID. Codexify reuses an unambiguous matching local checkout, or runs non-interactive git clone in the configured project clone directory before binding. Branch, pull-request, and commit URLs fetch the exact requested target; when an existing source checkout is on another commit, Codexify leaves it unchanged and selects a detached managed worktree at the requested commit. Worktree mode `never` therefore requires that the source checkout already be at the requested target. ChatGPT bindings survive MCP reconnects and server restarts and cannot be changed; start a new chat for another project. Clients without ChatGPT's stable conversation metadata fall back to binding the current MCP transport session. When only a project name or purpose is known, call list_projects first and pass one unambiguous result's selector as path. Do not guess among plausible projects. In multi-project mode, bind a new conversation before any filesystem, search, edit, command, git, project-instruction, skill, memory, or plan tool, then call get_agent_brief.".into()
    }

    fn describe(&self, config: &AppConfig) -> String {
        if config.multi_project {
            format!(
                "{} The access root is `{}` and GitHub clones are placed in `{}`. A filesystem path may be relative to the access root or absolute inside it.",
                self.description(),
                config.work_dir.display(),
                config.project_clone_dir.display()
            )
        } else {
            "Project-root selection is disabled on this server. Start codexify with --multi-project or set multiProject to true to enable it.".into()
        }
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Existing project directory (relative to the configured access root or absolute inside it), selector returned by list_projects, GitHub repository-root URL, HTTPS GitHub branch URL (/tree/<branch>), HTTPS GitHub pull-request URL (/pull/<number>), or HTTPS GitHub commit URL (/commit/<sha>, full 40-character hexadecimal commit ID) to reuse or clone and select"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
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
            "required": ["access_root", "source_project_root", "project_root", "repository_url", "cloned", "managed_worktree", "worktree_git_root", "worktrees_root", "worktree_mode", "warnings", "newly_selected", "binding_scope", "content"]
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
