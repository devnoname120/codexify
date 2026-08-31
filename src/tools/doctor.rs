use async_trait::async_trait;
use rmcp::model::MetaObject;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolBehavior, empty_object_schema, text_output_schema};
use crate::types::{AppConfig, ToolResult};

pub struct Doctor;

#[async_trait]
impl Tool for Doctor {
    fn name(&self) -> &'static str {
        "doctor"
    }

    fn title(&self) -> String {
        "Run Codexify doctor".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            true,
            "Runs read-only local diagnostics and bounded release/health probes without changing configuration, services, projects, or external state.",
        )
    }

    fn meta(&self) -> Option<MetaObject> {
        Some(
            serde_json::from_value(json!({
                "ui": { "visibility": ["app"] },
                "openai/visibility": "private",
                "openai/widgetAccessible": true,
                "openai/toolInvocation/invoking": "Running Codexify doctor",
                "openai/toolInvocation/invoked": "Codexify doctor finished"
            }))
            .expect("static doctor tool metadata must be an object"),
        )
    }

    fn description(&self) -> String {
        "Run the same read-only diagnostic engine as `codexify doctor` against the running server configuration. This action is available to Codexify widgets and intentionally hidden from the model.".to_string()
    }

    fn input_schema(&self) -> Value {
        empty_object_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, _args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        ToolResult::text(crate::doctor::run_for_config(config).await.render_human())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_is_read_only_and_app_only() {
        let behavior = Doctor.behavior();
        assert!(behavior.read_only);
        assert!(!behavior.destructive);
        assert!(behavior.idempotent);
        assert!(behavior.open_world);
        assert!(!Doctor.requires_project_root());

        let meta = Doctor.meta().unwrap();
        assert_eq!(
            meta.get("ui").and_then(|value| value.get("visibility")),
            Some(&json!(["app"]))
        );
        assert_eq!(meta.get("openai/visibility"), Some(&json!("private")));
        assert_eq!(meta.get("openai/widgetAccessible"), Some(&json!(true)));
    }
}
