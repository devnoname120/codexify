use rmcp::model::{MetaObject, Resource, ResourceContents};
use serde_json::json;
use std::sync::LazyLock;

pub const CHAT_UI_URI: &str = "ui://codexify/markdown-chat/v2/mcp-app.html";
pub const PREVIOUS_CHAT_UI_URI: &str = "ui://codexify/markdown-chat/v1/mcp-app.html";
pub const SETUP_CHAT_UI_URI: &str = "ui://codexify/setup-chat/v5/mcp-app.html";
pub const PREVIOUS_SETUP_CHAT_UI_URI_V4: &str = "ui://codexify/setup-chat/v4/mcp-app.html";
pub const PREVIOUS_SETUP_CHAT_UI_URI_V3: &str = "ui://codexify/setup-chat/v3/mcp-app.html";
pub const PREVIOUS_SETUP_CHAT_UI_URI: &str = "ui://codexify/setup-chat/v2/mcp-app.html";
pub const LEGACY_SETUP_CHAT_UI_URI: &str = "ui://codexify/setup-chat/v1/mcp-app.html";
pub const CHAT_WIDGET_META: &str = "io.github.devnoname120/codexify/markdown-chat";
pub const CHAT_ENABLED_META: &str = "io.github.devnoname120/codexify/markdown-chat-enabled";
pub static CHAT_UI_HTML: LazyLock<String> = LazyLock::new(|| {
    include_str!("markdown_chat_ui.html")
        .replace(
            "/* CODEXIFY_MARKDOWN_LIBRARY */",
            concat!(
                "/*\n",
                include_str!("vendor/markdown-it.LICENSE"),
                "*/\n",
                include_str!("vendor/markdown-it.min.js")
            ),
        )
        .replace(
            "/* CODEXIFY_MARKDOWN_RENDERER */",
            include_str!("markdown_chat_render.js"),
        )
});

pub fn tool_meta() -> MetaObject {
    let mut meta = crate::setup_ui::tool_meta();
    meta.0.get_mut("ui").expect("setup UI metadata")["resourceUri"] = json!(SETUP_CHAT_UI_URI);
    meta.0
        .insert("ui/resourceUri".into(), json!(SETUP_CHAT_UI_URI));
    meta.0
        .insert("openai/outputTemplate".into(), json!(SETUP_CHAT_UI_URI));
    meta
}

fn section<'a>(source: &'a str, opening: &str, closing: &str) -> &'a str {
    source
        .split_once(opening)
        .expect("embedded HTML opening")
        .1
        .split_once(closing)
        .expect("embedded HTML closing")
        .0
}

pub static SETUP_CHAT_UI_HTML: LazyLock<String> = LazyLock::new(|| {
    let style = section(&CHAT_UI_HTML, "<style>", "</style>")
        .replace(
            ":root:not([data-theme=\"light\"])",
            ":host(:not([data-theme=\"light\"]))",
        )
        .replace(":root[data-theme=\"dark\"]", ":host([data-theme=\"dark\"])")
        .replace(":root", ":host")
        .replace("body {", ":host { display:block;");
    let body = section(&CHAT_UI_HTML, "<body>", "<script>");
    let script = section(&CHAT_UI_HTML, "<script>", "</script>");
    let component = format!(
        "<template id=\"codexify-chat-template\"><style>{style}</style>{body}</template>\n<script>{script}</script>\n<script>"
    );
    crate::setup_ui::SETUP_UI_HTML.replacen("<script>", &component, 1)
});

fn resource_meta() -> MetaObject {
    serde_json::from_value(json!({
        "ui":{"prefersBorder":false,"csp":{"connectDomains":[],"resourceDomains":[]}},
        "openai/widgetPrefersBorder":false,
        "openai/widgetCSP":{"connect_domains":[],"resource_domains":[]},
        "openai/widgetDescription":"Codexify setup with one conversation-specific chat panel. One grey tick means sent, two grey ticks mean returned to the agent, and two blue ticks mean acknowledged by a chat tool. Compact counters show model-visible Codexify tool calls for the conversation and between messages, preserving their position when the user sends. Agent presence reflects the last agent tool call, not a live connection."
    })).expect("chat resource metadata")
}

pub fn resource() -> Resource {
    Resource::new(SETUP_CHAT_UI_URI, "codexify-setup-chat")
        .with_title("Codexify setup and Markdown chat")
        .with_description(
            "This conversation's CHAT.md messages, composer, and agent-delivery receipts",
        )
        .with_mime_type(crate::setup_ui::SETUP_UI_MIME_TYPE)
        .with_size(SETUP_CHAT_UI_HTML.len() as u64)
        .with_meta(resource_meta())
}

pub fn contents_for_uri(uri: &str) -> Option<ResourceContents> {
    let html = match uri {
        SETUP_CHAT_UI_URI
        | PREVIOUS_SETUP_CHAT_UI_URI_V4
        | PREVIOUS_SETUP_CHAT_UI_URI_V3
        | PREVIOUS_SETUP_CHAT_UI_URI
        | LEGACY_SETUP_CHAT_UI_URI => SETUP_CHAT_UI_HTML.as_str(),
        CHAT_UI_URI | PREVIOUS_CHAT_UI_URI => CHAT_UI_HTML.as_str(),
        _ => return None,
    };
    Some(
        ResourceContents::text(html, uri)
            .with_mime_type(crate::setup_ui::SETUP_UI_MIME_TYPE)
            .with_meta(resource_meta()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_chat_resource_embeds_one_panel_and_retains_the_old_uri() {
        let contents = serde_json::to_value(contents_for_uri(SETUP_CHAT_UI_URI).unwrap()).unwrap();
        let html = contents["text"].as_str().unwrap();
        assert_eq!(html, SETUP_CHAT_UI_HTML.as_str());
        assert_eq!(html.matches("id=\"chat\"").count(), 1);
        assert_eq!(html.matches("id=\"codexify-chat-template\"").count(), 1);
        assert!(html.contains(":host([hidden])"));
        assert!(html.contains("root.after(chatHost)"));
        assert!(html.contains("setup_ui_select_project"));
        assert!(html.contains("Read by agent"));
        assert!(html.contains(&format!(
            "if (age < {}) return {{ state:\"away\"",
            crate::markdown_chat::OFFLINE_AFTER_MS
        )));
        assert!(html.contains("id=\"tool-total\""));
        assert!(html.contains("tool-call-marker"));
        assert_eq!(resource().uri, SETUP_CHAT_UI_URI);
        assert_eq!(
            tool_meta().get("openai/outputTemplate"),
            Some(&json!(SETUP_CHAT_UI_URI))
        );
        assert!(contents_for_uri(CHAT_UI_URI).is_some());
        assert!(contents_for_uri(PREVIOUS_CHAT_UI_URI).is_some());
        assert!(contents_for_uri(PREVIOUS_SETUP_CHAT_UI_URI).is_some());
        assert!(contents_for_uri(PREVIOUS_SETUP_CHAT_UI_URI_V3).is_some());
        assert!(contents_for_uri(PREVIOUS_SETUP_CHAT_UI_URI_V4).is_some());
        assert!(contents_for_uri(LEGACY_SETUP_CHAT_UI_URI).is_some());
        assert!(contents_for_uri("ui://codexify/unrelated").is_none());
        if let Some(path) = std::env::var_os("CODEXIFY_CHAT_PREVIEW_HTML") {
            std::fs::write(path, html).unwrap();
        }
    }
}
