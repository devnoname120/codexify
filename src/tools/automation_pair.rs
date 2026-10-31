use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolBehavior, ToolRequestContext, parse_tool_args};
use crate::types::{AppConfig, ToolResult};

pub struct AutomationVerifyPairing;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyArgs {
    reference: String,
}

impl AutomationVerifyPairing {
    pub(crate) fn reference(args: &Value) -> Result<String, String> {
        serde_json::from_value::<VerifyArgs>(args.clone())
            .map(|args| args.reference)
            .map_err(|error| format!("Invalid pairing arguments: {error}"))
    }

    pub(crate) fn continuation_required(
        token: &str,
        reference: &str,
        generation: u64,
    ) -> ToolResult {
        let content = format!(
            "This ChatGPT conversation has no workspace attached yet. Call `continue_task` with continuationToken `{token}` to attach the existing task and its already-selected workspace. Then call `automation_verify_pairing` again with reference `{reference}`. Do not select a project, create a worktree, inspect files, or perform task work before pairing succeeds. Do not stop or end this turn; run continue_task now, then retry verification."
        );
        ToolResult::text(&content).with_structured(json!({
            "content": content, "verified": false, "pairingGeneration": generation, "terminal": false
        }))
    }
}

#[async_trait]
impl Tool for AutomationVerifyPairing {
    fn name(&self) -> &'static str {
        "automation_verify_pairing"
    }

    fn title(&self) -> String {
        "Verify agent-chat pairing".into()
    }

    fn description(&self) -> String {
        "Consume the one-time pairing reference sent by Codexify's standalone agent chat. The call verifies only that this already-authorized ChatGPT conversation owns the intended Codexify task; it does not transfer task ownership or authorize setup.".into()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            false,
            false,
            false,
            "Consumes one task-scoped pairing reference without modifying project files or external systems.",
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "reference": {
                    "type": "string",
                    "minLength": 43,
                    "maxLength": 43,
                    "pattern": "^[A-Za-z0-9_-]+$",
                    "writeOnly": true,
                    "description": "The one-time pairing reference supplied by the standalone Codexify chat."
                }
            },
            "required": ["reference"],
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": { "type": "string" },
                "verified": { "type": "boolean" },
                "pairingGeneration": { "type": "integer", "minimum": 1 },
                "terminal": { "type": "boolean" }
            },
            "required": ["content", "verified", "pairingGeneration", "terminal"],
            "additionalProperties": false
        }))
    }

    async fn call(&self, _: Value, config: &AppConfig, _: &SessionState) -> ToolResult {
        if !config.markdown_chat.automation.enabled {
            return ToolResult::error("Agent-chat browser automation is disabled.");
        }
        ToolResult::error(
            "Agent-chat pairing verification requires stable ChatGPT conversation and task context.",
        )
    }

    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        _session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        if !config.markdown_chat.automation.enabled {
            return ToolResult::error("Agent-chat browser automation is disabled.");
        }
        let args = match parse_tool_args::<VerifyArgs>(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        let (task, owner_generation, store) = match pairing_context(context) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(error),
        };
        match store.verify_pairing_reference(
            task,
            owner_generation,
            &args.reference,
            crate::markdown_chat::now_ms(),
        ) {
            Ok(pairing) => {
                let content = "Agent-chat pairing verified. Do not perform task work in this verification turn.";
                ToolResult::text(content).with_structured(json!({
                    "content": content,
                    "verified": true,
                    "pairingGeneration": pairing.generation,
                    "terminal": true
                }))
            }
            Err(error) => ToolResult::error(error),
        }
    }
}

fn pairing_context(
    context: &ToolRequestContext,
) -> Result<
    (
        &crate::project_bindings::ConversationIdentity,
        u64,
        &crate::agent_automation::AgentAutomationStore,
    ),
    String,
> {
    if context.conversation.is_none()
        || context.task_conversation.is_none()
        || context.task_owner_generation.is_none()
        || context.conversation_retired
    {
        return Err(
            "Agent-chat automation requires an active stable ChatGPT conversation and task.".into(),
        );
    }
    let task = context.task_conversation.as_ref().expect("checked task");
    let generation = context.task_owner_generation.expect("checked generation");
    let store = context
        .agent_automation
        .as_deref()
        .ok_or("Agent-chat automation state is unavailable for this task")?;
    Ok((task, generation, store))
}
