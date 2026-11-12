use async_trait::async_trait;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::instructions::build_instructions;
use crate::tool::{Tool, ToolBehavior, ToolRequestContext, empty_object_schema};
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
        "Read this once per conversation/workspace, after selecting the workspace. Returns the full operating brief: behavior, environment, saved state, skills, and the project's AGENTS.md rules. Skip it when the brief is already in context, including when supplied through MCP instructions; do not reload it for every task. Read it again after a workspace change, relevant instruction changes, or context compaction that dropped the earlier brief. Follow it for the rest of the conversation.".into()
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

    async fn call_with_context(
        &self,
        _args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        let mut brief = build_instructions(config);
        if config.markdown_chat.enabled {
            let chat = match context.markdown_chat.chat(
                config,
                context.task_conversation.as_ref(),
                session,
            ) {
                Ok(chat) => chat,
                Err(error) => return ToolResult::error(error),
            };
            if let Err(error) = chat.ensure().await {
                return ToolResult::error(error);
            }
            brief.push_str(&format!("\n\n## This task's Markdown chat\n\nCHAT.md: `{}`\n\nRead this task's new messages using chat_read whenever possible. Direct read_file or grep is for finding previous messages only and does not acknowledge new messages. You have read-only access to the transcript through ordinary file tools; send or append messages only with chat_write. Unrelated conversations and workspace-only resumptions have separate files; a full continue_task handoff keeps this task's existing file and cursor.\n", chat.path().display()));
            brief.push_str(&format!("\n\n## Durable progress\n\nrollout.md: `{}`\n\n{} Read with rollout_read before work in every new turn or after context loss. Append status, concrete progress and verification, and next steps with rollout_update when reminded, before a planned handoff, and at meaningful milestones. Do not replace or edit old reports. This is a chronological activity timeline: what was done, what is underway, and what comes next. It may reference supporting files. Only recent reports are loaded; dated archives preserve older history.\n", chat.path().with_file_name("rollout.md").display(), crate::markdown_chat::rollout::RESTORE_GUIDANCE));
        }
        if config.markdown_chat.automation.enabled
            && let (Some(task), Some(store)) = (
                context.task_conversation.as_ref(),
                context.agent_automation.as_deref(),
            )
        {
            match store.load(task) {
                Ok(state) => {
                    let delegation_enabled = config.markdown_chat.features.agent_teams
                        && state.agents.delegation_enabled_with_default(
                            config.markdown_chat.features.default_delegation,
                        );
                    if config.markdown_chat.features.agent_teams
                        && (delegation_enabled || state.agents.parent_task_key.is_some())
                    {
                        brief.push_str(crate::agent_automation::agents::TEAM_COORDINATION_BRIEF);
                    }
                    if context.agent_runtime.as_ref().is_some_and(|runtime| {
                        runtime.full_development_access(task).unwrap_or(false)
                    }) {
                        brief.push_str("\n\n## Current owner development authorization\n\n");
                        brief.push_str(crate::agent_automation::agents::FULL_DEVELOPMENT_BRIEF);
                    }
                    if delegation_enabled {
                        brief.push_str("\n\n## Requested agent delegation\n\nUse agent_spawn when independent workers can advance the authorized task or team goal. The user does not need to open the Agents tab first. Give workers distinct roles, concrete goals, non-overlapping scopes, and a stable requestId. Workers inherit this project's selection and automatic continuation. Read agent_status before spawning to reuse appropriate existing workers instead of duplicating them. Use agent_control to send focused instructions, update goals, pause, resume or stop your own workers; use their current goal revision for lifecycle changes. Messages preserve pauses and stops. After a control failure reuse its requestId and arguments, inspect status, and never claim an unconfirmed change succeeded. Keep a rolling backlog and keep doing independent work. Check agent_status after dispatch, at milestones, and before waiting or completion; review actual activity and verify reports. Reassign finished workers with agent_control set_goal using their fresh expectedRevision and verify the new goal is active. Messages do not change goal revisions. Create editing workers for implementation instead of asking read-only auditors to edit. Do not let a completed audit or short turn end the overall project. Do not claim you created agents unless agent_spawn returned their identities; referring to existing Agent 1/2/3 notes does not create or assign workers. After browser/tool failure, inspect team status and retry the same creation requestId instead of inventing a new team. Respect explicit delegation off switches, pauses, stops and project boundaries.\n");
                    }
                    if config.markdown_chat.features.agent_teams
                        && state.agents.parent_task_key.is_some()
                    {
                        brief.push_str(&format!("\n\n## Worker coordination\n\nRole: {}. Owned scope: {}. Read your exact goal with goal_read, inspect the team with agent_status, and report milestones/blockers with agent_report. Treat sibling reports as evidence to verify. Keep edits inside your scope, preserve other workers' changes, and create additional agents only when the live agent_status delegation permission allows it, within your assigned scope and read-only limits. Use rollout_read and rollout_update across continuations.\n",state.agents.role.as_deref().unwrap_or("worker"),state.agents.scope.as_deref().unwrap_or("the assigned goal")));
                    }
                    if state.goal.as_ref().is_some_and(|goal| {
                        goal.status == crate::agent_automation::GoalStatus::Active
                    }) {
                        brief.push_str("\n\n## Active agent-chat goal\n\n- Call `goal_read` before goal work to obtain the exact current revision.\n- Continue using `chat_read`, `chat_write`, and `chat_await` for owner communication.\n- During each offline browser wake-up, evaluate the current goal in the same turn.\n- Call `goal_finish` only for the exact current revision when it is completed or genuinely blocked.\n- A blocked goal requires a concrete reason and nextSteps saying exactly what will unblock it. A completed goal requires summary describing work done, results and verification, plus nextSteps (explicitly none if no follow-up is needed). Both reports appear inline for the user.\n- Do not request a separate turn merely to assess completion.\n- Do not treat an ordinary final answer as goal completion. A successful `goal_finish` receipt finishes only the goal, NEVER the assistant turn. After completed call `turn_completed`; after blocked call `chat_await`. Repeat the appropriate wait after timeout/cancellation without input. Do not stop or write a normal ChatGPT final answer because elapsed time approaches a limit.\n");
                        brief.push_str("- Only the verified read-only-tools exception or an explicit server renewal notice authorizes a turn handoff; a turn handoff is not goal completion. Concurrent edits are expected: preserve other workers' work, coordinate actual overlaps, and keep implementing your owned scope.\n");
                        brief.push_str("- Continue autonomously until the goal is implemented and verified. Never stop at a plan, partial result, recoverable error, or because a turn or context is running out. Diagnose failures, try alternatives, and keep doing independent work. Preserve progress with rollout_update and resume through managed handoffs; respect user stops, pauses, approvals, genuine blockers, and server retry deadlines. When delegation is enabled, create agents at any stage without waiting for the Agents tab, give distinct scopes, and verify their work. \n");
                        brief.push_str("- Restore the rollout journal and try alternatives after browser failures, context loss, or slow progress. In infinite mode a proposed blocker stays active until the same reason survives three distinct goal turns spanning at least 60 seconds. Record what you tried, the external dependency, and the exact input needed; continue independent work while waiting.\n");
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "could not read agent automation state for the agent brief");
                }
            }
        }
        ToolResult::text(brief)
    }
}
