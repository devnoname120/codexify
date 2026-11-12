use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent_automation::{
    GoalFinish, GoalRecord, GoalStatus, MAX_GOAL_BYTES, OwnerGoalCommand,
};
use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolBehavior, ToolRequestContext, empty_object_schema, parse_tool_args};
use crate::types::{AppConfig, ToolResult};

const MAX_GOAL_RESULT_BYTES: usize = 4096;

#[derive(Clone, Copy)]
pub enum GoalTool {
    Read,
    Set,
    Resume,
    Finish,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SetArgs {
    request_id: String,
    expected_revision: u64,
    markdown: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ResumeArgs {
    request_id: String,
    expected_revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct FinishArgs {
    expected_revision: u64,
    status: FinishStatus,
    summary: Option<String>,
    reason: Option<String>,
    next_steps: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum FinishStatus {
    Completed,
    Blocked,
}

#[async_trait]
impl Tool for GoalTool {
    fn name(&self) -> &'static str {
        match self {
            Self::Read => "goal_read",
            Self::Set => "goal_set",
            Self::Resume => "goal_resume",
            Self::Finish => "goal_finish",
        }
    }

    fn title(&self) -> String {
        match self {
            Self::Read => "Read agent-chat goal",
            Self::Set => "Set and start agent-chat goal",
            Self::Resume => "Resume agent-chat goal",
            Self::Finish => "Finish agent-chat goal",
        }
        .into()
    }

    fn description(&self) -> String {
        match self {
            Self::Read => {
                "Read the current standalone agent-chat automation goal, revision, and status without changing it."
            }
            Self::Set => {
                "Create or update this standalone agent-chat goal and activate it immediately. Read goal_read first; pass expectedRevision=0 only when no goal exists, otherwise its current revision. Updating an active goal retains its elapsed time and turn count. Use a fresh requestId for each operation and reuse it only to retry identical arguments."
            }
            Self::Resume => {
                "Resume the exact current paused or blocked standalone agent-chat goal without changing its text, revision, elapsed time, or turn count. An already active goal remains active. A completed goal requires goal_set to create a new revision. Use a fresh requestId for each operation and reuse it only to retry identical arguments."
            }
            Self::Finish => {
                "NON-TERMINAL TOOL. Mark the exact current goal revision completed or blocked and publish its report inline for the user. For completed, summary MUST describe work performed, results and verification; nextSteps MUST describe suggested follow-up (explicitly say none if none is needed). For blocked, reason MUST explain the concrete blocker and nextSteps MUST explain exactly what input, action or external change will unblock it. Never block merely for a timeout or rejected command. This finishes only the goal, NEVER the assistant turn: no normal ChatGPT final answer. After completed call turn_completed repeatedly to wait for requests; after blocked call chat_await repeatedly for the needed input, then goal_resume only once the blocker is resolved. Follow returned user input first. Do not stop because of elapsed time or an anticipated timeout. Infinite goals require the identical concrete blocker to persist across three distinct goal turns spanning at least 60 seconds. Check alternatives, continue independent work, and restore progress with rollout_read after context loss."
            }
        }
        .into()
    }

    fn behavior(&self) -> ToolBehavior {
        match self {
            Self::Read => ToolBehavior::new(
                true,
                false,
                true,
                false,
                "Reads private task goal state without changing it.",
            ),
            Self::Finish => ToolBehavior::new(
                false,
                false,
                true,
                false,
                "Changes only private task goal state and is idempotent for the same terminal receipt.",
            ),
            Self::Set | Self::Resume => ToolBehavior::new(
                false,
                false,
                true,
                false,
                "Activates private task goal state with an atomic revision check and durable request deduplication.",
            ),
        }
    }

    fn input_schema(&self) -> Value {
        match self {
            Self::Read => empty_object_schema(),
            Self::Set | Self::Resume => {
                let mut schema = json!({
                    "type": "object",
                    "properties": {
                        "requestId": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 512,
                            "description": "Unique operation identifier (for example a UUID). Reuse only for an identical retry.",
                            "writeOnly": true
                        },
                        "expectedRevision": {
                            "type": "integer",
                            "minimum": if matches!(self, Self::Set) { 0 } else { 1 },
                            "description": "Revision returned by goal_read; goal_set uses 0 if the goal is absent."
                        }
                    },
                    "required": ["requestId", "expectedRevision"],
                    "additionalProperties": false
                });
                if matches!(self, Self::Set) {
                    schema["properties"]["markdown"] = json!({
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_GOAL_BYTES,
                        "writeOnly": true
                    });
                    schema["required"]
                        .as_array_mut()
                        .expect("array")
                        .push(json!("markdown"));
                }
                schema
            }
            Self::Finish => json!({
                "type": "object",
                "properties": {
                    "expectedRevision": {
                        "type": "integer",
                        "minimum": 1
                    },
                    "status": {
                        "type": "string",
                        "enum": ["completed", "blocked"]
                    },
                    "summary": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_GOAL_RESULT_BYTES,
                        "description": "Required when status is completed: overview of work done, results and verification. Together with nextSteps, the formatted report must fit within 4096 UTF-8 bytes.",
                        "writeOnly": true
                    },
                    "reason": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_GOAL_RESULT_BYTES,
                        "description": "Required when status is blocked: concrete explanation of why work cannot proceed.",
                        "writeOnly": true
                    },
                    "nextSteps": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_GOAL_RESULT_BYTES,
                        "description": "Mandatory for both outcomes. Blocked: exact input, action or external change needed to unblock. Completed: suggested next steps, or explicitly state none are needed. Included in the inline report; combined formatted report limit is 4096 UTF-8 bytes.",
                        "writeOnly": true
                    }
                },
                "required": ["expectedRevision", "status", "nextSteps"],
                "anyOf": [
                    {"properties": {"status": {"const": "completed"}}, "required": ["summary"]},
                    {"properties": {"status": {"const": "blocked"}}, "required": ["reason"]}
                ],
                "additionalProperties": false
            }),
        }
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": { "type": "string" },
                "enabled": { "type": "boolean" },
                "status": {
                    "type": "string",
                    "enum": ["absent", "configured", "active", "completed", "blocked", "error"]
                },
                "revision": {
                    "anyOf": [
                        { "type": "integer", "minimum": 1 },
                        { "type": "null" }
                    ]
                },
                "markdown": {
                    "anyOf": [
                        { "type": "string", "maxLength": MAX_GOAL_BYTES },
                        { "type": "null" }
                    ]
                },
                "summary": {
                    "anyOf": [
                        { "type": "string", "maxLength": MAX_GOAL_RESULT_BYTES },
                        { "type": "null" }
                    ]
                },
                "blockedReason": {
                    "anyOf": [
                        { "type": "string", "maxLength": MAX_GOAL_RESULT_BYTES },
                        { "type": "null" }
                    ]
                },
                "terminal": { "type": "boolean", "description": "Terminal goal receipt only; never permission to end the assistant turn." },
                "assistant_turn_may_end": { "type": "boolean", "const": false },
                "required_next_action": { "type": "string", "enum": ["continue_work", "turn_completed", "chat_await", "chat_read"] }
            },
            "required": [
                "content",
                "enabled",
                "status",
                "revision",
                "markdown",
                "summary",
                "blockedReason",
                "terminal",
                "assistant_turn_may_end",
                "required_next_action"
            ],
            "additionalProperties": false
        }))
    }

    async fn call(&self, _: Value, config: &AppConfig, _: &SessionState) -> ToolResult {
        if !config.markdown_chat.automation.enabled {
            return ToolResult::error("Agent-chat browser automation is disabled.");
        }
        ToolResult::error("Agent-chat goals require stable ChatGPT conversation and task context.")
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
        let (task, store) = match task_store(context) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(error),
        };
        match self {
            Self::Read => match store.load(task) {
                Ok(state) => goal_result(state.goal.as_ref(), false),
                Err(error) => ToolResult::error(error),
            },
            Self::Set | Self::Resume => {
                let (request_id, revision, command) = if matches!(self, Self::Set) {
                    let args = match parse_tool_args::<SetArgs>(args) {
                        Ok(args) => args,
                        Err(error) => return *error,
                    };
                    (
                        args.request_id,
                        args.expected_revision,
                        OwnerGoalCommand::Set {
                            markdown: args.markdown,
                        },
                    )
                } else {
                    let args = match parse_tool_args::<ResumeArgs>(args) {
                        Ok(args) => args,
                        Err(error) => return *error,
                    };
                    (
                        args.request_id,
                        args.expected_revision,
                        OwnerGoalCommand::Run,
                    )
                };
                // Resolve the chat before changing the goal so unavailable chat state
                // cannot leave an activated goal behind as a side effect of resolution.
                let chat = match store.task_directory(task).and_then(|path| {
                    context
                        .markdown_chat
                        .chat_at_path(path.join("CHAT.md"), true)
                }) {
                    Ok(chat) => chat,
                    Err(error) => return ToolResult::error(error),
                };
                let receipt = match store.apply_agent_goal_command(
                    task,
                    &request_id,
                    revision,
                    command,
                    crate::markdown_chat::now_ms(),
                ) {
                    Ok(receipt) => receipt,
                    Err(error) => return ToolResult::error(error),
                };
                if !receipt.replayed {
                    if let Err(error) = chat.resume_agent().await {
                        return ToolResult::error(error);
                    }
                    let activity = match chat.offline_activity().await {
                        Ok(activity) => activity,
                        Err(error) => return ToolResult::error(error),
                    };
                    if let Err(error) = store.sync_goal_presence(
                        task,
                        activity.last_call_at_ms,
                        activity.agent_waiting_until_ms,
                        activity.agent_stop_requested,
                        crate::markdown_chat::now_ms(),
                    ) {
                        return ToolResult::error(error);
                    }
                }
                // A retry may arrive after an owner edit/pause. Return current state,
                // and never reapply the old activation or fabricate a user message.
                match store.load(task) {
                    Ok(state) => goal_result(state.goal.as_ref(), false),
                    Err(error) => ToolResult::error(error),
                }
            }
            Self::Finish => {
                let args = match parse_tool_args::<FinishArgs>(args) {
                    Ok(args) => args,
                    Err(error) => return *error,
                };
                let finish = match finish_report(&args) {
                    Ok(finish) => finish,
                    Err(error) => return ToolResult::error(error),
                };
                let chat = match store.task_directory(task).and_then(|path| {
                    context
                        .markdown_chat
                        .chat_at_path(path.join("CHAT.md"), true)
                }) {
                    Ok(chat) => chat,
                    Err(error) => return ToolResult::error(error),
                };
                let calls = match chat.owner_summary().await {
                    Ok(summary) => summary.total_tool_calls,
                    Err(error) => return ToolResult::error(error),
                };
                if let GoalFinish::Blocked { reason } = &finish
                    && let Err(error) = store.audit_goal_blocker(
                        task,
                        args.expected_revision,
                        args.reason.as_deref().unwrap_or(reason).trim(),
                        crate::markdown_chat::now_ms(),
                    )
                {
                    return ToolResult::error(error);
                }
                match store.finish_goal_with_tool_count(
                    task,
                    args.expected_revision,
                    finish,
                    crate::markdown_chat::now_ms(),
                    Some(calls),
                ) {
                    Ok(goal) => {
                        if let Err(error) = chat.record_goal_finish(&goal).await {
                            return ToolResult::error(error);
                        }
                        // Notify the durable terminal receipt immediately; the supervisor also
                        // checks it after restarts, using the same persisted deduplication claim.
                        if let Ok(path) = store.task_directory(task)
                            && let Ok(chat) = context
                                .markdown_chat
                                .chat_at_path(path.join("CHAT.md"), true)
                        {
                            let mut state = crate::agent_automation::AutomationTaskState::default();
                            state.goal = Some(goal.clone());
                            store.notify_state(&chat, &state).await;
                        }
                        let mut terminal_result = goal_result(Some(&goal), true);
                        if goal.status == GoalStatus::Completed {
                            let released = async {
                                if chat.queue_turn_completed().await? {
                                    chat.release_queued_message(crate::markdown_chat::now_ms())
                                        .await?;
                                }
                                chat.read(false).await
                            }
                            .await;
                            match released {
                                Ok(pending) if !pending.text.is_empty() => {
                                    let content = format!(
                                        "Goal revision {} is completed. Another user request is ready. Call chat_read now, then acknowledge it with chat_write and handle it before calling turn_completed. Do not send a normal ChatGPT final answer or end the assistant turn.",
                                        goal.revision
                                    );
                                    terminal_result.content =
                                        vec![crate::types::ToolContent::Text(content.clone())];
                                    if let Some(value) = terminal_result.structured_content.as_mut()
                                    {
                                        value["terminal"] = json!(false);
                                        value["required_next_action"] = json!("chat_read");
                                        value["content"] = json!(content);
                                    }
                                    terminal_result.new_chat_message_from_user = Some(pending.text);
                                    terminal_result.chat_delivery_end = Some(pending.end);
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    tracing::warn!(%error, "completed goal left queued requests pending")
                                }
                            }
                        }
                        terminal_result
                    }
                    Err(error) => ToolResult::error(error),
                }
            }
        }
    }
}

