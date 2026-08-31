use async_trait::async_trait;
use rmcp::model::MetaObject;
use serde::Serialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::self_update::{LatestVersionInspection, LatestVersionSource, LatestVersionStatus};
use crate::tool::{Tool, ToolBehavior, empty_object_schema};
use crate::types::{AppConfig, ToolResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UpdateCheckStatus {
    UpdateAvailable,
    UpToDate,
    AheadOfLatest,
    CheckFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateCheckOutput {
    pub(crate) status: UpdateCheckStatus,
    pub(crate) current_version: String,
    pub(crate) latest_version: Option<String>,
    pub(crate) source: Option<LatestVersionSource>,
    pub(crate) detail: Option<String>,
}

fn compact_error(error: &str) -> String {
    error
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(512)
        .collect()
}

pub(crate) fn output_from_result(
    result: Result<LatestVersionInspection, String>,
) -> UpdateCheckOutput {
    match result {
        Ok(inspection) => UpdateCheckOutput {
            status: match inspection.status {
                LatestVersionStatus::UpdateAvailable => UpdateCheckStatus::UpdateAvailable,
                LatestVersionStatus::UpToDate => UpdateCheckStatus::UpToDate,
                LatestVersionStatus::AheadOfLatest => UpdateCheckStatus::AheadOfLatest,
            },
            current_version: inspection.current.to_string(),
            latest_version: Some(inspection.latest.to_string()),
            source: Some(inspection.source),
            detail: None,
        },
        Err(error) => UpdateCheckOutput {
            status: UpdateCheckStatus::CheckFailed,
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            latest_version: None,
            source: None,
            detail: Some(compact_error(&error)),
        },
    }
}

pub(crate) fn output_schema_value() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": {
                "type": "string",
                "enum": ["update_available", "up_to_date", "ahead_of_latest", "check_failed"]
            },
            "currentVersion": { "type": "string" },
            "latestVersion": {
                "anyOf": [
                    { "type": "string" },
                    { "type": "null" }
                ]
            },
            "source": {
                "anyOf": [
                    {
                        "type": "string",
                        "enum": ["github_cli", "github_api"]
                    },
                    { "type": "null" }
                ]
            },
            "detail": {
                "anyOf": [
                    { "type": "string", "maxLength": 512 },
                    { "type": "null" }
                ]
            }
        },
        "required": ["status", "currentVersion", "latestVersion", "source", "detail"],
        "additionalProperties": false
    })
}

pub struct CheckForUpdates;

impl CheckForUpdates {
    fn result(output: UpdateCheckOutput) -> ToolResult {
        let text = match output.status {
            UpdateCheckStatus::UpdateAvailable => format!(
                "Codexify {} is available.",
                output.latest_version.as_deref().unwrap_or("latest")
            ),
            UpdateCheckStatus::UpToDate => {
                format!("Codexify {} is the latest release.", output.current_version)
            }
            UpdateCheckStatus::AheadOfLatest => format!(
                "The running Codexify version {} is newer than the latest published release {}.",
                output.current_version,
                output.latest_version.as_deref().unwrap_or("unknown")
            ),
            UpdateCheckStatus::CheckFailed => {
                "The latest Codexify release could not be checked quickly.".to_string()
            }
        };
        ToolResult::text(text).with_structured(
            serde_json::to_value(output).expect("update-check output must serialize"),
        )
    }
}

#[async_trait]
impl Tool for CheckForUpdates {
    fn name(&self) -> &'static str {
        "check_for_updates"
    }

    fn title(&self) -> String {
        "Check for Codexify updates".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            true,
            "Performs bounded public GitHub release probes without changing configuration, services, projects, or external state.",
        )
    }

    fn meta(&self) -> Option<MetaObject> {
        Some(
            serde_json::from_value(json!({
                "ui": { "visibility": ["app"] },
                "openai/visibility": "private",
                "openai/widgetAccessible": true,
                "openai/toolInvocation/invoking": "Checking for Codexify updates",
                "openai/toolInvocation/invoked": "Codexify update check finished"
            }))
            .expect("static update-check metadata must be an object"),
        )
    }

    fn description(&self) -> String {
        "Force a fresh bounded latest-release check for the Codexify setup widget. This action is available to Codexify widgets and intentionally hidden from the model."
            .to_string()
    }

    fn input_schema(&self) -> Value {
        empty_object_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(output_schema_value())
    }

    fn fills_structured_content(&self) -> bool {
        false
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, _args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        let result = crate::self_update::refresh_latest_version()
            .await
            .map_err(|error| format!("{error:#}"));
        Self::result(output_from_result(result))
    }
}

#[cfg(test)]
mod tests {
    use semver::Version;

    use super::*;

    #[test]
    fn update_check_result_matches_the_advertised_schema() {
        let output = output_from_result(Ok(LatestVersionInspection {
            status: LatestVersionStatus::UpdateAvailable,
            current: Version::new(1, 2, 0),
            latest: Version::new(1, 3, 0),
            source: LatestVersionSource::GithubCli,
        }));
        let result = CheckForUpdates::result(output);
        let structured = result.structured_content.as_ref().unwrap();
        let validator = jsonschema::options().build(&output_schema_value()).unwrap();

        assert!(validator.is_valid(structured));
        assert_eq!(structured["status"], "update_available");
        assert_eq!(structured["currentVersion"], "1.2.0");
        assert_eq!(structured["latestVersion"], "1.3.0");
        assert_eq!(structured["source"], "github_cli");
    }

    #[test]
    fn failed_update_check_is_non_error_structured_state() {
        let output = output_from_result(Err("offline\nwith   noise".to_string()));
        let result = CheckForUpdates::result(output);

        assert!(!result.is_error);
        assert_eq!(
            result.structured_content.as_ref().unwrap()["status"],
            "check_failed"
        );
        assert_eq!(
            result.structured_content.as_ref().unwrap()["detail"],
            "offline with noise"
        );
    }
}
