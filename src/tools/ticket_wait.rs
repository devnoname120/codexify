use async_trait::async_trait;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolBehavior, empty_object_schema};
use crate::types::{AppConfig, ToolResult};

pub struct TicketWait;

impl TicketWait {
    pub const NAME: &'static str = "ticket_wait";
    pub(crate) const WAIT_AGAIN: &'static str = "Ticket renewal is still pending. The wait is active and functioning normally. Your required next action is ticket_wait again, without a ticket. Repeat ticket_wait after every pending, interrupted, or timed-out wait until it returns new_codexify_ticket. These recovery waits take precedence over read-only turn-handoff rules. Do not run other tools, guess or retrieve a ticket, write a final answer, end your turn, or give up while waiting. A completed wait is not permission to stop.";

    pub(crate) fn waiting(interrupted: bool) -> ToolResult {
        Self::result(
            if interrupted {
                "interrupted"
            } else {
                "waiting"
            },
            if interrupted {
                format!(
                    "The wait was interrupted without renewing a ticket. {}",
                    Self::WAIT_AGAIN
                )
            } else {
                Self::WAIT_AGAIN.into()
            },
            Self::NAME,
        )
    }

    pub(crate) fn renewed(next_action: &str) -> ToolResult {
        Self::result(
            "renewed",
            "Your agent ticket has been renewed. Use the returned new_codexify_ticket as codexify_ticket on the next ticketed call, then use each response's latest successor. Before resuming task work, call rollout_read to read the latest progress reports and understand what happened while you waited; complete setup or workspace selection first if required. If rollout_read is unavailable, call get_agent_brief to recover the current task context. Then continue the current request and goal, rechecking current files and state rather than repeating work from before the wait. Do not end your turn merely because the ticket was renewed.".into(),
            next_action,
        )
    }

    pub(crate) fn unavailable(error: impl std::fmt::Display) -> ToolResult {
        let mut result = Self::result(
            "waiting",
            format!(
                "Ticket renewal could not access its state: {error}. {}",
                Self::WAIT_AGAIN
            ),
            Self::NAME,
        );
        result.is_error = true;
        result
    }

    fn result(state: &str, content: String, next_action: &str) -> ToolResult {
        ToolResult::text(content.clone()).with_structured(json!({
            "status":state,"content":content,"required_next_action":next_action,
            "assistant_turn_may_end":false
        }))
    }
}

#[async_trait]
impl Tool for TicketWait {
    fn name(&self) -> &'static str {
        Self::NAME
    }
    fn title(&self) -> String {
        "Wait for an agent ticket".into()
    }
    fn description(&self) -> String {
        "After an agent ticket rejection, call ticket_wait immediately without a ticket. This read-only waiting tool requires no codexify_ticket and does not access project files. It waits for exclusive ticket renewal. If it returns no new_codexify_ticket, you MUST call ticket_wait again, repeating after every pending result, interruption, or timeout. Keep waiting until it supplies a new ticket; do not run other tools, issue a normal final answer, end the assistant turn, or give up. Once renewed, use the new ticket to read the latest rollout_read reports before continuing the current work (complete setup/workspace selection first if needed). Ticket renewal and wait completion never authorize ending your turn.".into()
    }
    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            false,
            false,
            "Waits for exclusive ticket admission and updates only private ticket bookkeeping, without project or user-data access.",
        )
    }
    fn input_schema(&self) -> Value {
        empty_object_schema()
    }
    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type":"object","properties":{
                "status":{"type":"string","enum":["waiting","interrupted","renewed"]},
                "content":{"type":"string"},"required_next_action":{"type":"string"},
                "assistant_turn_may_end":{"type":"boolean","const":false}
            },"required":["status","content","required_next_action","assistant_turn_may_end"],"additionalProperties":false
        }))
    }
    fn requires_project_root(&self) -> bool {
        false
    }
    fn manages_model_output_budget(&self) -> bool {
        true
    }
    async fn call(&self, _: Value, _: &AppConfig, _: &SessionState) -> ToolResult {
        Self::unavailable("ticket renewal requires server request context")
    }
}