fn task_store(
    context: &ToolRequestContext,
) -> Result<
    (
        &crate::project_bindings::ConversationIdentity,
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
    let store = context
        .agent_automation
        .as_deref()
        .ok_or("Agent-chat automation state is unavailable for this task")?;
    Ok((task, store))
}

// Keep the complete report in the existing terminal text fields, so old task
// records and timeline/notification readers remain compatible after upgrades.
fn finish_report(args: &FinishArgs) -> Result<GoalFinish, String> {
    let next = args.next_steps.trim();
    if next.is_empty() {
        return Err("goal_finish requires non-empty nextSteps: how to unblock, or suggested follow-up after completion. Correct the arguments and retry goal_finish; do not end the assistant turn.".into());
    }
    let (detail, heading, next_heading) = match args.status {
        FinishStatus::Completed => {
            if args
                .reason
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            {
                return Err("A completed goal accepts summary, not reason. Correct the arguments and retry goal_finish.".into());
            }
            (
                args.summary.as_deref().unwrap_or("").trim(),
                "Work completed",
                "Suggested next steps",
            )
        }
        FinishStatus::Blocked => {
            if args
                .summary
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            {
                return Err("A blocked goal accepts reason, not summary. Correct the arguments and retry goal_finish.".into());
            }
            (
                args.reason.as_deref().unwrap_or("").trim(),
                "Why blocked",
                "To unblock",
            )
        }
    };
    if detail.is_empty() {
        return Err(match args.status {
            FinishStatus::Completed => "A completed goal requires a non-empty summary of work done, results and verification. Correct the arguments and retry goal_finish; do not end the assistant turn.",
            FinishStatus::Blocked => "A blocked goal requires a concrete reason explaining why work cannot proceed. Correct the arguments and retry goal_finish; do not end the assistant turn.",
        }.into());
    }
    let report = format!("### {heading}\n\n{detail}\n\n### {next_heading}\n\n{next}");
    if report.len() > MAX_GOAL_RESULT_BYTES {
        return Err("Goal report including headings must fit within 4096 UTF-8 bytes. Shorten the report and retry goal_finish; do not end the assistant turn.".into());
    }
    Ok(match args.status {
        FinishStatus::Completed => GoalFinish::Completed {
            summary: Some(report),
        },
        FinishStatus::Blocked => GoalFinish::Blocked { reason: report },
    })
}

