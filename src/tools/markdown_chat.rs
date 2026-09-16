use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::markdown_chat::{NotificationState, WaitOutcome, notification};
use crate::tool::{Tool, ToolBehavior, ToolRequestContext, empty_object_schema, parse_tool_args};
use crate::types::{AppConfig, ToolResult};

pub const KEEP_WAITING: &str = "Call chat_await again and keep waiting for the user's reply. Do not finish your turn, stop, complete, or checkpoint the turn under any circumstances. It is strictly forbidden, you will be stopped externally if needed so you should never stop. Use chat_write for questions and reports and chat_await if you are blocked and need a user answer before you can continue. If you are done with the current task then you should call chat_write to inform the user about the completion of the task then immediately after run chat_await to receive new instructions instead of ending the turn. This is an absolute rule, you are NOT allowed to stop, complete, finish, or checkpoint your turn. Instead you ALWAYS HAVE to call chat_await whenever you are done, never finish your turn.";

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
) -> ToolResult {
    ToolResult::text(content.clone()).with_structured(json!({
        "status": status,
        "content": content,
        "chat_file": path.to_string_lossy(),
        "notification": notification,
    }))
}

fn with_message(mut result: ToolResult, text: String, before_write: bool, end: u64) -> ToolResult {
    if !text.is_empty() {
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
            Self::Read => "Read and acknowledge all new user text in this conversation's CHAT.md, without truncation. Prefer this over direct file reads. If there is no message, continue useful work; when blocked or out of work, send a message with chat_write then call chat_await instead of ending the turn (ending, stopping, and checkpointing the turn are strictly forbidden). Whenever the user posts a new message your top priority is to use chat_write to either answer him (if it's a question) or acknowledge what he said (if it’s a statement or instructions).",
            Self::Write => "Append a Markdown message to this conversation's CHAT.md and optionally notify the user through configured notification services. This is the only supported way for the agent to write this file. Returns any complete unread user text that arrived before the append; read that text before continuing. Use this for questions, progress and completion reports instead of ChatGPT replies.",
            Self::Await => "Wait for new user text in this conversation's CHAT.md, then return and acknowledge it in full without truncation. The server config controls the wait deadline; no tool argument can change it. If you are blocked again or still blocked or the user hasn’t answered yet then you have to call chat_await again: you are NOT allowed to finish, stop, or checkpoint your turn under no circumstances. Finishing, stopping or checkpointing the turn is absolutely forbidden, you have to call chat_await over and over until the user provides an answer, you are not allowed to stop without the user’s explicit authorization.",
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
                "notification":{"type":"string", "enum":["not_configured","pending","accepted","failed","cancelled"]}
            },
            "required":["status","content","chat_file","notification"],
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
            return result("cancelled", "Markdown chat operation was cancelled. If it was a chat_await operation then run it again.".into(), chat.path(), NotificationState::Cancelled);
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
                result("written", content, chat.path(), state),
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
                    let (status, content) = if snapshot.text.is_empty() {
                        ("empty", "The user has not written in CHAT.md since the last chat operation. Continue useful work and check again later. When blocked or out of work, use chat_write followed by chat_await instead of ending the turn.".to_string())
                    } else {
                        ("message", "New user text is included in new_chat_message_from_user. Read it before continuing and prioritize answering it ASAP using the chat_write tool.".to_string())
                    };
                    with_message(result(status, content, chat.path(), snapshot.notification), snapshot.text, false, snapshot.end)
                }
                Err(error) => ToolResult::error(error),
            },
            Self::Await => match chat.wait(Duration::from_millis(config.markdown_chat.max_wait_ms), context.cancellation.clone()).await {
                Ok(WaitOutcome::Message(snapshot)) => with_message(result("message", "The user replied. Read new_chat_message_from_user before continuing and prioritize replying him ASAP with chat_write.".into(), chat.path(), snapshot.notification), snapshot.text, false, snapshot.end),
                Ok(WaitOutcome::TimedOut(state)) => result("timeout", format!("The user has not replied yet. {} {KEEP_WAITING}", notification::description(state)), chat.path(), state),
                Ok(WaitOutcome::Cancelled) => result("cancelled", "Call chat_await again and again until you receive chat from the user. Never finish or stop your turn under any circumstances".into(), chat.path(), NotificationState::Cancelled),
                Err(error) => ToolResult::error(error),
            },
            Self::Write => unreachable!(),
        }
    }
}
