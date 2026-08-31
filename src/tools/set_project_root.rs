use async_trait::async_trait;
use serde::Deserialize;
use std::future::Future;

use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::project_bindings::{ProjectBindingScope, ProjectRootSelection, WithoutProjectSelection};
use crate::setup_ui;
use crate::tool::{Tool, ToolBehavior};
use crate::types::{AppConfig, ToolResult};

pub struct SetProjectRoot;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetProjectRootArgs {
    path: Option<String>,
    #[serde(rename = "withoutProject")]
    without_project: Option<bool>,
}

pub enum ProjectSelectionRequest {
    Project(String),
    WithoutProject,
}

pub enum ProjectSelection {
    Project(ProjectRootSelection),
    WithoutProject(WithoutProjectSelection),
}

impl SetProjectRoot {
    pub const NAME: &'static str = "set_project_root";
}

fn parse_request(args: &Value) -> Result<ProjectSelectionRequest, String> {
    let SetProjectRootArgs {
        path,
        without_project,
    } = serde_json::from_value(args.clone())
        .map_err(|error| format!("Invalid tool arguments: {error}"))?;
    match (path, without_project) {
        (Some(path), None) if !path.trim().is_empty() => Ok(ProjectSelectionRequest::Project(path)),
        (None, Some(true)) => Ok(ProjectSelectionRequest::WithoutProject),
        (Some(_), Some(_)) => Err(
            "Invalid tool arguments: provide either path or withoutProject, not both".to_string(),
        ),
        (Some(_), None) => Err("Invalid tool arguments: path must not be empty".to_string()),
        (None, Some(false)) => {
            Err("Invalid tool arguments: withoutProject must be true".to_string())
        }
        (None, None) => {
            Err("Invalid tool arguments: provide either path or withoutProject=true".to_string())
        }
    }
}

