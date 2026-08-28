use async_trait::async_trait;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::instructions::build_instructions;
use crate::tool::{Tool, ToolBehavior, empty_object_schema};
use crate::types::{AppConfig, ToolResult};

pub struct GetAgentBrief;

#[async_trait]
impl Tool for GetAgentBrief {
    fn name(&self) -> &'static str {
        "get_agent_brief"
    }

    fn title(&self) -> String {
        "Read agent brief".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Reads generated workspace guidance without changing state.",
        )
    }

    fn description(&self) -> String {
        "Read this first, at the start of a task. Returns the full operating brief for this workspace in one call: how a coding agent is expected to behave here, the machine's OS and shell, the working directory and command policy, and the project's own AGENTS.md rules. It is the same text this server sends in its MCP instructions, so skip it if you have already been given those. Follow what it says for the rest of the conversation.".into()
    }

    fn input_schema(&self) -> Value {
        empty_object_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The operating brief: behaviour, environment, and project instructions."
                }
            },
            "required": ["content"],
            "additionalProperties": false
        }))
    }

    async fn call(&self, _args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        ToolResult::text(build_instructions(config))
    }
}
