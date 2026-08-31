use async_trait::async_trait;
use rmcp::model::MetaObject;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::self_update::{UpdateStatusRecord, read_update_status, valid_update_id};
use crate::tool::{Tool, ToolBehavior, parse_tool_args};
use crate::types::{AppConfig, ToolResult};

pub struct SelfUpdateStatus;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SelfUpdateStatusArgs {
    update_id: String,
}

impl SelfUpdateStatus {
    fn output_schema_value() -> Value {
        json!({
            "type": "object",
            "properties": {
                "updateId": {
                    "type": "string",
                    "minLength": 24,
                    "maxLength": 24,
                    "pattern": "^[0-9a-f]{24}$"
                },
                "fromVersion": { "type": "string" },
                "targetVersion": { "type": "string" },
                "state": {
                    "type": "string",
                    "enum": [
                        "scheduled",
                        "installing",
                        "validating",
                        "restarting",
                        "succeeded",
                        "failed",
                        "rolled_back"
                    ]
                },
                "updatedAt": { "type": "string", "format": "date-time" },
                "failureCode": {
                    "anyOf": [
                        { "type": "string", "maxLength": 64 },
                        { "type": "null" }
                    ]
                },
                "failureDetail": {
                    "anyOf": [
                        { "type": "string", "maxLength": 512 },
                        { "type": "null" }
                    ]
                },
                "runningVersion": { "type": "string" }
            },
            "required": [
                "updateId",
                "fromVersion",
                "targetVersion",
                "state",
                "updatedAt",
                "failureCode",
                "failureDetail",
                "runningVersion"
            ],
            "additionalProperties": false
        })
    }

    fn result(record: UpdateStatusRecord) -> ToolResult {
        let mut structured = serde_json::to_value(&record)
            .expect("validated update status records must serialize as objects");
        structured
            .as_object_mut()
            .expect("update status record serialization must be an object")
            .insert(
                "runningVersion".to_string(),
                Value::String(env!("CARGO_PKG_VERSION").to_string()),
            );
        ToolResult::text("Codexify update status read.").with_structured(structured)
    }
}

#[async_trait]
impl Tool for SelfUpdateStatus {
    fn name(&self) -> &'static str {
        "self_update_status"
    }

    fn title(&self) -> String {
        "Read update status".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Reads one private Codexify updater record without modifying files, project state, or external systems.",
        )
    }

    fn meta(&self) -> Option<MetaObject> {
        Some(
            serde_json::from_value(json!({
                "ui": { "visibility": ["app"] },
                "openai/visibility": "private",
                "openai/widgetAccessible": true
            }))
            .expect("static self-update status metadata must be an object"),
        )
    }

    fn description(&self) -> String {
        "Read one durable Codexify self-update record for the updater component. This app-only tool is not intended for model invocation."
            .to_string()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "updateId": {
                    "type": "string",
                    "minLength": 24,
                    "maxLength": 24,
                    "pattern": "^[0-9a-f]{24}$",
                    "description": "Opaque identifier returned by self_update."
                }
            },
            "required": ["updateId"],
            "additionalProperties": false
        })
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

    async fn call(&self, args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        let args: SelfUpdateStatusArgs = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        if !valid_update_id(&args.update_id) {
            return ToolResult::error("Self-update status request has an invalid update id.");
        }
        match read_update_status(&args.update_id) {
            Ok(record) => Self::result(record),
            Err(_) => ToolResult::error("Self-update status is unavailable."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_update::UpdatePhase;
    use serde_json::json;

    fn record() -> UpdateStatusRecord {
        UpdateStatusRecord {
            update_id: "0123456789abcdef01234567".to_string(),
            from_version: "1.0.0".to_string(),
            target_version: "2.0.0".to_string(),
            state: UpdatePhase::Succeeded,
            updated_at: "2026-08-31T12:34:56Z".to_string(),
            failure_code: None,
            failure_detail: None,
        }
    }

    #[test]
    fn descriptor_is_closed_read_only_and_app_only() {
        let tool = SelfUpdateStatus;
        assert_eq!(tool.name(), "self_update_status");
        assert_eq!(tool.input_schema()["additionalProperties"], false);
        assert_eq!(
            tool.input_schema()["properties"]["updateId"]["pattern"],
            "^[0-9a-f]{24}$"
        );
        assert_eq!(tool.output_schema().unwrap()["additionalProperties"], false);
        assert!(!tool.requires_project_root());

        let behavior = tool.behavior();
        assert!(behavior.read_only);
        assert!(!behavior.destructive);
        assert!(behavior.idempotent);
        assert!(!behavior.open_world);

        let meta = tool.meta().unwrap();
        assert_eq!(meta.get("ui").unwrap()["visibility"], json!(["app"]));
        assert_eq!(meta.get("openai/visibility"), Some(&json!("private")));
        assert_eq!(meta.get("openai/widgetAccessible"), Some(&json!(true)));
    }

    #[test]
    fn successful_result_reports_the_record_and_running_version() {
        let result = SelfUpdateStatus::result(record());
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["state"], "succeeded");
        assert_eq!(structured["runningVersion"], env!("CARGO_PKG_VERSION"));
        assert_eq!(structured["targetVersion"], "2.0.0");
    }

    #[tokio::test]
    async fn malformed_ids_are_rejected_without_exposing_paths() {
        let config = crate::config::default_config(std::env::temp_dir());
        let result = SelfUpdateStatus
            .call(
                json!({ "updateId": "../not-an-update" }),
                &config,
                &SessionState::new(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.joined_text().contains("invalid update id"));
        assert!(!result.joined_text().contains(".codexify"));
    }
}
