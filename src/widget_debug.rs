use rmcp::model::MetaObject;
use serde_json::json;

use crate::types::{AppConfig, ToolResult};

pub const DEBUG_META_KEY: &str = "io.github.devnoname120/codexify/debug";

pub fn attach_tool_timing(result: &mut ToolResult, tool: &str, duration_ms: u64) {
    let meta = result.meta.get_or_insert_with(MetaObject::new);
    meta.0.insert(
        DEBUG_META_KEY.to_string(),
        json!({
            "tool": tool,
            "durationMs": duration_ms,
            "serverVersion": env!("CARGO_PKG_VERSION")
        }),
    );
}

pub fn attach_configured_tool_timing(
    config: &AppConfig,
    result: &mut ToolResult,
    tool: &str,
    duration_ms: u64,
) {
    if config.debug {
        attach_tool_timing(result, tool, duration_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_metadata_is_hidden_and_preserves_existing_widget_metadata() {
        let mut result = ToolResult::text("ok");
        result.meta = Some(serde_json::from_value(json!({ "existing": true })).unwrap());

        attach_tool_timing(&mut result, "doctor", 42);

        let meta = result.meta.unwrap();
        assert_eq!(meta.get("existing"), Some(&json!(true)));
        assert_eq!(meta.get(DEBUG_META_KEY).unwrap()["tool"], "doctor");
        assert_eq!(meta.get(DEBUG_META_KEY).unwrap()["durationMs"], 42);
        assert_eq!(
            meta.get(DEBUG_META_KEY).unwrap()["serverVersion"],
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn configured_timing_is_present_only_in_debug_mode() {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().to_path_buf());
        let mut result = ToolResult::text("ok");

        attach_configured_tool_timing(&config, &mut result, "setup", 7);
        assert!(result.meta.is_none());

        config.debug = true;
        attach_configured_tool_timing(&config, &mut result, "setup", 7);
        assert_eq!(
            result
                .meta
                .as_ref()
                .and_then(|meta| meta.get(DEBUG_META_KEY))
                .and_then(|value| value.get("durationMs")),
            Some(&json!(7))
        );
    }
}
