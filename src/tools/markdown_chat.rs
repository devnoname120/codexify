use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::markdown_chat::{NotificationState, WaitOutcome, notification};
use crate::tool::{Tool, ToolBehavior, ToolRequestContext, empty_object_schema, parse_tool_args};
use crate::types::{AppConfig, ToolResult};

pub const KEEP_WAITING: &str = "Markdown chat is still active. The required next action is chat_await. Do not emit a normal assistant final response or end the turn. Call chat_await again unless the user explicitly authorized ending this turn or a higher-priority instruction requires it.";

const NEXT_CONTINUE_OR_COMPLETED: &str = "continue_or_turn_completed";
const NEXT_TURN_COMPLETED: &str = "turn_completed";
const KEEP_COMPLETED_WAITING: &str = "The request is complete, but the assistant turn MUST remain active. Required next call: turn_completed. Repeat turn_completed after every timeout or cancellation without user input. Never send a normal ChatGPT final answer or stop merely because time has elapsed or a timeout is approaching.";
const NEXT_CHAT_WRITE: &str = "chat_write";
const NEXT_CHAT_AWAIT: &str = "chat_await";

fn active_workspace(
    config: &AppConfig,
    session: &SessionState,
    context: &ToolRequestContext,
) -> Result<Option<std::path::PathBuf>, String> {
    match context.task_conversation.as_ref() {
        Some(identity) => context
            .project_bindings
            .selected_project_root(config, identity),
        None => match session.binding_state(config)? {
            crate::project_bindings::ProjectBindingState::Project(selection) => {
                Ok(Some(selection.project_root))
            }
            crate::project_bindings::ProjectBindingState::WithoutProject {
                scratch_root, ..
            } => Ok(Some(scratch_root)),
            crate::project_bindings::ProjectBindingState::Unselected { .. } => Ok(None),
        },
    }
}

