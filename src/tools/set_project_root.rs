use async_trait::async_trait;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::project_bindings::{ProjectBindingScope, ProjectRootSelection};
use crate::tool::{Tool, arg_str};
use crate::types::{AppConfig, ToolResult};

pub struct SetProjectRoot;

impl SetProjectRoot {
    pub const NAME: &'static str = "set_project_root";
}

pub fn select_and_render(
    args: &Value,
    select: impl FnOnce(&str) -> Result<ProjectRootSelection, String>,
) -> ToolResult {
    let Some(path) = arg_str(args, "path") else {
        return ToolResult::error("path must be a string");
    };

    let selection = match select(path) {
        Ok(selection) => selection,
        Err(error) => return ToolResult::error(error),
    };

    let state = if selection.newly_selected {
        "Project root selected"
    } else {
        "Project root was already selected"
    };
    let persistence = match selection.scope {
        ProjectBindingScope::ChatGptConversation => {
            "This ChatGPT conversation is permanently bound to that project. The binding survives MCP reconnects and server restarts; start a new chat for another project."
        }
        ProjectBindingScope::McpTransportSession => {
            "This MCP transport session is permanently bound to that project. Clients that do not provide a stable conversation identifier must select again after reconnecting."
        }
    };
    let content = format!(
        "{state}: {}\nAccess root: {}\n{persistence}\nCall `get_agent_brief` now so the environment, saved state, skills, and project instructions are loaded from the selected root.",
        selection.project_root.display(),
        selection.access_root.display()
    );

    ToolResult::text(content.clone()).with_structured(json!({
        "access_root": selection.access_root.to_string_lossy(),
        "project_root": selection.project_root.to_string_lossy(),
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

    fn description(&self) -> String {
        "Bind the current ChatGPT conversation to one project directory beneath the server's configured access root. ChatGPT bindings survive MCP reconnects and server restarts and cannot be changed; start a new chat for another project. Clients without ChatGPT's stable conversation metadata fall back to binding the current MCP transport session. In multi-project mode, call this for a new unbound conversation before any filesystem, search, edit, command, git, project-instruction, skill, memory, or plan tool, then call get_agent_brief.".into()
    }

    fn describe(&self, config: &AppConfig) -> String {
        if config.multi_project {
            format!(
                "{} The access root is `{}`. The path may be relative to that root or an absolute path inside it.",
                self.description(),
                config.work_dir.display()
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
                    "description": "Existing project directory, relative to the configured access root or an absolute path inside it"
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
                "project_root": { "type": "string" },
                "newly_selected": { "type": "boolean" },
                "binding_scope": { "type": "string", "enum": ["chatgpt_conversation", "mcp_transport_session"] },
                "content": { "type": "string" }
            },
            "required": ["access_root", "project_root", "newly_selected", "binding_scope", "content"]
        }))
    }

    fn fills_structured_content(&self) -> bool {
        false
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, config: &AppConfig, session: &SessionState) -> ToolResult {
        select_and_render(&args, |path| session.select_project_root(config, path))
    }
}
