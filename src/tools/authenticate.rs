use async_trait::async_trait;
use serde_json::{Value, json};

use crate::conversation_auth::{
    AUTHENTICATE_TOOL_NAME, MAX_CONVERSATION_AUTH_TOKEN_BYTES, MIN_CONVERSATION_AUTH_TOKEN_BYTES,
    conversation_auth_tokens_match, validate_conversation_auth_token,
};
use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolRequestContext, arg_str};
use crate::types::{AppConfig, ToolResult};

pub struct Authenticate;

#[async_trait]
impl Tool for Authenticate {
    fn name(&self) -> &'static str {
        AUTHENTICATE_TOOL_NAME
    }

    fn description(&self) -> String {
        "Authorize this ChatGPT conversation to use the connector. Call once with the token supplied by the user or in ChatGPT Project instructions. After verification, only the authorization decision is cached; the submitted token is not retained.".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "token": {
                    "type": "string",
                    "description": "The connector conversation-authentication token.",
                    "minLength": MIN_CONVERSATION_AUTH_TOKEN_BYTES,
                    "maxLength": MAX_CONVERSATION_AUTH_TOKEN_BYTES,
                    "pattern": "^[A-Za-z0-9_-]+$",
                    "writeOnly": true
                }
            },
            "required": ["token"],
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": { "type": "string" }
            },
            "required": ["content"],
            "additionalProperties": false
        }))
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, _args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        ToolResult::error("Authentication requires request metadata.")
    }

    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        let Some(expected) = config.conversation_auth_token.as_deref() else {
            return ToolResult::error("Conversation authentication is not enabled.");
        };
        let Some(provided) = arg_str(&args, "token") else {
            return ToolResult::error("Authentication failed.");
        };
        if validate_conversation_auth_token(provided).is_err()
            || !conversation_auth_tokens_match(expected, provided)
        {
            return ToolResult::error("Authentication failed.");
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
        ToolResult::text(format!(
            "Authentication succeeded for {}. {next_step}",
            scope.description()
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::default_config;
    use crate::conversation_auth::ConversationAuthorizationStore;
    use crate::project_bindings::ConversationIdentity;
    use crate::review::ReviewCheckpointManager;

    fn context(
        identity: Option<ConversationIdentity>,
        authorizations: Arc<ConversationAuthorizationStore>,
    ) -> ToolRequestContext {
        ToolRequestContext {
            conversation: identity,
            conversation_authorizations: authorizations,
            review_checkpoints: Arc::new(ReviewCheckpointManager::new()),
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn valid_token_authorizes_only_the_current_conversation() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        let token = "codexify_chat_0123456789abcdef0123456789abcdef";
        config.conversation_auth_token = Some(token.to_string());
        let store = Arc::new(ConversationAuthorizationStore::new());
        let session = SessionState::new();
        let first = ConversationIdentity::from_openai_session("first").unwrap();
        let second = ConversationIdentity::from_openai_session("second").unwrap();
        let request_context = context(Some(first.clone()), store.clone());

        let result = Authenticate
            .call_with_context(
                json!({ "token": token }),
                &config,
                &session,
                &request_context,
            )
            .await;

        assert!(!result.is_error);
        assert!(store.is_authorized(Some(&first), &session));
        assert!(!store.is_authorized(Some(&second), &session));
    }

    #[tokio::test]
    async fn invalid_token_does_not_authorize_or_echo_the_value() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.conversation_auth_token =
            Some("codexify_chat_0123456789abcdef0123456789abcdef".to_string());
        let store = Arc::new(ConversationAuthorizationStore::new());
        let session = SessionState::new();
        let identity = ConversationIdentity::from_openai_session("first").unwrap();
        let request_context = context(Some(identity.clone()), store.clone());
        let invalid = "codexify_chat_invalid-invalid-invalid-invalid";

        let result = Authenticate
            .call_with_context(
                json!({ "token": invalid }),
                &config,
                &session,
                &request_context,
            )
            .await;

        assert!(result.is_error);
        assert!(!result.joined_text().contains(invalid));
        assert!(!store.is_authorized(Some(&identity), &session));
    }

    #[tokio::test]
    async fn clients_without_conversation_metadata_use_transport_scope() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        let token = "codexify_chat_0123456789abcdef0123456789abcdef";
        config.conversation_auth_token = Some(token.to_string());
        let store = Arc::new(ConversationAuthorizationStore::new());
        let first_session = SessionState::new();
        let second_session = SessionState::new();
        let request_context = context(None, store.clone());

        Authenticate
            .call_with_context(
                json!({ "token": token }),
                &config,
                &first_session,
                &request_context,
            )
            .await;

        assert!(store.is_authorized(None, &first_session));
        assert!(!store.is_authorized(None, &second_session));
    }
}
