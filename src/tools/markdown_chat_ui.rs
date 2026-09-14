use async_trait::async_trait;
use rmcp::model::MetaObject;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::markdown_chat_ui::CHAT_WIDGET_META;
use crate::tool::{Tool, ToolBehavior, ToolRequestContext, parse_tool_args, text_output_schema};
use crate::types::{AppConfig, ToolResult};

pub enum ChatUiTool {
    Send,
    State,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    request_id: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateArgs {
    #[serde(default)]
    before: Option<u64>,
    #[serde(default)]
    revision: Option<String>,
}

fn private_result(value: Value) -> ToolResult {
    let mut result = ToolResult::text("Markdown chat widget state updated.");
    result.meta =
        Some(serde_json::from_value(json!({CHAT_WIDGET_META: value})).expect("widget metadata"));
    result.audit.sensitive_output = true;
    result
}

#[async_trait]
impl Tool for ChatUiTool {
    fn name(&self) -> &'static str {
        match self {
            Self::Send => "chat_ui_send",
            Self::State => "chat_ui_state",
        }
    }
    fn title(&self) -> String {
        match self {
            Self::Send => "Send user chat message",
            Self::State => "Read chat widget state",
        }
        .into()
    }
    fn description(&self) -> String {
        match self {
            Self::Send => "App-only: append the user's Markdown to this conversation's CHAT.md. Reusing request_id with the same text is idempotent. Does not acknowledge or deliver text to the agent.",
            Self::State => "App-only: read a page of this conversation's chat history and delivery receipts without acknowledging messages. No arbitrary path or conversation selector is accepted.",
        }.into()
    }
    fn meta(&self) -> Option<MetaObject> {
        Some(serde_json::from_value(json!({
            "ui":{"visibility":["app"]}, "openai/visibility":"private", "openai/widgetAccessible":true
        })).expect("app-only metadata"))
    }
    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            matches!(self, Self::State),
            false,
            true,
            false,
            "The widget reads only its conversation, or idempotently appends a user message to that transcript; it never publishes externally or advances agent delivery.",
        )
    }
    fn input_schema(&self) -> Value {
        match self {
            Self::Send => json!({"type":"object", "properties":{
                "request_id":{"type":"string", "pattern":"^[A-Za-z0-9_-]{1,80}$"},
                "message":{"type":"string", "minLength":1, "writeOnly":true}
            }, "required":["request_id","message"], "additionalProperties":false}),
            Self::State => json!({"type":"object", "properties":{
                "before":{"type":"integer", "minimum":0},
                "revision":{"type":"string", "maxLength":128}
            }, "additionalProperties":false}),
        }
    }
    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }
    async fn call(&self, _: Value, _: &AppConfig, _: &SessionState) -> ToolResult {
        ToolResult::error("Chat widget actions require conversation context.")
    }
    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        if !config.ui_widgets || !config.markdown_chat.enabled {
            return ToolResult::error("The Markdown chat widget is disabled.");
        }
        if context.cancellation.is_cancelled() {
            return ToolResult::error("Chat widget request was cancelled.");
        }
        let chat = match context
            .markdown_chat
            .chat(config, context.conversation.as_ref(), session)
        {
            Ok(chat) => chat,
            Err(error) => return ToolResult::error(error),
        };
        match self {
            Self::Send => {
                let SendArgs {
                    request_id,
                    message,
                } = match parse_tool_args(args) {
                    Ok(args) => args,
                    Err(error) => return *error,
                };
                match chat.append_user(request_id, message).await {
                    Ok(receipt) => private_result(json!({"sent":receipt})),
                    Err(error) => ToolResult::error(error),
                }
            }
            Self::State => {
                let StateArgs { before, revision } = match parse_tool_args(args) {
                    Ok(args) => args,
                    Err(error) => return *error,
                };
                if let Some(at_ms) = context
                    .markdown_chat
                    .last_agent_call(context.conversation.as_ref(), session)
                    && let Err(error) = chat.record_agent_call(at_ms).await
                {
                    return ToolResult::error(error);
                }
                match chat.widget_page(before, revision).await {
                    Ok(page) => private_result(serde_json::to_value(page).expect("chat page")),
                    Err(error) => ToolResult::error(error),
                }
            }
        }
    }
}
