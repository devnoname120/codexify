use rmcp::model::{MetaObject, Resource, ResourceContents};
use serde_json::json;

pub const SETUP_UI_URI: &str = "ui://codexify/setup/v3/mcp-app.html";
pub const PREVIOUS_SETUP_UI_URI: &str = "ui://codexify/setup/v2/mcp-app.html";
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

pub fn app_callable_tool_meta() -> MetaObject {
    serde_json::from_value(json!({
        "ui": { "visibility": ["model", "app"] },
        "openai/widgetAccessible": true
    }))
    .expect("static setup app-callable tool metadata must be an object")
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
        .with_description(
            "Codexify workspace selection, setup, update, connector-schema, and diagnostic status",
        )
        .with_mime_type(SETUP_UI_MIME_TYPE)
        .with_size(SETUP_UI_HTML.len() as u64)
        .with_meta(resource_meta())
}

pub fn contents_for_uri(uri: &str) -> Option<ResourceContents> {
    if uri != SETUP_UI_URI && uri != PREVIOUS_SETUP_UI_URI && uri != LEGACY_SETUP_UI_URI {
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
        assert_eq!(SETUP_UI_URI, "ui://codexify/setup/v3/mcp-app.html");
        assert!(contents_for_uri(PREVIOUS_SETUP_UI_URI).is_some());
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
            "ui/open-link",
            "window.openai.openExternal",
            "Check for updates",
            "Upgrade",
            "Refresh",
            "Autofix",
            "web-sandbox\\.oaiusercontent\\.com",
            "plugin_asdk_app_",
            "Information-,Refresh,-Connected",
            "scroll below the list of tools",
            "Doctor returned no structured report",
            "status-pass",
            "status-warning",
            "status-failure",
            "status-skipped",
            "io.github.devnoname120/codexify/debug",
            "Chat without a project",
            "project-search",
            "list_projects",
            "set_project_root",
            "withoutProject",
            "active_root",
            "source_project_root",
            "managed_worktree",
            "Aliases:",
            "Worktree",
            "Scratch",
            "projectQueryGeneration",
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
        let picker = &text[text.find("const scratch").unwrap()..];
        assert!(
            picker.find("Chat without a project").unwrap() < picker.find("const search").unwrap(),
            "the projectless option must be constructed before the search input"
        );
        assert!(picker.contains("picker.append(scratch, search"));
        let search_handler = &text[text.find("search.addEventListener(\"input\"").unwrap()..];
        assert!(
            search_handler.find("projectQueryGeneration += 1").unwrap()
                < search_handler.find("setTimeout").unwrap(),
            "editing the query must invalidate an in-flight response before the debounce fires"
        );
        assert!(!text.contains("data.nextStep"));
        assert!(!text.contains("Connector status and diagnostics"));
        assert!(!text.contains("REFRESH_PROMPT"));
        assert!(!text.contains("sendRefreshPrompt"));
        assert!(!text.contains("document.referrer"));
        assert!(!text.contains("chatgptReferrer"));
        assert!(!text.contains("connectorSettingsUrl"));
        assert!(!text.contains("connectorPluginId"));
        assert!(!text.contains("innerHTML"));
        let open_link = &text[text.find("async function openLink").unwrap()
            ..text.find("function connectorSlug").unwrap()];
        assert!(
            open_link.find("window.openai.openExternal").unwrap()
                < open_link.find("ui/open-link").unwrap(),
            "Refresh must prefer ChatGPT openExternal for relative settings hashes"
        );
        assert!(text.contains("max-width: 500px"));
        assert!(contents_for_uri("ui://codexify/setup/unknown").is_none());
    }
}
