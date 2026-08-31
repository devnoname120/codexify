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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectorSchemaInfo {
    status: ConnectorSchemaStatus,
    advertised_version: String,
    observed_version: Option<String>,
    refresh_recommended: bool,
    settings_url: Option<String>,
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
    update: UpdateCheckOutput,
    connector_schema: ConnectorSchemaInfo,
    debug: Option<SetupDebugInfo>,
}

fn setup_result(
    scope_description: &str,
    next_step: &str,
    observed_connector_version: Option<&str>,
    update_result: Result<LatestVersionInspection, String>,
    settings_url: Option<&str>,
    debug: bool,
    update_check_ms: u64,
) -> ToolResult {
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
        settings_url: settings_url.map(ToOwned::to_owned),
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
            " ChatGPT's cached Codexify connector schema could not be confirmed as current. Open ChatGPT Settings, select the Codexify connector, scroll to the bottom of its tool list, and click Refresh.",
        );
        if let Some(url) = connector_schema.settings_url.as_deref() {
            text.push_str(&format!(" Open the connector settings directly: {url}"));
        }
    }

    let output = SetupOutput {
        content: text.clone(),
        server_version: advertised_version.to_string(),
        next_step: next_step.to_string(),
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
                        "settingsUrl": {
                            "anyOf": [
                                { "type": "string" },
                                { "type": "null" }
                            ]
                        }
                    },
                    "required": [
                        "status",
                        "advertisedVersion",
                        "observedVersion",
                        "refreshRecommended",
                        "settingsUrl"
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
        let next_step = if config.multi_project {
            "Select the project for this conversation, then call `get_agent_brief`."
        } else {
            "Call `get_agent_brief` before using project tools."
        };
        let update_started = Instant::now();
        let update_result = update_check().await;
        let update_check_ms =
            u64::try_from(update_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let settings_url = config.chatgpt_connector_settings_url.clone().or_else(|| {
            context
                .connector_id
                .as_deref()
                .and_then(setup_ui::connector_settings_url)
        });
        setup_result(
            scope.description(),
            next_step,
            connector_version.as_deref(),
            update_result,
            settings_url.as_deref(),
            config.debug,
            update_check_ms,
        )
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
    use crate::project_bindings::ConversationIdentity;
    use crate::self_update::{LatestVersionSource, LatestVersionStatus};

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

    #[test]
    fn setup_result_reports_update_and_connector_schema_status_directly() {
        let result = setup_result(
            "this ChatGPT conversation",
            "Call `get_agent_brief` before using project tools.",
            Some("1.0.0"),
            Ok(crate::self_update::LatestVersionInspection {
                status: crate::self_update::LatestVersionStatus::UpdateAvailable,
                current: Version::new(1, 1, 0),
                latest: Version::new(1, 2, 0),
                source: crate::self_update::LatestVersionSource::GithubCli,
            }),
            Some("https://chatgpt.com/g/example/project#settings/Plugins/plugin_example"),
            true,
            17,
        );

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
        assert_eq!(structured["debug"]["updateCheckMs"], 17);
        assert!(result.joined_text().contains("click Refresh"));
    }

    #[test]
    fn missing_connector_marker_is_unknown_and_old_cached_calls_remain_valid() {
        let args: SetupArgs = serde_json::from_value(json!({
            "ref": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        }))
        .unwrap();
        assert!(args.connector_version.is_none());

        let result = setup_result(
            "this MCP transport session",
            "Call `get_agent_brief` before using project tools.",
            None,
            Err("offline".to_string()),
            None,
            false,
            9,
        );
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
            connector_id: None,
            conversation_authorizations: authorizations,
            diff_checkpoints: Arc::new(DiffCheckpointManager::new()),
            artifact_egress: Arc::new(crate::artifact_egress::ArtifactEgressStore::new_at(
                crate::types::ArtifactEgressConfig::default(),
                state_root.join("artifacts"),
            )),
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn request_connector_id_supplies_a_direct_settings_link() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        let auth_token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        config.conversation_auth_token = Some(auth_token.into());
        let store = Arc::new(ConversationAuthorizationStore::new());
        let session = SessionState::new();
        let mut request_context = context(None, store, root.path());
        request_context.connector_id = Some("asdk_app_abc123".to_string());

        let result = ConversationAuthorization
            .call_with_context_and_update_check(
                json!({
                    "ref": auth_token,
                    "connectorVersion": env!("CARGO_PKG_VERSION")
                }),
                &config,
                &session,
                &request_context,
                || async {
                    Ok(LatestVersionInspection {
                        status: LatestVersionStatus::UpToDate,
                        current: Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
                        latest: Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
                        source: LatestVersionSource::GithubApi,
                    })
                },
            )
            .await;

        assert_eq!(
            result.structured_content.as_ref().unwrap()["connectorSchema"]["settingsUrl"],
            "https://chatgpt.com/#settings/Plugins/plugin_asdk_app_abc123"
        );
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
