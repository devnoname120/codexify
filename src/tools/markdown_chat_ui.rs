use async_trait::async_trait;
use rmcp::model::MetaObject;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::markdown_chat_ui::CHAT_WIDGET_META;
use crate::tool::{Tool, ToolBehavior, ToolRequestContext, parse_tool_args, text_output_schema};
use crate::types::{AppConfig, ToolContent, ToolResult};

pub enum ChatUiTool {
    Send,
    State,
    File,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileArgs {
    href: String,
    expected_workspace: Option<String>,
    expected_chat_file: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    request_id: String,
    message: String,
    expected_workspace: Option<String>,
    expected_chat_file: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateArgs {
    #[serde(default)]
    before: Option<u64>,
    #[serde(default)]
    revision: Option<String>,
    expected_workspace: Option<String>,
    expected_chat_file: Option<String>,
}

fn check_destination(
    config: &AppConfig,
    chat_file: &std::path::Path,
    expected_workspace: Option<&str>,
    expected_chat_file: Option<&str>,
    requires_destination: bool,
) -> Result<(), Box<ToolResult>> {
    let missing = config.multi_project && requires_destination && expected_chat_file.is_none();
    let changed = expected_workspace
        .is_some_and(|path| std::path::Path::new(path) != config.work_dir)
        || expected_chat_file.is_some_and(|path| std::path::Path::new(path) != chat_file);
    if !missing && !changed {
        return Ok(());
    }
    let mut result = ToolResult::error(if missing {
        "Reload the chat widget before sending or downloading: it must identify the transcript currently displayed. No message was appended or file resolved."
    } else {
        "Workspace changed. Refresh the selected workspace before retrying. No message was appended or file resolved."
    });
    result.meta = Some(
        serde_json::from_value(json!({CHAT_WIDGET_META:{"workspace_changed":true}}))
            .expect("chat context metadata"),
    );
    result.audit.sensitive_output = true;
    Err(Box::new(result))
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
            Self::File => "chat_ui_file",
        }
    }
    fn title(&self) -> String {
        match self {
            Self::Send => "Send user chat message",
            Self::State => "Read chat widget state",
            Self::File => "Download referenced chat file",
        }
        .into()
    }
    fn description(&self) -> String {
        match self {
            Self::Send => "App-only: append the user's Markdown to this conversation's CHAT.md. Reusing request_id with the same text is idempotent. Does not acknowledge or deliver text to the agent.",
            Self::State => "App-only: read a page of this conversation's chat history and delivery receipts without acknowledging messages. No arbitrary path or conversation selector is accepted.",
            Self::File => "App-only: resolve an exported or project-relative Markdown file link in the active workspace for a user-requested download. Sandbox names must match an unambiguous prior export. Does not read or acknowledge chat messages.",
        }.into()
    }
    fn meta(&self) -> Option<MetaObject> {
        Some(serde_json::from_value(json!({
            "ui":{"visibility":["app"]}, "openai/visibility":"private", "openai/widgetAccessible":true
        })).expect("app-only metadata"))
    }
    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            !matches!(self, Self::Send),
            false,
            true,
            false,
            "The widget reads its conversation or an active-workspace file, or idempotently appends a user message; it never publishes externally or advances agent delivery.",
        )
    }
    fn input_schema(&self) -> Value {
        let mut schema = match self {
            Self::Send => json!({"type":"object", "properties":{
                "request_id":{"type":"string", "pattern":"^[A-Za-z0-9_-]{1,80}$"},
                "message":{"type":"string", "minLength":1, "writeOnly":true}
            }, "required":["request_id","message"], "additionalProperties":false}),
            Self::State => json!({"type":"object", "properties":{
                "before":{"type":"integer", "minimum":0},
                "revision":{"type":"string", "maxLength":128}
            }, "additionalProperties":false}),
            Self::File => {
                json!({"type":"object", "properties":{"href":{"type":"string", "minLength":1, "maxLength":4096}}, "required":["href"], "additionalProperties":false})
            }
        };
        for (name, description) in [
            (
                "expected_workspace",
                "The selected workspace displayed by setup. Compared with the server-resolved workspace; never used to select a path.",
            ),
            (
                "expected_chat_file",
                "The chat_file returned by the displayed chat state. Required for sends and file actions in multi-project mode; never used to select a transcript.",
            ),
        ] {
            schema["properties"][name] =
                json!({"type":"string","minLength":1,"description":description});
        }
        schema
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
            Self::File => {
                let FileArgs {
                    href,
                    expected_workspace,
                    expected_chat_file,
                } = match parse_tool_args(args) {
                    Ok(args) => args,
                    Err(error) => return *error,
                };
                if let Err(error) = check_destination(
                    config,
                    chat.path(),
                    expected_workspace.as_deref(),
                    expected_chat_file.as_deref(),
                    true,
                ) {
                    return *error;
                }
                match context
                    .artifact_egress
                    .chat_file_link(&config.work_dir, &href, &context.cancellation)
                    .await
                {
                    Ok(resource) => {
                        let mut file = serde_json::to_value(&resource).expect("file resource");
                        file["type"] = json!("resource_link");
                        let mut result = private_result(json!({"file":file}));
                        result.content.push(ToolContent::ResourceLink(resource));
                        result
                    }
                    Err(error) => ToolResult::error(error.to_string()),
                }
            }
            Self::Send => {
                let SendArgs {
                    request_id,
                    message,
                    expected_workspace,
                    expected_chat_file,
                } = match parse_tool_args(args) {
                    Ok(args) => args,
                    Err(error) => return *error,
                };
                if let Err(error) = check_destination(
                    config,
                    chat.path(),
                    expected_workspace.as_deref(),
                    expected_chat_file.as_deref(),
                    true,
                ) {
                    return *error;
                }
                match chat.append_user(request_id, message).await {
                    Ok(receipt) => private_result(json!({"sent":receipt})),
                    Err(error) => ToolResult::error(error),
                }
            }
            Self::State => {
                let StateArgs {
                    before,
                    revision,
                    expected_workspace,
                    expected_chat_file,
                } = match parse_tool_args(args) {
                    Ok(args) => args,
                    Err(error) => return *error,
                };
                if let Err(error) = check_destination(
                    config,
                    chat.path(),
                    expected_workspace.as_deref(),
                    expected_chat_file.as_deref(),
                    false,
                ) {
                    return *error;
                }
                if let Some(activity) = context
                    .markdown_chat
                    .agent_activity(context.conversation.as_ref(), session)
                    && let Err(error) = chat.sync_agent_activity(activity).await
                {
                    return ToolResult::error(error);
                }
                match chat.widget_page(before, revision).await {
                    Ok(page) => {
                        let mut page = serde_json::to_value(page).expect("chat page");
                        page["workspace_path"] = json!(config.work_dir);
                        private_result(page)
                    }
                    Err(error) => ToolResult::error(error),
                }
            }
        }
    }
}
