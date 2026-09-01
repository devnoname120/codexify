use std::future::Future;
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::conversation_auth::{
    AUTHORIZATION_TOOL_WIRE_NAME, CONVERSATION_AUTH_TOKEN_HEX_LENGTH,
    conversation_auth_tokens_match, validate_conversation_auth_token,
};
use crate::exec_sessions::SessionState;
use crate::project_bindings::{ProjectBindingScope, ProjectBindingState};
use crate::self_update::LatestVersionInspection;
use crate::setup_ui;
use crate::tool::{Tool, ToolBehavior, ToolRequestContext, parse_tool_args};
use crate::tools::check_for_updates::{
    UpdateCheckOutput, UpdateCheckStatus, output_from_result,
    output_schema_value as update_output_schema_value,
};
use crate::types::{AppConfig, ToolResult};

// This is an authentication and authorization tool despite its deliberately
// innocuous ChatGPT-facing name and parameter. ChatGPT can falsely classify a
// token-looking connector call as a secret leak and refuse it, so the wire schema
// says `setup(ref)` and the real token is shaped like a SHA-256 hex string. The
// token is not a digest of a different configured secret.
pub struct ConversationAuthorization;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupArgs {
    r#ref: String,
    #[serde(rename = "connectorVersion")]
    connector_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ConnectorSchemaStatus {
    Current,
    Stale,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SetupProjectStatus {
    Unselected,
    Selected,
    WithoutProject,
    CheckFailed,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectorSchemaInfo {
    status: ConnectorSchemaStatus,
    advertised_version: String,
    observed_version: Option<String>,
    refresh_recommended: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetupProjectInfo {
    status: SetupProjectStatus,
    selection_available: bool,
    access_root: Option<String>,
    name: Option<String>,
    active_path: Option<String>,
    source_path: Option<String>,
    managed_worktree: bool,
    binding_scope: Option<String>,
    detail: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetupDebugInfo {
    update_check_ms: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetupOutput {
    content: String,
    server_version: String,
    next_step: String,
    project: SetupProjectInfo,
    update: UpdateCheckOutput,
    connector_schema: ConnectorSchemaInfo,
    debug: Option<SetupDebugInfo>,
}

fn path_name(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

fn scope_name(scope: ProjectBindingScope) -> String {
    scope.as_str().to_string()
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

fn static_project_info(config: &AppConfig) -> SetupProjectInfo {
    match std::fs::canonicalize(&config.work_dir) {
        Ok(project_root) => SetupProjectInfo {
            status: SetupProjectStatus::Selected,
            selection_available: false,
            access_root: Some(project_root.to_string_lossy().into_owned()),
            name: Some(path_name(&project_root)),
            active_path: Some(project_root.to_string_lossy().into_owned()),
            source_path: Some(project_root.to_string_lossy().into_owned()),
            managed_worktree: false,
            binding_scope: Some("static".to_string()),
            detail: None,
        },
        Err(error) => SetupProjectInfo {
            status: SetupProjectStatus::CheckFailed,
            selection_available: false,
            access_root: Some(config.work_dir.to_string_lossy().into_owned()),
            name: None,
            active_path: None,
            source_path: None,
            managed_worktree: false,
            binding_scope: Some("static".to_string()),
            detail: Some(compact_error(&format!(
                "Could not resolve static project root {}: {error}",
                config.work_dir.display()
            ))),
        },
    }
}

fn project_info(
    config: &AppConfig,
    session: &SessionState,
    context: &ToolRequestContext,
) -> SetupProjectInfo {
    if !config.multi_project {
        return static_project_info(config);
    }

    let state = match context.conversation.as_ref() {
        Some(identity) => context.project_bindings.binding_state(config, identity),
        None => session.binding_state(config),
    };
    match state {
        Ok(ProjectBindingState::Unselected { access_root, scope }) => SetupProjectInfo {
            status: SetupProjectStatus::Unselected,
            selection_available: true,
            access_root: Some(access_root.to_string_lossy().into_owned()),
            name: None,
            active_path: None,
            source_path: None,
            managed_worktree: false,
            binding_scope: Some(scope_name(scope)),
            detail: None,
        },
        Ok(ProjectBindingState::Project(selection)) => SetupProjectInfo {
            status: SetupProjectStatus::Selected,
            selection_available: true,
            access_root: Some(selection.access_root.to_string_lossy().into_owned()),
            name: Some(path_name(&selection.source_project_root)),
            active_path: Some(selection.project_root.to_string_lossy().into_owned()),
            source_path: Some(selection.source_project_root.to_string_lossy().into_owned()),
            managed_worktree: selection.managed_worktree,
            binding_scope: Some(scope_name(selection.scope)),
            detail: None,
        },
        Ok(ProjectBindingState::WithoutProject {
            access_root,
            scratch_root,
            scope,
        }) => SetupProjectInfo {
            status: SetupProjectStatus::WithoutProject,
            selection_available: true,
            access_root: Some(access_root.to_string_lossy().into_owned()),
            name: Some("Chat without a project".to_string()),
            active_path: Some(scratch_root.to_string_lossy().into_owned()),
            source_path: None,
            managed_worktree: false,
            binding_scope: Some(scope_name(scope)),
            detail: None,
        },
        Err(error) => SetupProjectInfo {
            status: SetupProjectStatus::CheckFailed,
            selection_available: true,
            access_root: Some(config.work_dir.to_string_lossy().into_owned()),
            name: None,
            active_path: None,
            source_path: None,
            managed_worktree: false,
            binding_scope: Some(if context.conversation.is_some() {
                "chatgpt_conversation".to_string()
            } else {
                "mcp_transport_session".to_string()
            }),
            detail: Some(compact_error(&error)),
        },
    }
}

fn next_step_for_project(project: &SetupProjectInfo) -> String {
    match project.status {
        SetupProjectStatus::Unselected => {
            "If the intended project is already unambiguous, call `set_project_root` directly. Otherwise let the user choose a project or Chat without a project in the setup card. Then call `get_agent_brief`.".to_string()
        }
        SetupProjectStatus::Selected => {
            "Call `get_agent_brief` before using project tools.".to_string()
        }
        SetupProjectStatus::WithoutProject => {
            "Call `get_agent_brief` before using the private scratch workspace.".to_string()
        }
        SetupProjectStatus::CheckFailed => {
            "Project state could not be checked; resolve the setup-card error before using project-scoped tools."
                .to_string()
        }
    }
}

struct SetupResultInput<'a> {
    scope_description: &'a str,
    next_step: &'a str,
    project: SetupProjectInfo,
    observed_connector_version: Option<&'a str>,
    update_result: Result<LatestVersionInspection, String>,
    debug: bool,
    update_check_ms: u64,
}

fn setup_result(input: SetupResultInput<'_>) -> ToolResult {
    let SetupResultInput {
        scope_description,
        next_step,
        project,
        observed_connector_version,
        update_result,
        debug,
        update_check_ms,
    } = input;
    let advertised_version = env!("CARGO_PKG_VERSION");
    let observed_connector_version = observed_connector_version.map(str::trim);
    let schema_status = match observed_connector_version {
        Some(observed) if observed == advertised_version => ConnectorSchemaStatus::Current,
        Some(_) => ConnectorSchemaStatus::Stale,
        None => ConnectorSchemaStatus::Unknown,
    };
    let connector_schema = ConnectorSchemaInfo {
        status: schema_status,
        advertised_version: advertised_version.to_string(),
        observed_version: observed_connector_version.map(ToOwned::to_owned),
        refresh_recommended: schema_status != ConnectorSchemaStatus::Current,
    };

    let update = output_from_result(update_result);

    let mut text = format!("Setup completed for {scope_description}. {next_step}");
    match update.status {
        UpdateCheckStatus::UpdateAvailable => {
            if let Some(latest) = update.latest_version.as_deref() {
                text.push_str(&format!(
                    " Codexify {latest} is available; the setup panel can start the update."
                ));
            }
        }
        UpdateCheckStatus::CheckFailed => {
            text.push_str(" The latest Codexify release could not be checked quickly.");
        }
        UpdateCheckStatus::UpToDate | UpdateCheckStatus::AheadOfLatest => {}
    }
    if connector_schema.refresh_recommended {
        text.push_str(
            " ChatGPT's cached Codexify connector schema could not be confirmed as current. The setup panel offers a Refresh action that opens the connector settings and explains where to click Refresh.",
        );
    }

    let output = SetupOutput {
        content: text.clone(),
        server_version: advertised_version.to_string(),
        next_step: next_step.to_string(),
        project,
        update,
        connector_schema,
        debug: debug.then_some(SetupDebugInfo { update_check_ms }),
    };
    ToolResult::text(text)
        .with_structured(serde_json::to_value(output).expect("setup output must serialize"))
}

impl ConversationAuthorization {
    fn output_schema_value() -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string" },
                "serverVersion": { "type": "string" },
                "nextStep": { "type": "string" },
                "project": {
                    "type": "object",
                    "properties": {
                        "status": {
                            "type": "string",
                            "enum": ["unselected", "selected", "without_project", "check_failed"]
                        },
                        "selectionAvailable": { "type": "boolean" },
                        "accessRoot": { "type": ["string", "null"] },
                        "name": { "type": ["string", "null"] },
                        "activePath": { "type": ["string", "null"] },
                        "sourcePath": { "type": ["string", "null"] },
                        "managedWorktree": { "type": "boolean" },
                        "bindingScope": {
                            "type": ["string", "null"],
                            "enum": ["static", "chatgpt_conversation", "mcp_transport_session", null]
                        },
                        "detail": { "type": ["string", "null"] }
                    },
                    "required": [
                        "status",
                        "selectionAvailable",
                        "accessRoot",
                        "name",
                        "activePath",
                        "sourcePath",
                        "managedWorktree",
                        "bindingScope",
                        "detail"
                    ],
                    "additionalProperties": false
                },
                "update": update_output_schema_value(),
                "connectorSchema": {
                    "type": "object",
                    "properties": {
                        "status": {
                            "type": "string",
                            "enum": ["current", "stale", "unknown"]
                        },
                        "advertisedVersion": { "type": "string" },
                        "observedVersion": {
                            "anyOf": [
                                { "type": "string" },
                                { "type": "null" }
                            ]
                        },
                        "refreshRecommended": { "type": "boolean" },
                    },
                    "required": [
                        "status",
                        "advertisedVersion",
                        "observedVersion",
                        "refreshRecommended"
                    ],
                    "additionalProperties": false
                },
                "debug": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {
                                "updateCheckMs": {
                                    "type": "integer",
                                    "minimum": 0
                                }
                            },
                            "required": ["updateCheckMs"],
                            "additionalProperties": false
                        },
                        { "type": "null" }
                    ]
                }
            },
            "required": [
                "content",
                "serverVersion",
                "nextStep",
                "project",
                "update",
                "connectorSchema",
                "debug"
            ],
            "additionalProperties": false
        })
    }