fn goal_result(goal: Option<&GoalRecord>, terminal_receipt: bool) -> ToolResult {
    let (content, status, revision, markdown, summary, blocked_reason, terminal) = match goal {
        None => (
            "No agent-chat goal is configured.".to_string(),
            "absent",
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            false,
        ),
        Some(goal) => {
            let status = goal_status(goal.status);
            let content = match goal.status {
                GoalStatus::Configured => format!(
                    "Agent-chat goal revision {} is configured but not active.",
                    goal.revision
                ),
                GoalStatus::Active => format!(
                    "Agent-chat goal revision {} is active. Evaluate this exact revision while working.",
                    goal.revision
                ),
                GoalStatus::Completed => format!(
                    "Agent-chat goal revision {} is completed and its report is visible inline. Required next call: turn_completed. On timeout or cancellation without input, call turn_completed again. Do not send a normal ChatGPT final answer or end the assistant turn, including near an anticipated timeout.",
                    goal.revision
                ),
                GoalStatus::Blocked => format!(
                    "Agent-chat goal revision {} is blocked and its explanation and unblock steps are visible inline. Required next call: chat_await. On timeout or cancellation without input, call chat_await again. Once the blocker is resolved, call goal_resume before continuing goal work. Do not send a normal ChatGPT final answer or end the assistant turn, including near an anticipated timeout.",
                    goal.revision
                ),
                GoalStatus::Error => format!(
                    "Agent-chat goal revision {} is in an unrecoverable error state.",
                    goal.revision
                ),
            };
            (
                content,
                status,
                json!(goal.revision),
                json!(goal.markdown),
                json!(goal.summary),
                json!(goal.blocked_reason),
                terminal_receipt
                    && matches!(goal.status, GoalStatus::Completed | GoalStatus::Blocked),
            )
        }
    };
    let mut result = ToolResult::text(content.clone()).with_structured(json!({
        "content": content,
        "enabled": true,
        "status": status,
        "revision": revision,
        "markdown": markdown,
        "summary": summary,
        "blockedReason": blocked_reason,
        "terminal": terminal,
        "assistant_turn_may_end": false,
        "required_next_action": match goal.map(|goal| goal.status) {
            Some(GoalStatus::Completed) => "turn_completed",
            Some(GoalStatus::Blocked | GoalStatus::Configured) => "chat_await",
            Some(GoalStatus::Active) => "continue_work",
            _ => "continue_work",
        }
    }));
    result.audit.sensitive_output = true;
    result
}

