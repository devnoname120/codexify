use rmcp::model::{MetaObject, Resource, ResourceContents};
use serde_json::json;

pub const SETUP_UI_URI: &str = "ui://codexify/setup/v2/mcp-app.html";
pub const LEGACY_SETUP_UI_URI: &str = "ui://codexify/setup/v1/mcp-app.html";
pub const SETUP_UI_MIME_TYPE: &str = "text/html;profile=mcp-app";

pub fn tool_meta() -> MetaObject {
    serde_json::from_value(json!({
        "ui": {
            "resourceUri": SETUP_UI_URI,
            "visibility": ["model", "app"]
        },
        "ui/resourceUri": SETUP_UI_URI,
        "openai/outputTemplate": SETUP_UI_URI,
        "openai/widgetAccessible": true
    }))
    .expect("static setup tool metadata must be an object")
}

pub fn resource_meta() -> MetaObject {
    serde_json::from_value(json!({
        "ui": {
            "prefersBorder": false,
            "csp": {
                "connectDomains": [],
                "resourceDomains": []
            }
        },
        "openai/widgetPrefersBorder": false,
        "openai/widgetCSP": {
            "connect_domains": [],
            "resource_domains": []
        }
    }))
    .expect("static setup resource metadata must be an object")
}

pub fn resource() -> Resource {
    Resource::new(SETUP_UI_URI, "codexify-setup")
        .with_title("Codexify status")
        .with_description("Codexify setup, update, connector-schema, and diagnostic status")
        .with_mime_type(SETUP_UI_MIME_TYPE)
        .with_size(SETUP_UI_HTML.len() as u64)
        .with_meta(resource_meta())
}

pub fn contents_for_uri(uri: &str) -> Option<ResourceContents> {
    if uri != SETUP_UI_URI && uri != LEGACY_SETUP_UI_URI {
        return None;
    }
    Some(
        ResourceContents::text(SETUP_UI_HTML, uri)
            .with_mime_type(SETUP_UI_MIME_TYPE)
            .with_meta(resource_meta()),
    )
}

pub const SETUP_UI_HTML: &str = include_str!("setup_ui.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_tool_metadata_links_an_app_accessible_widget() {
        let meta = tool_meta();
        assert_eq!(
            meta.get("ui").and_then(|value| value.get("resourceUri")),
            Some(&json!(SETUP_UI_URI))
        );
        assert_eq!(
            meta.get("openai/outputTemplate"),
            Some(&json!(SETUP_UI_URI))
        );
        assert_eq!(meta.get("openai/widgetAccessible"), Some(&json!(true)));
    }

    #[test]
    fn current_and_legacy_setup_resource_uris_are_readable() {
        assert_eq!(SETUP_UI_URI, "ui://codexify/setup/v2/mcp-app.html");
        assert!(contents_for_uri(SETUP_UI_URI).is_some());
        assert!(contents_for_uri(LEGACY_SETUP_UI_URI).is_some());
    }

    #[test]
    fn setup_resource_contains_compact_actions_and_follow_up_prompts() {
        let contents = contents_for_uri(SETUP_UI_URI).unwrap();
        let serialized = serde_json::to_value(contents).unwrap();
        let text = serialized["text"].as_str().unwrap();
        for expected in [
            "tools/call",
            "window.openai.callTool",
            "check_for_updates",
            "self_update",
            "doctor",
            "ui/message",
            "window.openai.sendFollowUpMessage",
            "Check for updates",
            "Upgrade",
            "Refresh",
            "Autofix",
            "plugin://dev-<slug>@...",
            "#settings/Plugins/plugin_asdk_app_<slug>",
            "#settings/Plugins",
            "scroll below the list of tools",
            "Doctor returned no structured report",
            "status-pass",
            "status-warning",
            "status-failure",
            "status-skipped",
            "io.github.devnoname120/codexify/debug",
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
        assert!(!text.contains("data.nextStep"));
        assert!(!text.contains("Connector status and diagnostics"));
        assert!(!text.contains("openExternal"));
        assert!(!text.contains("connectorSettingsUrl"));
        assert!(!text.contains("connectorPluginId"));
        assert!(text.contains("max-width: 500px"));
        assert!(contents_for_uri("ui://codexify/setup/unknown").is_none());
    }
}
