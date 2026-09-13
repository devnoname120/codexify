use rmcp::model::{MetaObject, Resource, ResourceContents};
use serde_json::json;

pub const CHAT_UI_URI: &str = "ui://codexify/markdown-chat/v1/mcp-app.html";
pub const CHAT_WIDGET_META: &str = "io.github.devnoname120/codexify/markdown-chat";
pub const CHAT_UI_HTML: &str = include_str!("markdown_chat_ui.html");

pub fn tool_meta() -> MetaObject {
    serde_json::from_value(json!({
        "ui":{"resourceUri":CHAT_UI_URI,"visibility":["model"]},
        "ui/resourceUri":CHAT_UI_URI,
        "openai/outputTemplate":CHAT_UI_URI,
        "openai/toolInvocation/invoking":"Opening Markdown chat",
        "openai/toolInvocation/invoked":"Markdown chat"
    }))
    .expect("chat tool metadata")
}

fn resource_meta() -> MetaObject {
    serde_json::from_value(json!({
        "ui":{"prefersBorder":false,"csp":{"connectDomains":[],"resourceDomains":[]}},
        "openai/widgetPrefersBorder":false,
        "openai/widgetCSP":{"connect_domains":[],"resource_domains":[]},
        "openai/widgetDescription":"A conversation-specific Markdown chat. Send messages without starting a new ChatGPT turn. One grey tick means saved; two blue ticks mean included in an agent-facing tool response, not proof of comprehension."
    })).expect("chat resource metadata")
}

pub fn resource() -> Resource {
    Resource::new(CHAT_UI_URI, "codexify-markdown-chat")
        .with_title("Markdown chat")
        .with_description(
            "This conversation's CHAT.md messages, composer, and agent-delivery receipts",
        )
        .with_mime_type(crate::setup_ui::SETUP_UI_MIME_TYPE)
        .with_size(CHAT_UI_HTML.len() as u64)
        .with_meta(resource_meta())
}

pub fn contents_for_uri(uri: &str) -> Option<ResourceContents> {
    (uri == CHAT_UI_URI).then(|| {
        ResourceContents::text(CHAT_UI_HTML, uri)
            .with_mime_type(crate::setup_ui::SETUP_UI_MIME_TYPE)
            .with_meta(resource_meta())
    })
}
