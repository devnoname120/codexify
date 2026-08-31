use async_trait::async_trait;
use rmcp::model::MetaObject;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolBehavior, empty_object_schema};
use crate::types::{AppConfig, ToolResult};

pub struct Doctor;

impl Doctor {
    fn output_schema_value() -> Value {
        json!({
            "type": "object",
            "properties": {
                "ok": { "type": "boolean" },
                "version": { "type": "string" },
                "platform": {
                    "type": "object",
                    "properties": {
                        "os": { "type": "string" },
                        "arch": { "type": "string" }
                    },
                    "required": ["os", "arch"],
                    "additionalProperties": false
                },
                "checks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pass", "warning", "failure", "skipped"]
                            },
                            "summary": { "type": "string" },
                            "detail": {
                                "anyOf": [
                                    { "type": "string" },
                                    { "type": "null" }
                                ]
                            },
                            "remediation": {
                                "anyOf": [
                                    { "type": "string" },
                                    { "type": "null" }
                                ]
                            }
                        },
                        "required": ["id", "status", "summary", "detail", "remediation"],
                        "additionalProperties": false
                    }
                },
                "summary": {
                    "type": "object",
                    "properties": {
                        "passed": { "type": "integer", "minimum": 0 },
                        "warnings": { "type": "integer", "minimum": 0 },
                        "failures": { "type": "integer", "minimum": 0 },
                        "skipped": { "type": "integer", "minimum": 0 }
                    },
                    "required": ["passed", "warnings", "failures", "skipped"],
                    "additionalProperties": false
                }
            },
            "required": ["ok", "version", "platform", "checks", "summary"],
            "additionalProperties": false
        })
    }
}

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
        Some(Self::output_schema_value())
    }

    fn fills_structured_content(&self) -> bool {
        false
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, _args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let report = crate::doctor::run_for_config(config).await;
        let text = report.render_human();
        let structured =
            serde_json::to_value(&report).expect("doctor report must serialize as structured data");
        ToolResult::text(text).with_structured(structured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{DoctorCheck, DoctorReport};

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

    #[test]
    fn doctor_schema_accepts_the_structured_report_shape() {
        let report = DoctorReport::new(vec![
            DoctorCheck::pass("runtime", "Runtime is usable"),
            DoctorCheck::warning("release", "Release check unavailable")
                .with_detail("offline")
                .with_remediation("Try again later"),
            DoctorCheck::failure("service", "Service is stopped"),
            DoctorCheck::skipped("tunnel", "Tunnel is not configured"),
        ]);
        let structured = serde_json::to_value(report).unwrap();
        let schema = Doctor.output_schema().unwrap();
        let validator = jsonschema::options().build(&schema).unwrap();

        assert!(validator.is_valid(&structured));
        assert!(!Doctor.fills_structured_content());
    }

    #[tokio::test]
    async fn doctor_call_returns_structured_content_matching_its_schema() {
        let root = tempfile::tempdir().unwrap();
        let config = crate::config::default_config(root.path().to_path_buf());
        let result = Doctor.call(json!({}), &config, &SessionState::new()).await;
        let structured = result
            .structured_content
            .as_ref()
            .expect("doctor must return structured content");
        let schema = Doctor.output_schema().unwrap();
        let validator = jsonschema::options().build(&schema).unwrap();

        assert!(validator.is_valid(structured));
        assert!(result.joined_text().starts_with("Codexify doctor "));
    }
}
