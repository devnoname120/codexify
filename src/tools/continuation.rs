use async_trait::async_trait;
use rmcp::model::MetaObject;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolBehavior, empty_object_schema};
use crate::types::{AppConfig, ToolResult};

pub const CONTINUATION_META: &str = "io.github.devnoname120/codexify/continuation";

pub struct PrepareContinuation;

impl PrepareContinuation {
    pub const NAME: &'static str = "setup_ui_prepare_continuation";

    pub(crate) fn result(token: &str, workspace: &str) -> ToolResult {
        let mut result = ToolResult::text("Continuation prompt ready.")
            .with_structured(json!({"content":"Continuation prompt ready."}));
        result.meta = Some(
            serde_json::from_value(json!({
                CONTINUATION_META: {
                    "token": token,
                    "workspace": workspace
                }
            }))
            .expect("continuation result metadata"),
        );
        result
    }
}

#[async_trait]
impl Tool for PrepareContinuation {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn title(&self) -> String {
        "Prepare conversation continuation".into()
    }

    fn description(&self) -> String {
        "App-only action that prepares a one-time prompt for continuing this Codexify task in another ChatGPT conversation.".into()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            false,
            false,
            false,
            "Creates one private continuation capability and revokes an older unused capability for the same task.",
        )
    }

    fn meta(&self) -> Option<MetaObject> {
        Some(
            serde_json::from_value(json!({
                "ui":{"visibility":["app"]},
                "openai/visibility":"private",
                "openai/widgetAccessible":true
            }))
            .expect("app-only continuation metadata"),
        )
    }

    fn input_schema(&self) -> Value {
        empty_object_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(crate::tool::text_output_schema())
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, _: Value, _: &AppConfig, _: &SessionState) -> ToolResult {
        ToolResult::error("Continuation preparation requires request context.")
    }
}

pub struct ContinueTask;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ContinueTaskArgs {
    continuation_token: String,
}

impl ContinueTask {
    pub const NAME: &'static str = "continue_task";

    pub(crate) fn token(args: &Value) -> Result<String, String> {
        let parsed: ContinueTaskArgs = serde_json::from_value(args.clone())
            .map_err(|error| format!("Invalid arguments for `{}`: {error}", Self::NAME))?;
        Ok(parsed.continuation_token)
    }

    pub(crate) fn result(workspace: &str, pairing_pending: bool) -> ToolResult {
        let content = if pairing_pending {
            "This Codexify task and its existing workspace are now attached. Call automation_verify_pairing now with the reference supplied in the pairing request. Do not select another workspace, start task work, stop, or end this turn before verification succeeds."
        } else {
            "This Codexify task is now attached to this ChatGPT conversation. Call get_agent_brief and recall before continuing."
        };
        ToolResult::text(content).with_structured(json!({
            "content":content,
            "continued":true,
            "activeRoot":workspace
        }))
    }
}

#[async_trait]
impl Tool for ContinueTask {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn title(&self) -> String {
        "Continue a Codexify task".into()
    }

    fn description(&self) -> String {
        "Use the one-time continuationToken from a Codexify handoff prompt before selecting a workspace. The existing workspace, agent-chat history, saved plan, memory, diff state, and resident command sessions remain attached; the previous ChatGPT conversation becomes read-only.".into()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            false,
            true,
            false,
            "Moves private ownership of one existing Codexify task to the current ChatGPT conversation; repeating it from the new owner is a no-op.",
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "continuationToken":{
                    "type":"string",
                    "minLength":43,
                    "maxLength":43,
                    "writeOnly":true,
                    "description":"Opaque one-time token copied from the previous conversation's Codexify setup card."
                }
            },
            "required":["continuationToken"],
            "additionalProperties":false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type":"object",
            "properties":{
                "content":{"type":"string"},
                "continued":{"type":"boolean"},
                "activeRoot":{"type":"string"}
            },
            "required":["content", "continued", "activeRoot"],
            "additionalProperties":false
        }))
    }

    fn fills_structured_content(&self) -> bool {
        false
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, _: Value, _: &AppConfig, _: &SessionState) -> ToolResult {
        ToolResult::error("Task continuation requires request context.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_result_keeps_the_token_component_only() {
        let result = PrepareContinuation::result(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "/workspace",
        );
        assert_eq!(result.joined_text(), "Continuation prompt ready.");
        assert_eq!(
            result.structured_content,
            Some(json!({"content":"Continuation prompt ready."}))
        );
        let serialized = serde_json::to_string(&result.structured_content).unwrap();
        assert!(!serialized.contains("AAAA"));
        assert_eq!(
            result.meta.as_ref().unwrap()[CONTINUATION_META],
            json!({
                "token":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                "workspace":"/workspace"
            })
        );
    }

    #[test]
    fn continuation_tools_have_private_prepare_and_write_only_claim_contracts() {
        let prepare = PrepareContinuation;
        let prepare_meta = prepare.meta().unwrap();
        assert_eq!(
            prepare_meta.get("openai/visibility"),
            Some(&json!("private"))
        );
        assert!(!prepare.behavior().read_only);
        assert!(!prepare.behavior().idempotent);

        let claim = ContinueTask;
        let schema = claim.input_schema();
        assert_eq!(schema["required"], json!(["continuationToken"]));
        assert_eq!(schema["properties"]["continuationToken"]["writeOnly"], true);
        assert_eq!(schema["properties"]["continuationToken"]["minLength"], 43);
        assert_eq!(schema["properties"]["continuationToken"]["maxLength"], 43);
        assert!(claim.meta().is_none());
        assert!(!claim.behavior().read_only);
        assert!(claim.behavior().idempotent);
    }
}