async fn workspace_transition(
    config: &AppConfig,
    session: &SessionState,
    context: &ToolRequestContext,
    previous: Option<&std::path::Path>,
) -> Result<Option<std::path::PathBuf>, String> {
    loop {
        let current = active_workspace(config, session, context)?;
        if current.as_deref() != previous {
            return Ok(current);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn workspace_wait_result(status: &str, root: Option<&std::path::Path>) -> ToolResult {
    let (content, next) = match root {
        Some(root) => (format!("The user selected workspace {}. Call get_agent_brief now to load this workspace's instructions, environment and saved state before doing any project work.", root.display()), "get_agent_brief"),
        None => ("No workspace is selected. Keep the setup project picker visible and wait with chat_await. A greeting or missing project context is not a request for scratch; do not call set_project_root unless the user explicitly chooses a workspace.".into(), NEXT_CHAT_AWAIT),
    };
    ToolResult::text(content.clone()).with_structured(json!({
        "status":status,"content":content,"chat_file":Value::Null,"notification":"not_configured",
        "required_next_action":next,"assistant_turn_may_end":false
    }))
}

#[derive(Clone, Copy)]
pub enum ChatTool {
    Read,
    Write,
    Await,
    Completed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    message: String,
    #[serde(default)]
    event: WriteEvent,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WriteEvent {
    #[default]
    Message,
    Completed,
    InputRequired,
}

impl From<WriteEvent> for notification::NotificationEvent {
    fn from(event: WriteEvent) -> Self {
        match event {
            WriteEvent::Message => Self::Message,
            WriteEvent::Completed => Self::Completed,
            WriteEvent::InputRequired => Self::InputRequired,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

fn result(
    status: &str,
    content: String,
    path: &std::path::Path,
    notification: NotificationState,
    required_next_action: &'static str,
) -> ToolResult {
    let next = match required_next_action {
        NEXT_CHAT_WRITE => {
            "Required next call: chat_write to acknowledge the new user input, then continue handling it."
        }
        NEXT_CHAT_AWAIT => {
            "Required next call: chat_await for the needed user input; repeat it after timeout or cancellation without input."
        }
        NEXT_TURN_COMPLETED => {
            "Required next call: turn_completed to wait for the next request; repeat it after timeout or cancellation without input."
        }
        _ => {
            "Run the next task tool if work remains; call goal_finish with its report when a goal is completed or blocked; otherwise call turn_completed when finished or chat_await for needed input."
        }
    };
    // A server-issued renewal already contains its explicit protocol override.
    let content = if content.starts_with("TURN RENEWAL REQUIRED.") {
        content
    } else {
        format!(
            "{content} {next} Do not send a normal ChatGPT final answer or end the assistant turn, including near an anticipated timeout."
        )
    };
    ToolResult::text(content.clone()).with_structured(json!({
        "status": status,
        "content": content,
        "chat_file": path.to_string_lossy(),
        "notification": notification,
        "required_next_action": required_next_action,
        "assistant_turn_may_end": false,
    }))
}

fn with_message(mut result: ToolResult, text: String, before_write: bool, end: u64) -> ToolResult {
    if !text.is_empty() {
        if let Some(Value::Object(structured)) = result.structured_content.as_mut() {
            structured.insert(
                "required_next_action".into(),
                Value::String(NEXT_CHAT_WRITE.into()),
            );
        }
        let introduction = if before_write {
            "Before you wrote your message, the user sent the following in CHAT.md. Prioritize answering him ASAP:\n\n"
        } else {
            "The user wrote a new message in CHAT.md. Prioritize answering him ASAP:\n\n"
        };
        result.new_chat_message_from_user = Some(format!("{introduction}{text}"));
        result.chat_delivery_end = Some(end);
    }
    result
}

#[async_trait]
impl Tool for ChatTool {
    fn name(&self) -> &'static str {
        match self {
            Self::Read => "chat_read",
            Self::Write => "chat_write",
            Self::Await => "chat_await",
            Self::Completed => "turn_completed",
        }
    }

    fn title(&self) -> String {
        match self {
            Self::Read => "Read user messages",
            Self::Write => "Send a Markdown message",
            Self::Await => "Wait for needed user input",
            Self::Completed => "Complete the current request and wait for the next",
        }
        .into()
    }

    fn meta(&self) -> Option<rmcp::model::MetaObject> {
        Some(
            serde_json::from_value(json!({"ui":{"visibility":["model"]}}))
                .expect("chat visibility"),
        )
    }

    fn description(&self) -> String {
        match self {
            Self::Read => "NON-TERMINAL TOOL. Read and acknowledge all new user text in this Codexify task's CHAT.md, without truncation. Prefer this over direct file reads. After handling all newly read messages, continue useful work if any remains; otherwise call turn_completed when the request is finished, or chat_await when you need the user to answer a question and MUST NOT end the assistant turn. Whenever the user posts a new message, prioritize answering or acknowledging it with chat_write.",
            Self::Write => "NON-TERMINAL TOOL. Append a Markdown message to this Codexify task's CHAT.md and optionally notify the user through configured notification services. This is the only supported way for the agent to write this file. You MUST NOT end the assistant turn after a successful chat_write, including when asking a question. If new_chat_message_from_user is returned, answer or acknowledge it with chat_write before other work. For a question, use event=message and continue useful work that does not need the answer; when the answer is needed to proceed, use event=input_required and call chat_await repeatedly until the user replies. Do not call turn_completed while this request needs an answer. Otherwise continue useful work if any remains; if finished, your next action MUST be turn_completed. A completion report is still non-terminal, including near an anticipated timeout.",
            Self::Completed => "NON-TERMINAL TOOL. Call only when the current user request is complete. Deliver exactly one oldest eligible queued request, or wait for the next request when none is ready. Queued requests remain private while a goal is unfinished or blocked. Handle the returned request and report using chat_write before completing another request. After every timeout or cancellation without input, your next call MUST be turn_completed again. Never send a normal ChatGPT final answer or end the assistant turn, including near an anticipated timeout. Use chat_await instead when you need the user's answer to continue the current request.",
            Self::Await => "NON-TERMINAL TOOL. Wait only when the current request needs the user's input; this does not complete the request or release queued requests. Use turn_completed when the request is finished. Never substitute a normal assistant final response for chat_await. Wait for new user text in this Codexify task's CHAT.md, then return and acknowledge it in full without truncation. The server config controls the wait deadline; no tool argument can change it. If a user message is returned, answer or acknowledge it with chat_write. After a timeout or cancellation without a user message, call chat_await again unless the user explicitly authorized ending this turn or a higher-priority instruction requires it.",
        }.into()
    }

    fn behavior(&self) -> ToolBehavior {
        match self {
            Self::Write => ToolBehavior::new(
                false,
                false,
                false,
                true,
                "Appends a user-facing message outside the repository and may POST that message to the configured notification provider.",
            ),
            Self::Read | Self::Await | Self::Completed => ToolBehavior::new(
                false,
                false,
                false,
                false,
                "Reads and acknowledges user messages, advancing the conversation's read cursor.",
            ),
        }
    }

    fn input_schema(&self) -> Value {
        if matches!(self, Self::Write) {
            json!({"type":"object", "properties":{"message":{"type":"string", "minLength":1, "writeOnly":true, "description":"Complete Markdown to append and send to the user."},"event":{"type":"string","enum":["message","completed","input_required"],"default":"message","description":"Use completed for a finished chat request, input_required when you must wait for the user's answer, and message for progress or a question while useful work can continue without its answer. Asking a question never ends the assistant turn; continue useful work or call chat_await, and never use completed merely to wait for an answer. input_required holds queued tasks and makes the next user message a direct reply. completed makes one queued task eligible when no goal holds it. After input_required call chat_await; after completed call turn_completed. Neither event ends the assistant turn or changes goal status; use goal_finish for a goal."}}, "required":["message"], "additionalProperties":false})
        } else {
            empty_object_schema()
        }
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type":"object",
            "properties":{
                "status":{"type":"string", "enum":["written","message","empty","timeout","cancelled","continue","workspace_selected","workspace_selection"]},
                "content":{"type":"string"},
                "chat_file":{"type":["string","null"]},
                "notification":{"type":"string", "enum":["not_configured","suppressed","pending","accepted","failed","cancelled"]},
                "required_next_action":{"type":"string", "enum":["continue_or_turn_completed","chat_write","chat_await","turn_completed","get_agent_brief"]},
                "assistant_turn_may_end":{"type":"boolean", "const":false}
            },
            "required":["status","content","chat_file","notification","required_next_action","assistant_turn_may_end"],
            "additionalProperties":false
        }))
    }

    fn manages_model_output_budget(&self) -> bool {
        true
    }

    fn requires_project_root(&self) -> bool {
        !matches!(self, Self::Await | Self::Completed)
    }

    async fn call(&self, _args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        ToolResult::error("Markdown chat requires the current request's conversation context.")
    }

    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        let base_config = config;
        if !config.markdown_chat.enabled {
            return ToolResult::error("Markdown chat is disabled");
        }
        let effective;
        let config = if matches!(self, Self::Await | Self::Completed) {
            if let Err(error) = parse_tool_args::<EmptyArgs>(args.clone()) {
                return *error;
            }
            let root = match active_workspace(config, session, context) {
                Ok(root) => root,
                Err(error) => return ToolResult::error(error),
            };
            let Some(root) = root else {
                return tokio::select! {
                    biased;
                    _ = context.cancellation.cancelled() => workspace_wait_result("cancelled", None),
                    changed = tokio::time::timeout(Duration::from_millis(config.markdown_chat.max_wait_ms), workspace_transition(config, session, context, None)) => match changed {
                        Ok(Ok(root)) => workspace_wait_result("workspace_selected", root.as_deref()),
                        Ok(Err(error)) => ToolResult::error(error),
                        Err(_) => workspace_wait_result("timeout", None),
                    }
                };
            };
            let pending = match &context.task_conversation {
                Some(identity) => context
                    .project_bindings
                    .pending_workspace_change(config, identity),
                None => Ok(session.pending_workspace_change()),
            };
            match pending {
                Ok(Some(_)) => return workspace_wait_result("workspace_selected", Some(&root)),
                Err(error) => return ToolResult::error(error),
                Ok(None) => {}
            }
            effective = {
                let mut selected = config.clone();
                selected.work_dir = root;
                selected
            };
            &effective
        } else {
            config
        };
        let chat =
            match context
                .markdown_chat
                .chat(config, context.task_conversation.as_ref(), session)
            {
                Ok(chat) => chat,
                Err(error) => return ToolResult::error(error),
            };
        if context.cancellation.is_cancelled() {
            let next_action = match self {
                Self::Await => NEXT_CHAT_AWAIT,
                Self::Completed => NEXT_TURN_COMPLETED,
                _ => NEXT_CONTINUE_OR_COMPLETED,
            };
            return result(
                "cancelled",
                "Markdown chat operation was cancelled. Follow required_next_action and do not end the assistant turn.".into(),
                chat.path(),
                NotificationState::Cancelled,
                next_action,
            );
        }
        if matches!(self, Self::Await | Self::Completed)
            && let Some(task) = context.task_conversation.as_ref()
            && let Ok(store) = crate::agent_automation::AgentAutomationStore::for_config(config)
            && let Ok(Some(notice)) =
                store.turn_renewal_notice(task, crate::markdown_chat::now_ms())
        {
            return result(
                "continue",
                notice,
                chat.path(),
                NotificationState::Suppressed,
                NEXT_CONTINUE_OR_COMPLETED,
            );
        }
        if matches!(self, Self::Completed) {
            match chat.queue_goal_holds().await {
                Ok(true) => {
                    return ToolResult::error(
                        "The goal is unfinished. Continue its work and mark it complete with goal_finish before waiting for the next request using turn_completed. If the goal needs the user's input, call chat_await repeatedly; queued requests remain private. Do not send a normal ChatGPT final answer or end the assistant turn.",
                    );
                }
                Err(error) => return ToolResult::error(error),
                Ok(false) => {}
            }
            match chat.queue_turn_completed().await {
                Ok(false) => return result("continue", "A user request has already been delivered for this turn. Handle it and report with chat_write before completing another turn.".into(), chat.path(), NotificationState::Suppressed, NEXT_CHAT_WRITE),
                Err(error) => return ToolResult::error(error),
                Ok(true) => {}
            }
        }
        if matches!(self, Self::Await)
            && let Err(error) = chat.queue_agent_work(false).await
        {
            return ToolResult::error(error);
        }
        if matches!(self, Self::Write) {
            let WriteArgs { message, event } = match parse_tool_args(args) {
                Ok(args) => args,
                Err(error) => return *error,
            };
            let event = notification::NotificationEvent::from(event);
            let mut receipt = match chat.append_with_event(message.clone(), event).await {
                Ok(receipt) => receipt,
                Err(error) => return ToolResult::error(error),
            };
            // A successful reply is progress; observing or acknowledging an
            // inbox is not. Record this before a completed reply releases work.
            if let Err(error) = chat.queue_agent_work(true).await {
                return ToolResult::error(format!(
                    "The message was appended, but its request progress could not be saved: {error}. Do not append it again."
                ));
            }
            let mut next_action = NEXT_CONTINUE_OR_COMPLETED;
            if !receipt.user_text.is_empty()
                && let Err(error) = chat.queue_reply_received().await
            {
                tracing::warn!(%error, "could not clear the answered input request");
            }
            if event == notification::NotificationEvent::InputRequired
                && receipt.user_text.is_empty()
            {
                // A question holds the queue before the following chat_await is called.
                if let Err(error) = chat.queue_agent_work(false).await {
                    return ToolResult::error(format!(
                        "The message was appended, but its input-wait state could not be saved: {error}. Do not append it again."
                    ));
                }
                next_action = NEXT_CHAT_AWAIT;
            }
            let mut returned_end = receipt.end_offset;
            let mut sent_before_write = true;
            if event == notification::NotificationEvent::Completed && receipt.user_text.is_empty() {
                let next = async {
                    if !chat.queue_goal_holds().await? && chat.queue_turn_completed().await? {
                        next_action = NEXT_TURN_COMPLETED;
                        chat.release_queued_message(crate::markdown_chat::now_ms())
                            .await?;
                        let snapshot = chat.read(true).await?;
                        return Ok::<_, String>(Some(snapshot));
                    }
                    Ok(None)
                }
                .await;
                match next {
                    Ok(Some(snapshot)) => {
                        sent_before_write = false;
                        receipt.user_text = snapshot.text;
                        returned_end = snapshot.end;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error, "completed request did not release a queued message")
                    }
                }
            }
            let notify = notification::enabled_for_chat(&config.markdown_chat, event, &chat).await;
            let claimed = if notify && event == notification::NotificationEvent::InputRequired {
                match chat.claim_input_notification().await {
                    Ok(claimed) => claimed,
                    Err(error) => {
                        tracing::warn!(%error, "could not claim input notification");
                        false
                    }
                }
            } else {
                notify
            };
            let state = if claimed {
                let _ = chat
                    .set_notification(receipt.end_offset, NotificationState::Pending)
                    .await;
                let workspace = config
                    .work_dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                notification::publish_event(
                    &config.markdown_chat,
                    event,
                    &workspace,
                    message,
                    &context.cancellation,
                )
                .await
            } else if config.markdown_chat.notifications.is_some() {
                NotificationState::Suppressed
            } else {
                NotificationState::NotConfigured
            };
            let notification_warning = chat.set_notification(receipt.end_offset, state).await.err();
            let mut content = format!(
                "Message appended to CHAT.md. {}",
                notification::description(state)
            );
            if receipt.user_text.is_empty() {
                if event == notification::NotificationEvent::InputRequired {
                    content.push_str(" Your question does not complete the request or end the assistant turn. Call chat_await now for the needed answer, then repeat it after timeouts without input.");
                } else if event == notification::NotificationEvent::Message {
                    content.push_str(" If you asked a question, continue useful work that does not need the answer; when the answer is needed to proceed, call chat_await. Asking a question never authorizes ending the assistant turn or calling turn_completed while the request needs that answer.");
                }
            }
            if let Some(warning) = receipt.cursor_warning.or(notification_warning) {
                content.push_str(&format!(" The append succeeded, but cursor/notification persistence failed: {warning}. Do not send the same message again merely to retry persistence."));
            }
            if !receipt.user_text.is_empty() {
                next_action = NEXT_CHAT_WRITE;
            }
            return with_message(
                result("written", content, chat.path(), state, next_action),
                receipt.user_text,
                sent_before_write,
                returned_end,
            );
        }
        if let Err(error) = parse_tool_args::<EmptyArgs>(args) {
            return *error;
        }
        if matches!(self, Self::Await) {
            let workspace = config
                .work_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            if let Err(error) =
                notification::notify_input(&chat, &config.markdown_chat, &workspace).await
            {
                tracing::warn!(%error, "could not notify pending chat input");
            }
        }
        match self {
            Self::Read => match chat
                .read_cancellable(true, context.cancellation.clone())
                .await
            {
                Ok(snapshot) => {
                    if !snapshot.text.is_empty()
                        && let Err(error) = chat.queue_reply_received().await
                    {
                        tracing::warn!(%error, "could not clear the answered input request");
                    }
                    let (status, content, next_action) = if snapshot.text.is_empty() {
                        (
                            "empty",
                            "The user has not written in CHAT.md since the last chat operation. Continue useful work if any remains; otherwise call turn_completed when finished, or chat_await if user input is required. Do not end the assistant turn.".to_string(),
                            NEXT_CONTINUE_OR_COMPLETED,
                        )
                    } else {
                        (
                            "message",
                            "New user text is included in new_chat_message_from_user. Read it before continuing and answer or acknowledge it ASAP using chat_write.".to_string(),
                            NEXT_CHAT_WRITE,
                        )
                    };
                    with_message(
                        result(
                            status,
                            content,
                            chat.path(),
                            snapshot.notification,
                            next_action,
                        ),
                        snapshot.text,
                        false,
                        snapshot.end,
                    )
                }
                Err(error) => ToolResult::error(error),
            },
            Self::Await | Self::Completed => match tokio::select! {
                outcome = async { if matches!(self, Self::Completed) { chat.wait_completed(Duration::from_millis(config.markdown_chat.max_wait_ms), context.cancellation.clone()).await } else { chat.wait(Duration::from_millis(config.markdown_chat.max_wait_ms), context.cancellation.clone()).await } } => outcome,
                changed = workspace_transition(base_config, session, context, Some(&config.work_dir)) => {
                    return match changed {
                        Ok(root) => workspace_wait_result(if root.is_some() { "workspace_selected" } else { "workspace_selection" }, root.as_deref()),
                        Err(error) => ToolResult::error(error),
                    };
                }
            } {
                Ok(WaitOutcome::Message(snapshot)) => {
                    if let Err(error) = chat.queue_reply_received().await {
                        tracing::warn!(%error, "could not clear the answered input request");
                    }
                    with_message(
                    result(
                        "message",
                        "The user replied. Read new_chat_message_from_user before continuing and answer or acknowledge it ASAP with chat_write.".into(),
                        chat.path(),
                        snapshot.notification,
                        NEXT_CHAT_WRITE,
                    ),
                    snapshot.text,
                    false,
                    snapshot.end,
                )
                }
                Ok(WaitOutcome::TimedOut(state)) => result(
                    "timeout",
                    if matches!(self, Self::Completed) {
                        KEEP_COMPLETED_WAITING.into()
                    } else {
                        format!(
                            "The user has not replied yet. {} {KEEP_WAITING}",
                            notification::description(state)
                        )
                    },
                    chat.path(),
                    state,
                    if matches!(self, Self::Completed) {
                        NEXT_TURN_COMPLETED
                    } else {
                        NEXT_CHAT_AWAIT
                    },
                ),
                Ok(WaitOutcome::Cancelled) => result(
                    "cancelled",
                    if matches!(self, Self::Completed) {
                        KEEP_COMPLETED_WAITING.into()
                    } else {
                        KEEP_WAITING.into()
                    },
                    chat.path(),
                    NotificationState::Cancelled,
                    if matches!(self, Self::Completed) {
                        NEXT_TURN_COMPLETED
                    } else {
                        NEXT_CHAT_AWAIT
                    },
                ),
                Err(error) => ToolResult::error(error),
            },
            Self::Write => unreachable!(),
        }
    }
}