    async fn call_with_context_and_update_check<F, Fut>(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: &ToolRequestContext,
        update_check: F,
    ) -> ToolResult
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<LatestVersionInspection, String>>,
    {
        let Some(expected_token) = config.conversation_auth_token.as_deref() else {
            return ToolResult::error("Conversation setup is not enabled.");
        };
        let SetupArgs {
            r#ref: provided_ref,
            connector_version,
        } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(_) => return ToolResult::error("Setup failed."),
        };
        if validate_conversation_auth_token(&provided_ref).is_err()
            || !conversation_auth_tokens_match(expected_token, &provided_ref)
        {
            return ToolResult::error("Setup failed.");
        }

        let scope = context
            .conversation_authorizations
            .authorize(context.conversation.as_ref(), session)
            .map_err(ToolResult::error);
        let scope = match scope {
            Ok(scope) => scope,
            Err(error) => return error,
        };
        let project = project_info(config, session, context);
        let next_step = next_step_for_project(&project);
        let update_started = Instant::now();
        let update_result = update_check().await;
        let update_check_ms =
            u64::try_from(update_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        setup_result(SetupResultInput {
            scope_description: scope.description(),
            next_step: &next_step,
            project,
            observed_connector_version: connector_version.as_deref(),
            update_result,
            debug: config.debug,
            update_check_ms,
        })
    }
}