fn goal_status(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Configured => "configured",
        GoalStatus::Active => "active",
        GoalStatus::Completed => "completed",
        GoalStatus::Blocked => "blocked",
        GoalStatus::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool;

    #[test]
    fn goal_payloads_are_private_for_tool_logging() {
        let schema = GoalTool::Finish.input_schema();
        assert_eq!(schema["properties"]["summary"]["writeOnly"], true);
        assert_eq!(schema["properties"]["reason"]["writeOnly"], true);
        assert_eq!(schema["properties"]["nextSteps"]["writeOnly"], true);
        assert_eq!(
            GoalTool::Set.input_schema()["properties"]["markdown"]["writeOnly"],
            true
        );

        let goal = GoalRecord {
            revision: 1,
            markdown: "private goal markdown".into(),
            status: GoalStatus::Blocked,
            created_at_ms: 1,
            updated_at_ms: 2,
            finished_at_ms: Some(2),
            summary: None,
            blocked_reason: Some("private blocker".into()),
            elapsed_active_ms: 0,
            active_since_ms: None,
            timer_until_ms: None,
            turn_count: 0,
            completed_chat_end: None,
            completed_tool_call_count: None,
        };
        let result = goal_result(Some(&goal), true);
        assert!(result.audit.sensitive_output);
    }
}
