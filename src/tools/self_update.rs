use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::self_update::{SelfUpdateReceipt, SelfUpdateStatus};
use crate::tool::{Tool, ToolBehavior, parse_tool_args};
use crate::types::{AppConfig, ToolResult};

pub struct SelfUpdate;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelfUpdateArgs {
    confirm: bool,
}

impl SelfUpdate {
    fn output_schema_value() -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["scheduled", "up_to_date", "ahead_of_latest"]
                },
                "currentVersion": { "type": "string" },
                "targetVersion": { "type": "string" },
                "updateId": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "serviceRestart": { "type": "boolean" },
                "logPath": { "type": "string" }
            },
            "required": [
                "status",
                "currentVersion",
                "targetVersion",
                "updateId",
                "serviceRestart",
                "logPath"
            ],
            "additionalProperties": false
        })
    }

    fn result(receipt: SelfUpdateReceipt) -> ToolResult {
        let text = match receipt.status {
            SelfUpdateStatus::Scheduled => {
                if receipt.service_restart {
                    format!(
                        "Codexify {} is verified and staged. Detached update {} will stop and restart the service after this response is delivered; the MCP connection will disconnect temporarily. Follow progress with `codexify service logs -f`.",
                        receipt.target_version,
                        receipt.update_id.as_deref().unwrap_or("<unknown>")
                    )
                } else {
                    format!(
                        "Codexify {} is verified and staged. Detached update {} will replace the installed executable after this response is delivered. The current foreground process will continue running the previous version until it is restarted.",
                        receipt.target_version,
                        receipt.update_id.as_deref().unwrap_or("<unknown>")
                    )
                }
            }
            SelfUpdateStatus::UpToDate => format!(
                "Codexify {} is already the latest release.",
                receipt.current_version
            ),
            SelfUpdateStatus::AheadOfLatest => format!(
                "The running Codexify version {} is newer than the latest published release {}; no update was scheduled.",
                receipt.current_version, receipt.target_version
            ),
        };
        ToolResult::text(text).with_structured(
            serde_json::to_value(receipt).expect("self-update receipt must serialize"),
        )
    }
}

#[async_trait]
impl Tool for SelfUpdate {
    fn name(&self) -> &'static str {
        "self_update"
    }

    fn title(&self) -> String {
        "Update Codexify".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            true,
            false,
            true,
            "Downloads a release from GitHub, replaces the installed Codexify executable, and may restart its background service.",
        )
    }

    fn description(&self) -> String {
        "Update the installed Codexify executable to the latest verified GitHub release. Call only after the user explicitly requests an update and pass confirm=true. The release is downloaded, checksum-verified, extracted, and probed before a detached OS-managed worker is scheduled. When Codexify is service-supervised, that worker runs outside the service kill boundary, waits for this tool response to be delivered, stops the service, atomically replaces the executable with rollback protection, and starts the service again. The MCP connection will disconnect temporarily. Progress is written to the service log.".to_string()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "confirm": {
                    "type": "boolean",
                    "const": true,
                    "description": "Must be true, confirming that the user explicitly requested this self-update."
                }
            },
            "required": ["confirm"],
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
        let args: SelfUpdateArgs = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        if !args.confirm {
            return ToolResult::error(
                "Self-update was not confirmed. Call self_update only after an explicit user request and pass confirm=true.",
            );
        }
        match crate::self_update::trigger().await {
            Ok(receipt) => Self::result(receipt),
            Err(error) => ToolResult::error(format!(
                "Codexify self-update could not be prepared or scheduled: {error:#}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_requires_explicit_confirmation_and_has_exact_output() {
        let tool = SelfUpdate;
        assert_eq!(tool.input_schema()["properties"]["confirm"]["const"], true);
        assert_eq!(tool.input_schema()["required"], json!(["confirm"]));
        let output = tool.output_schema().unwrap();
        assert_eq!(output["additionalProperties"], false);
        assert_eq!(output["properties"]["status"]["enum"][0], "scheduled");
    }

    #[test]
    fn behavior_matches_global_destructive_open_world_update() {
        let behavior = SelfUpdate.behavior();
        assert!(!behavior.read_only);
        assert!(behavior.destructive);
        assert!(!behavior.idempotent);
        assert!(behavior.open_world);
        assert!(!SelfUpdate.requires_project_root());
        assert!(!SelfUpdate.may_modify_project());
    }

    #[tokio::test]
    async fn false_confirmation_is_rejected_before_update_preflight() {
        let config = crate::config::default_config(std::env::temp_dir());
        let result = SelfUpdate
            .call(json!({ "confirm": false }), &config, &SessionState::new())
            .await;
        assert!(result.is_error);
        assert!(result.joined_text().contains("not confirmed"));
    }

    #[test]
    fn receipts_have_schema_matching_structured_content() {
        let receipt = SelfUpdateReceipt {
            status: SelfUpdateStatus::Scheduled,
            current_version: "1.0.0".to_string(),
            target_version: "2.0.0".to_string(),
            update_id: Some("abc123".to_string()),
            service_restart: true,
            log_path: "/tmp/codexify.log".to_string(),
        };
        let result = SelfUpdate::result(receipt);
        assert!(!result.is_error);
        let validator = jsonschema::options()
            .build(&SelfUpdate::output_schema_value())
            .unwrap();
        assert!(validator.is_valid(result.structured_content.as_ref().unwrap()));
        assert!(result.joined_text().contains("disconnect temporarily"));
    }
}