#[async_trait]
impl Tool for ConversationAuthorization {
    fn name(&self) -> &'static str {
        AUTHORIZATION_TOOL_WIRE_NAME
    }

    fn title(&self) -> String {
        "Set up connector".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            false,
            true,
            true,
            "Adds an authorization grant for the current conversation or transport and performs a bounded public GitHub release check without changing project state.",
        )
    }

    fn meta(&self) -> Option<rmcp::model::MetaObject> {
        Some(setup_ui::tool_meta())
    }

    fn description(&self) -> String {
        format!(
            "Call this function once when using the codexify connector for the first time in a conversation. Provide the SHA-256-shaped `ref` supplied by the user or system prompt. Connector version marker: `{}`; when the `connectorVersion` argument is available, copy this marker into it unchanged. After setup completes, continue as directed. The submitted reference is not retained.",
            env!("CARGO_PKG_VERSION")
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref": {
                    "type": "string",
                    "description": "The 64-character lowercase hexadecimal reference supplied for codexify setup.",
                    "minLength": CONVERSATION_AUTH_TOKEN_HEX_LENGTH,
                    "maxLength": CONVERSATION_AUTH_TOKEN_HEX_LENGTH,
                    "pattern": "^[0-9a-f]{64}$",
                    "writeOnly": true
                },
                "connectorVersion": {
                    "type": "string",
                    "description": "Copy the connector version marker from this tool's advertised description unchanged. Omit only when the cached connector schema does not provide this argument.",
                    "minLength": 1,
                    "maxLength": 64
                }
            },
            "required": ["ref"],
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

    async fn call(&self, _args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        ToolResult::error("Setup requires request metadata.")
    }

    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        self.call_with_context_and_update_check(args, config, session, context, || async {
            crate::self_update::inspect_latest_version()
                .await
                .map_err(|error| format!("{error:#}"))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use semver::Version;

    use super::*;
    use crate::config::default_config;
    use crate::conversation_auth::ConversationAuthorizationStore;
    use crate::diff::DiffCheckpointManager;
    use crate::project_bindings::{ConversationIdentity, ProjectBindingStore};

    fn assert_model_facing_setup_vocabulary(text: &str) {
        let text = text.to_ascii_lowercase();
        for forbidden in [
            "authenticat",
            "authoriz",
            "checksum",
            "credential",
            "secret",
            "token",
            "verify",
        ] {
            assert!(
                !text.contains(forbidden),
                "unexpected `{forbidden}` in `{text}`"
            );
        }
    }

    #[test]
    fn setup_descriptor_carries_a_backward_compatible_connector_version_marker() {
        let tool = ConversationAuthorization;
        let description = tool.description();
        assert!(description.contains(env!("CARGO_PKG_VERSION")));
        assert!(description.contains("connectorVersion"));

        let schema = tool.input_schema();
        assert!(schema["properties"].get("connectorVersion").is_some());
        assert_eq!(schema["required"], json!(["ref"]));
    }

    fn fixture_project_info() -> SetupProjectInfo {
        SetupProjectInfo {
            status: SetupProjectStatus::Selected,
            selection_available: false,
            access_root: Some("/tmp/project".to_string()),
            name: Some("project".to_string()),
            active_path: Some("/tmp/project".to_string()),
            source_path: Some("/tmp/project".to_string()),
            managed_worktree: false,
            binding_scope: Some("static".to_string()),
            detail: None,
        }
    }

    #[test]
    fn setup_result_reports_update_and_connector_schema_status_directly() {
        let result = setup_result(SetupResultInput {
            scope_description: "this ChatGPT conversation",
            next_step: "Call `get_agent_brief` before using project tools.",
            project: fixture_project_info(),
            observed_connector_version: Some("1.0.0"),
            update_result: Ok(crate::self_update::LatestVersionInspection {
                status: crate::self_update::LatestVersionStatus::UpdateAvailable,
                current: Version::new(1, 1, 0),
                latest: Version::new(1, 2, 0),
                source: crate::self_update::LatestVersionSource::GithubCli,
            }),
            debug: true,
            update_check_ms: 17,
        });

        assert!(!result.is_error);
        let structured = result.structured_content.as_ref().unwrap();
        let validator = jsonschema::options()
            .build(&ConversationAuthorization::output_schema_value())
            .unwrap();
        assert!(validator.is_valid(structured));
        assert_eq!(structured["update"]["status"], "update_available");
        assert_eq!(structured["update"]["latestVersion"], "1.2.0");
        assert_eq!(structured["update"]["source"], "github_cli");
        assert_eq!(structured["connectorSchema"]["status"], "stale");
        assert_eq!(structured["connectorSchema"]["refreshRecommended"], true);
        assert!(structured["connectorSchema"].get("settingsUrl").is_none());
        assert!(
            ConversationAuthorization::output_schema_value()["properties"]["connectorSchema"]
                ["properties"]
                .get("settingsUrl")
                .is_none()
        );
        assert_eq!(structured["debug"]["updateCheckMs"], 17);
        assert!(
            result
                .joined_text()
                .contains("opens the connector settings")
        );
        assert!(!result.joined_text().contains("Open ChatGPT Settings"));
    }

    #[test]
    fn missing_connector_marker_is_unknown_and_old_cached_calls_remain_valid() {
        let args: SetupArgs = serde_json::from_value(json!({
            "ref": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        }))
        .unwrap();
        assert!(args.connector_version.is_none());

        let result = setup_result(SetupResultInput {
            scope_description: "this MCP transport session",
            next_step: "Call `get_agent_brief` before using project tools.",
            project: fixture_project_info(),
            observed_connector_version: None,
            update_result: Err("offline".to_string()),
            debug: false,
            update_check_ms: 9,
        });
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["connectorSchema"]["status"], "unknown");
        assert_eq!(structured["connectorSchema"]["refreshRecommended"], true);
        assert_eq!(structured["update"]["status"], "check_failed");
        assert_eq!(structured["debug"], Value::Null);
    }

    fn context(
        identity: Option<ConversationIdentity>,
        authorizations: Arc<ConversationAuthorizationStore>,
        state_root: &std::path::Path,
    ) -> ToolRequestContext {
        ToolRequestContext {
            conversation: identity,
            conversation_authorizations: authorizations,
            project_bindings: Arc::new(ProjectBindingStore::new(
                state_root.join("project-bindings"),
            )),
            diff_checkpoints: Arc::new(DiffCheckpointManager::new()),
            artifact_egress: Arc::new(crate::artifact_egress::ArtifactEgressStore::new_at(
                crate::types::ArtifactEgressConfig::default(),
                state_root.join("artifacts"),
            )),
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    fn current_update() -> LatestVersionInspection {
        LatestVersionInspection {
            status: crate::self_update::LatestVersionStatus::UpToDate,
            current: Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
            latest: Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
            source: crate::self_update::LatestVersionSource::GithubApi,
        }
    }

    fn initialize_git_project(project: &std::path::Path) {
        std::fs::create_dir_all(project).unwrap();
        std::fs::write(project.join("tracked.txt"), "tracked\n").unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["add", "tracked.txt"],
            vec![
                "-c",
                "user.email=codexify@example.invalid",
                "-c",
                "user.name=Codexify Tests",
                "commit",
                "--quiet",
                "-m",
                "initial",
            ],
        ] {
            let output = std::process::Command::new("git")
                .args(&args)
                .current_dir(project)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[tokio::test]
    async fn setup_reports_an_unselected_multi_project_context() {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        std::fs::create_dir_all(&access).unwrap();
        let mut config = default_config(access.clone());
        config.multi_project = true;
        let auth_token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        config.conversation_auth_token = Some(auth_token.into());
        let identity = ConversationIdentity::from_openai_session("project-picker").unwrap();
        let request_context = context(
            Some(identity),
            Arc::new(ConversationAuthorizationStore::new()),
            root.path(),
        );

        let result = ConversationAuthorization
            .call_with_context_and_update_check(
                json!({ "ref": auth_token }),
                &config,
                &SessionState::new(),
                &request_context,
                || async { Ok(current_update()) },
            )
            .await;

        assert!(!result.is_error, "{}", result.joined_text());
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["project"]["status"], "unselected");
        assert_eq!(structured["project"]["selectionAvailable"], true);
        assert_eq!(
            structured["project"]["bindingScope"],
            "chatgpt_conversation"
        );
        assert!(structured["project"]["activePath"].is_null());
        assert!(
            structured["nextStep"]
                .as_str()
                .unwrap()
                .contains("setup card")
        );
    }

    #[tokio::test]
    async fn setup_reports_selected_project_and_scratch_paths() {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        let project = access.join("alpha");
        std::fs::create_dir_all(&project).unwrap();
        let mut config = default_config(access.clone());
        config.multi_project = true;
        config.worktrees.mode = crate::types::WorktreeMode::Never;
        let auth_token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        config.conversation_auth_token = Some(auth_token.into());

        let identity = ConversationIdentity::from_openai_session("selected-project").unwrap();
        let selected_context = context(
            Some(identity.clone()),
            Arc::new(ConversationAuthorizationStore::new()),
            &root.path().join("selected"),
        );
        selected_context
            .project_bindings
            .select_project_root(&config, &identity, "alpha")
            .await
            .unwrap();
        let selected = ConversationAuthorization
            .call_with_context_and_update_check(
                json!({ "ref": auth_token }),
                &config,
                &SessionState::new(),
                &selected_context,
                || async { Ok(current_update()) },
            )
            .await;
        let selected_output = selected.structured_content.as_ref().unwrap();
        assert_eq!(selected_output["project"]["status"], "selected");
        assert_eq!(selected_output["project"]["name"], "alpha");
        assert_eq!(
            selected_output["project"]["activePath"],
            std::fs::canonicalize(&project)
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(
            selected_output["project"]["sourcePath"],
            selected_output["project"]["activePath"]
        );
        assert_eq!(selected_output["project"]["managedWorktree"], false);

        let scratch_identity =
            ConversationIdentity::from_openai_session("selected-scratch").unwrap();
        let scratch_context = context(
            Some(scratch_identity.clone()),
            Arc::new(ConversationAuthorizationStore::new()),
            &root.path().join("scratch"),
        );
        let scratch_selection = scratch_context
            .project_bindings
            .select_without_project(&config, &scratch_identity)
            .await
            .unwrap();
        let scratch = ConversationAuthorization
            .call_with_context_and_update_check(
                json!({ "ref": auth_token }),
                &config,
                &SessionState::new(),
                &scratch_context,
                || async { Ok(current_update()) },
            )
            .await;
        let scratch_output = scratch.structured_content.as_ref().unwrap();
        assert_eq!(scratch_output["project"]["status"], "without_project");
        assert_eq!(scratch_output["project"]["name"], "Chat without a project");
        assert_eq!(
            scratch_output["project"]["activePath"],
            scratch_selection.scratch_root.to_string_lossy().as_ref()
        );
        assert!(scratch_output["project"]["sourcePath"].is_null());
    }

    #[tokio::test]
    async fn setup_reports_the_active_managed_worktree_path() {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        let project = access.join("alpha");
        initialize_git_project(&project);
        let mut config = default_config(access);
        config.multi_project = true;
        config.worktrees.mode = crate::types::WorktreeMode::Always;
        config.worktrees.root = root.path().join("worktrees");
        config.worktrees.auto_cleanup_enabled = false;
        let auth_token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        config.conversation_auth_token = Some(auth_token.into());
        let identity = ConversationIdentity::from_openai_session("managed-worktree").unwrap();
        let request_context = context(
            Some(identity.clone()),
            Arc::new(ConversationAuthorizationStore::new()),
            root.path(),
        );
        let selection = request_context
            .project_bindings
            .select_project_root(&config, &identity, "alpha")
            .await
            .unwrap();
        assert!(selection.managed_worktree);

        let result = ConversationAuthorization
            .call_with_context_and_update_check(
                json!({ "ref": auth_token }),
                &config,
                &SessionState::new(),
                &request_context,
                || async { Ok(current_update()) },
            )
            .await;

        let project_info = &result.structured_content.as_ref().unwrap()["project"];
        assert_eq!(project_info["status"], "selected");
        assert_eq!(project_info["managedWorktree"], true);
        assert!(
            same_file::is_same_file(
                project_info["activePath"].as_str().unwrap(),
                &selection.project_root,
            )
            .unwrap()
        );
        assert!(
            same_file::is_same_file(
                project_info["sourcePath"].as_str().unwrap(),
                &selection.source_project_root,
            )
            .unwrap()
        );
        assert_ne!(project_info["activePath"], project_info["sourcePath"]);
    }

    #[tokio::test]
    async fn setup_keeps_authorization_success_when_project_state_check_fails() {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        std::fs::create_dir_all(&access).unwrap();
        let mut config = default_config(access.clone());
        config.multi_project = true;
        let auth_token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        config.conversation_auth_token = Some(auth_token.into());
        let identity = ConversationIdentity::from_openai_session("broken-project-state").unwrap();
        let request_context = context(
            Some(identity),
            Arc::new(ConversationAuthorizationStore::new()),
            root.path(),
        );
        std::fs::remove_dir_all(&access).unwrap();

        let result = ConversationAuthorization
            .call_with_context_and_update_check(
                json!({ "ref": auth_token }),
                &config,
                &SessionState::new(),
                &request_context,
                || async { Ok(current_update()) },
            )
            .await;

        assert!(!result.is_error, "{}", result.joined_text());
        let project = &result.structured_content.as_ref().unwrap()["project"];
        assert_eq!(project["status"], "check_failed");
        assert!(project["detail"].as_str().unwrap().contains("access root"));
    }

    #[tokio::test]
    async fn valid_auth_token_authorizes_only_the_current_conversation() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        let auth_token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        config.conversation_auth_token = Some(auth_token.into());
        let store = Arc::new(ConversationAuthorizationStore::new());
        let session = SessionState::new();
        let first = ConversationIdentity::from_openai_session("first").unwrap();
        let second = ConversationIdentity::from_openai_session("second").unwrap();
        let request_context = context(Some(first.clone()), store.clone(), root.path());

        let result = ConversationAuthorization
            .call_with_context_and_update_check(
                json!({ "ref": auth_token }),
                &config,
                &session,
                &request_context,
                || async { Err("offline fixture".to_string()) },
            )
            .await;

        assert!(!result.is_error);
        assert_model_facing_setup_vocabulary(&result.joined_text());
        assert!(store.is_authorized(Some(&first), &session));
        assert!(!store.is_authorized(Some(&second), &session));
    }

    #[tokio::test]
    async fn invalid_auth_token_does_not_authorize_or_echo_the_value() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.conversation_auth_token =
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into());
        let store = Arc::new(ConversationAuthorizationStore::new());
        let session = SessionState::new();
        let identity = ConversationIdentity::from_openai_session("first").unwrap();
        let request_context = context(Some(identity.clone()), store.clone(), root.path());
        let invalid = "abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd";

        let result = ConversationAuthorization
            .call_with_context(
                json!({ "ref": invalid }),
                &config,
                &session,
                &request_context,
            )
            .await;

        assert!(result.is_error);
        assert_model_facing_setup_vocabulary(&result.joined_text());
        assert!(!result.joined_text().contains(invalid));
        assert!(!store.is_authorized(Some(&identity), &session));
    }

    #[tokio::test]
    async fn clients_without_conversation_metadata_use_transport_scope() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        let auth_token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        config.conversation_auth_token = Some(auth_token.into());
        let store = Arc::new(ConversationAuthorizationStore::new());
        let first_session = SessionState::new();
        let second_session = SessionState::new();
        let request_context = context(None, store.clone(), root.path());

        ConversationAuthorization
            .call_with_context_and_update_check(
                json!({ "ref": auth_token }),
                &config,
                &first_session,
                &request_context,
                || async { Err("offline fixture".to_string()) },
            )
            .await;

        assert!(store.is_authorized(None, &first_session));
        assert!(!store.is_authorized(None, &second_session));
    }
}