fn project_name(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

fn render_project_selection(selection: ProjectRootSelection) -> ToolResult {
    let name = project_name(&selection.source_project_root);
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
        "mode": "project",
        "project_name": name,
        "access_root": selection.access_root.to_string_lossy(),
        "active_root": selection.project_root.to_string_lossy(),
        "source_project_root": selection.source_project_root.to_string_lossy(),
        "project_root": selection.project_root.to_string_lossy(),
        "scratch_root": Value::Null,
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

fn render_without_project_selection(selection: WithoutProjectSelection) -> ToolResult {
    let state = if selection.newly_selected {
        "Scratch workspace selected"
    } else {
        "Scratch workspace was already selected"
    };
    let persistence = match selection.scope {
        ProjectBindingScope::ChatGptConversation => {
            "This ChatGPT conversation is permanently attached to this private scratch workspace. The workspace and its files survive MCP reconnects and Codexify restarts; start a new chat to attach a real project."
        }
        ProjectBindingScope::McpTransportSession => {
            "This MCP transport session is attached to this private scratch workspace. The workspace is removed when the transport session ends."
        }
    };
    let content = format!(
        "{state}\nScratch workspace: {}\nConfigured project access root: {}\n{persistence}\nCall `get_agent_brief` now before using filesystem, command, memory, skill, or project-instruction tools in the scratch workspace.",
        selection.scratch_root.display(),
        selection.access_root.display()
    );
    ToolResult::text(content.clone()).with_structured(json!({
        "mode": "without_project",
        "project_name": "Chat without a project",
        "access_root": selection.access_root.to_string_lossy(),
        "active_root": selection.scratch_root.to_string_lossy(),
        "source_project_root": Value::Null,
        "project_root": Value::Null,
        "scratch_root": selection.scratch_root.to_string_lossy(),
        "repository_url": Value::Null,
        "cloned": false,
        "managed_worktree": false,
        "worktree_git_root": Value::Null,
        "worktrees_root": Value::Null,
        "worktree_mode": Value::Null,
        "warnings": [],
        "newly_selected": selection.newly_selected,
        "binding_scope": selection.scope.as_str(),
        "content": content
    }))
}

pub async fn select_and_render<F, Fut>(args: &Value, select: F) -> ToolResult
where
    F: FnOnce(ProjectSelectionRequest) -> Fut,
    Fut: Future<Output = Result<ProjectSelection, String>>,
{
    let request = match parse_request(args) {
        Ok(request) => request,
        Err(error) => return ToolResult::error(error),
    };

    let selection = match select(request).await {
        Ok(selection) => selection,
        Err(error) => return ToolResult::error(error),
    };
    match selection {
        ProjectSelection::Project(selection) => render_project_selection(selection),
        ProjectSelection::WithoutProject(selection) => render_without_project_selection(selection),
    }
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

    fn meta(&self) -> Option<rmcp::model::MetaObject> {
        Some(setup_ui::app_callable_tool_meta())
    }

    fn description(&self) -> String {
        "Bind the current ChatGPT conversation either to one source project beneath the server's configured access root or to a private scratch workspace by passing withoutProject=true. A project path may instead be an HTTPS or SSH Git repository URL ending in `.git`, including non-GitHub hosts such as GitLab, or a supported GitHub repository-root URL. GitHub HTTPS branch URLs ending in /tree/<branch>, pull-request URLs ending in /pull/<number>, and commit URLs ending in /commit/<sha> with a full 40-character hexadecimal commit ID also select exact targets. Codexify reuses an unambiguous matching local checkout, or runs non-interactive git clone in the configured project clone directory before binding. Targeted GitHub URLs fetch the exact requested target; when an existing source checkout is on another commit, Codexify leaves it unchanged and selects a detached managed worktree at the requested commit. Worktree mode `never` therefore requires that the source checkout already be at the requested target. Local/file, insecure, credential-bearing HTTPS, and other arbitrary Git transports are rejected. ChatGPT project and scratch bindings survive MCP reconnects and server restarts and cannot be changed; start a new chat for another choice. Clients without ChatGPT's stable conversation metadata fall back to binding the current MCP transport session, whose scratch workspace is deleted on disconnect. When only a project name or purpose is known, call list_projects first and pass one unambiguous result's selector as path. Do not guess among plausible projects. In multi-project mode, make one project or scratch choice before filesystem, search, edit, command, Git, project-instruction, skill, memory, or plan work, then call get_agent_brief.".into()
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
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Existing project selector/path or supported Git repository URL."
                },
                "withoutProject": {
                    "const": true,
                    "description": "Choose a private scratch workspace instead of attaching a project."
                }
            },
            "oneOf": [
                {
                    "required": ["path"],
                    "not": { "required": ["withoutProject"] }
                },
                {
                    "required": ["withoutProject"],
                    "not": { "required": ["path"] }
                }
            ],
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "mode": { "type": "string", "enum": ["project", "without_project"] },
                "project_name": { "type": "string" },
                "access_root": { "type": "string" },
                "active_root": { "type": "string" },
                "source_project_root": { "type": ["string", "null"] },
                "project_root": { "type": ["string", "null"] },
                "scratch_root": { "type": ["string", "null"] },
                "repository_url": { "type": ["string", "null"] },
                "cloned": { "type": "boolean" },
                "managed_worktree": { "type": "boolean" },
                "worktree_git_root": { "type": ["string", "null"] },
                "worktrees_root": { "type": ["string", "null"] },
                "worktree_mode": {
                    "anyOf": [
                        { "type": "string", "enum": ["auto", "always", "never"] },
                        { "type": "null" }
                    ]
                },
                "warnings": { "type": "array", "items": { "type": "string" } },
                "newly_selected": { "type": "boolean" },
                "binding_scope": { "type": "string", "enum": ["chatgpt_conversation", "mcp_transport_session"] },
                "content": { "type": "string" }
            },
            "required": ["mode", "project_name", "access_root", "active_root", "source_project_root", "project_root", "scratch_root", "repository_url", "cloned", "managed_worktree", "worktree_git_root", "worktrees_root", "worktree_mode", "warnings", "newly_selected", "binding_scope", "content"],
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
        select_and_render(&args, |request| async move {
            match request {
                ProjectSelectionRequest::Project(path) => session
                    .select_project_root(config, &path)
                    .await
                    .map(ProjectSelection::Project),
                ProjectSelectionRequest::WithoutProject => session
                    .select_without_project(config)
                    .await
                    .map(ProjectSelection::WithoutProject),
            }
        })
        .await
    }
}
