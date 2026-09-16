use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::markdown_chat::{NotificationState, WaitOutcome, notification};
use crate::tool::{Tool, ToolBehavior, ToolRequestContext, empty_object_schema, parse_tool_args};
use crate::types::{AppConfig, ToolResult};

pub const KEEP_WAITING: &str = "Markdown chat is still active. The required next action is chat_await. Do not emit a normal assistant final response or end the turn. Call chat_await again unless the user explicitly authorized ending this turn or a higher-priority instruction requires it.";

const NEXT_CONTINUE_OR_AWAIT: &str = "continue_or_chat_await";
const NEXT_CHAT_WRITE: &str = "chat_write";
const NEXT_CHAT_AWAIT: &str = "chat_await";

#[derive(Clone, Copy)]
pub enum ChatTool {
    Read,
    Write,
    Await,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    message: String,
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
        }
    }

    fn title(&self) -> String {
        match self {
            Self::Read => "Read user messages",
            Self::Write => "Send a Markdown message",
            Self::Await => "Wait for user message",
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
            Self::Read => "NON-TERMINAL TOOL. Read and acknowledge all new user text in this conversation's CHAT.md, without truncation. Prefer this over direct file reads. After handling all newly read messages, continue useful work if any remains; otherwise you MUST call chat_await and MUST NOT end the assistant turn. Whenever the user posts a new message, prioritize answering or acknowledging it with chat_write.",
            Self::Write => "NON-TERMINAL TOOL. Append a Markdown message to this conversation's CHAT.md and optionally notify the user through configured notification services. This is the only supported way for the agent to write this file. You MUST NOT end the assistant turn after a successful chat_write. If new_chat_message_from_user is returned, answer or acknowledge it with chat_write before other work. Otherwise continue useful work if any remains; if not, your next action MUST be chat_await. A completion report is still non-terminal.",
            Self::Await => "This is the only valid idle state while Markdown chat is active. Never substitute a normal assistant final response for chat_await. Wait for new user text in this conversation's CHAT.md, then return and acknowledge it in full without truncation. The server config controls the wait deadline; no tool argument can change it. If a user message is returned, answer or acknowledge it with chat_write. After a timeout or cancellation without a user message, call chat_await again unless the user explicitly authorized ending this turn or a higher-priority instruction requires it.",
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
            _ => ToolBehavior::new(
                true,
                false,
                false,
                false,
                "Reads user messages and updates only private read-cursor bookkeeping.",
            ),
        }
    }

    fn input_schema(&self) -> Value {
        if matches!(self, Self::Write) {
            json!({"type":"object", "properties":{"message":{"type":"string", "minLength":1, "writeOnly":true, "description":"Complete Markdown to append and send to the user."}}, "required":["message"], "additionalProperties":false})
        } else {
            empty_object_schema()
        }
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type":"object",
            "properties":{
                "status":{"type":"string", "enum":["written","message","empty","timeout","cancelled"]},
                "content":{"type":"string"},
                "chat_file":{"type":"string"},
                "notification":{"type":"string", "enum":["not_configured","pending","accepted","failed","cancelled"]},
                "required_next_action":{"type":"string", "enum":["continue_or_chat_await","chat_write","chat_await"]},
                "assistant_turn_may_end":{"type":"boolean", "const":false}
            },
            "required":["status","content","chat_file","notification","required_next_action","assistant_turn_may_end"],
            "additionalProperties":false
        }))
    }

    fn manages_model_output_budget(&self) -> bool {
        true
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
        let chat = match context
            .markdown_chat
            .chat(config, context.conversation.as_ref(), session)
        {
            Ok(chat) => chat,
            Err(error) => return ToolResult::error(error),
        };
        if context.cancellation.is_cancelled() {
            let next_action = if matches!(self, Self::Await) {
                NEXT_CHAT_AWAIT
            } else {
                NEXT_CONTINUE_OR_AWAIT
            };
            return result(
                "cancelled",
                "Markdown chat operation was cancelled. Follow required_next_action and do not end the assistant turn.".into(),
                chat.path(),
                NotificationState::Cancelled,
                next_action,
            );
        }
        if matches!(self, Self::Write) {
            let WriteArgs { message } = match parse_tool_args(args) {
                Ok(args) => args,
                Err(error) => return *error,
            };
            let receipt = match chat.append(message.clone()).await {
                Ok(receipt) => receipt,
                Err(error) => return ToolResult::error(error),
            };
            let state = if config.markdown_chat.notifications.is_some() {
                let _ = chat
                    .set_notification(receipt.end_offset, NotificationState::Pending)
                    .await;
                let workspace = config
                    .work_dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                notification::publish_config(
                    &config.markdown_chat,
                    &workspace,
                    message,
                    &context.cancellation,
                )
                .await
            } else {
                NotificationState::NotConfigured
            };
            let notification_warning = chat.set_notification(receipt.end_offset, state).await.err();
            let mut content = format!(
                "Message appended to CHAT.md. {}",
                notification::description(state)
            );
            if let Some(warning) = receipt.cursor_warning.or(notification_warning) {
                content.push_str(&format!(" The append succeeded, but cursor/notification persistence failed: {warning}. Do not send the same message again merely to retry persistence."));
            }
            return with_message(
                result(
                    "written",
                    content,
                    chat.path(),
                    state,
                    NEXT_CONTINUE_OR_AWAIT,
                ),
                receipt.user_text,
                true,
                receipt.end_offset,
            );
        }
        if let Err(error) = parse_tool_args::<EmptyArgs>(args) {
            return *error;
        }
        match self {
            Self::Read => match chat.read_cancellable(true, context.cancellation.clone()).await {
                Ok(snapshot) => {
                    let (status, content, next_action) = if snapshot.text.is_empty() {
                        (
                            "empty",
                            "The user has not written in CHAT.md since the last chat operation. Continue useful work if any remains; otherwise call chat_await. Do not end the assistant turn.".to_string(),
                            NEXT_CONTINUE_OR_AWAIT,
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
            Self::Await => match chat.wait(Duration::from_millis(config.markdown_chat.max_wait_ms), context.cancellation.clone()).await {
                Ok(WaitOutcome::Message(snapshot)) => with_message(
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
                ),
                Ok(WaitOutcome::TimedOut(state)) => result(
                    "timeout",
                    format!(
                        "The user has not replied yet. {} {KEEP_WAITING}",
                        notification::description(state)
                    ),
                    chat.path(),
                    state,
                    NEXT_CHAT_AWAIT,
                ),
                Ok(WaitOutcome::Cancelled) => result(
                    "cancelled",
                    KEEP_WAITING.into(),
                    chat.path(),
                    NotificationState::Cancelled,
                    NEXT_CHAT_AWAIT,
                ),
                Err(error) => ToolResult::error(error),
            },
            Self::Write => unreachable!(),
        }
    }
}
