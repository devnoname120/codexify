use async_trait::async_trait;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::tool::{Tool, arg_str};
use crate::types::{AppConfig, ToolResult};

pub struct SetProjectRoot;

#[async_trait]
impl Tool for SetProjectRoot {
    fn name(&self) -> &'static str {
        "set_project_root"
    }

    fn description(&self) -> String {
        "Bind this MCP session to one project directory beneath the server's configured access root. In multi-project mode, call this before any filesystem, search, edit, command, git, project-instruction, skill, memory, or plan tool. The binding cannot be changed within the same session; open a new chat or MCP session for another project. After selecting, call get_agent_brief.".into()
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
                "content": { "type": "string" }
            },
            "required": ["access_root", "project_root", "newly_selected", "content"]
        }))
    }

    fn fills_structured_content(&self) -> bool {
        false
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, config: &AppConfig, session: &SessionState) -> ToolResult {
        let Some(path) = arg_str(&args, "path") else {
            return ToolResult::error("path must be a string");
        };

        let selection = match session.select_project_root(config, path) {
            Ok(selection) => selection,
            Err(error) => return ToolResult::error(error),
        };

        let state = if selection.newly_selected {
            "Project root selected"
        } else {
            "Project root was already selected"
        };
        let content = format!(
            "{state}: {}\nAccess root: {}\nThis MCP session is permanently bound to that project. Call `get_agent_brief` now so the environment, saved state, skills, and project instructions are loaded from the selected root.",
            selection.project_root.display(),
            selection.access_root.display()
        );

        ToolResult::text(content.clone()).with_structured(json!({
            "access_root": selection.access_root.to_string_lossy(),
            "project_root": selection.project_root.to_string_lossy(),
            "newly_selected": selection.newly_selected,
            "content": content
        }))
    }
}
