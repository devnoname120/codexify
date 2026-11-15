use super::agents::{AgentTool, compact_team};
use crate::agent_automation::agents::AgentClientRequest;
use crate::agent_automation::agents::AgentRuntime;
use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolBehavior, ToolRequestContext};
use crate::types::{AppConfig, ToolResult};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub struct AgentClient;

impl AgentClient {
    pub(crate) async fn execute(
        args: Value,
        runtime: &AgentRuntime,
        cancellation: &CancellationToken,
    ) -> ToolResult {
        let request = match serde_json::from_value::<AgentClientRequest>(args) {
            Ok(request) => request,
            Err(error) => {
                return ToolResult::error(format!("Invalid agent_client request: {error}"));
            }
        };
        let full_reports = matches!(
            &request,
            AgentClientRequest::Status {
                include_reports: true,
                ..
            }
        );
        match runtime.client_request(request, cancellation).await {
            Ok(mut value) => {
                if !full_reports {
                    if let Some(team) = value.get_mut("team") {
                        *team = compact_team(team.take());
                    }
                    if let Some(team) = value
                        .get_mut("receipt")
                        .and_then(|value| value.get_mut("team"))
                    {
                        *team = compact_team(team.take());
                    }
                }
                ToolResult::text(serde_json::to_string_pretty(&value).unwrap_or_default())
                    .with_structured(value)
            }
            Err(error) => ToolResult::error(error),
        }
    }
}

#[async_trait]
impl Tool for AgentClient {
    fn name(&self) -> &'static str {
        "agent_client"
    }
    fn title(&self) -> String {
        "Manage Codexify agents".into()
    }
    fn description(&self) -> String {
        "Create and control persistent Codexify teams from any AI client. Call capabilities, then setup with requestId, clientName, projectPath, model and effort; save teamId. Use list to reconnect, status/inbox for progress and reports, spawn for scoped workers, and control for message/set_goal/pause/resume/stop/retire. Lifecycle changes require the worker's current expectedRevision. Retry with identical requestId arguments. Workers implement authorized assignments by default; set readOnly:true for an audit. A read-only parent cannot delegate editing. Use includeReports:true for full evidence. Respect stops; setup replays do not resume teams. Enable Agent teams and AI agent control in Settings. The service publishes one MCP/HTTP/JavaScript connection at startup; discover it with codexify agents connection. ChatGPT worker conversations retain their regular scoped agent tools.".into()
    }
    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            false,
            false,
            false,
            "Creates persistent project-bound teams and controls only workers recorded in those teams; uses the managed browser to create worker chats.",
        )
    }
    fn requires_project_root(&self) -> bool {
        false
    }
    fn input_schema(&self) -> Value {
        let spawn = AgentTool::Spawn.input_schema();
        let mut workers = spawn["properties"]["workers"].clone();
        workers["minItems"] = json!(0);
        let control = AgentTool::Control.input_schema();
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["capabilities","setup","list","status","spawn","control","inbox","stop_team"],"description":"capabilities/list need no other fields. setup requires requestId, clientName, projectPath, model, effort; workers and createWorktree are optional. status/inbox require teamId. spawn requires teamId, requestId and 1–8 workers. control requires teamId and control. stop_team requires teamId and requestId. Each action accepts only its own fields."},
            "teamId":{"type":"string","pattern":"^[a-f0-9]{64}$"},
            "afterTeamId":{"type":"string","pattern":"^[a-f0-9]{64}$","description":"For list only: pass nextAfterTeamId from the previous page when hasMore is true."},
            "requestId":{"type":"string","minLength":1,"maxLength":80,"pattern":"^[A-Za-z0-9_-]+$"},
            "clientName":{"type":"string","minLength":1,"maxLength":80},
            "projectPath":{"type":"string","minLength":1,"maxLength":4096},
            "model":{"type":"string","minLength":1,"maxLength":256},
            "effort":{"type":"string","minLength":1,"maxLength":256},
            "createWorktree":{"type":"boolean","default":false},
            "workers":workers,
            "includeReports":{"type":"boolean","default":false},
            "control":control,
            "before":{"type":"integer","minimum":0},
            "knownRevision":{"type":"string","maxLength":256}
        },"required":["action"],"additionalProperties":false})
    }
    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        serde_json::from_value::<AgentClientRequest>(args.clone())
            .map(|_| ())
            .map_err(|e| format!("Invalid agent_client request: {e}"))
    }
    async fn call(&self, _: Value, _: &AppConfig, _: &SessionState) -> ToolResult {
        ToolResult::error("agent_client requires an authenticated MCP request context")
    }
    async fn call_with_context(
        &self,
        args: Value,
        _: &AppConfig,
        _: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        if context.conversation.is_some() || context.task_conversation.is_some() {
            return ToolResult::error(
                "ChatGPT and worker conversations must use agent_spawn/status/control/message; agent_client cannot switch their team identity",
            );
        }
        let Some(runtime) = context.agent_runtime.as_ref() else {
            return ToolResult::error(
                "Enable browser automation before using coding-app agent control",
            );
        };
        Self::execute(args, runtime, &context.cancellation).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generic_client_schema_and_parser_reject_ambiguous_controls() {
        let tool = crate::tool::validate_and_wrap_tool(Box::new(AgentClient)).unwrap();
        let schema = tool.input_schema();
        for combinator in ["oneOf", "anyOf", "allOf"] {
            assert!(schema.get(combinator).is_none());
        }
        let validator = jsonschema::validator_for(&schema).unwrap();
        for args in [
            json!({"action":"status"}),
            json!({"action":"control","teamId":"a".repeat(64)}),
            json!({"action":"setup","requestId":"setup"}),
            json!({"action":"list","teamId":"../../other"}),
        ] {
            assert!(AgentClient.validate_arguments(&args).is_err());
        }
        assert!(
            AgentClient
                .validate_arguments(&json!({"action":"list","workers":[]}))
                .is_err()
        );
        assert!(validator.is_valid(&json!({"action":"inbox","teamId":"a".repeat(64)})));
        assert!(!tool.requires_project_root());
    }
}
