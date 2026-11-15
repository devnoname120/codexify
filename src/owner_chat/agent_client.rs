//! Universal agent connection, published with the service and tunnel.
use super::*;
use crate::agent_automation::agents::AgentRuntime;
use crate::tool::Tool;
use crate::tools::agent_client::AgentClient;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};

const MCP_PATH: &str = "/mcp/agents";
const REQUEST_PATH: &str = "/api/agent-client/request";
const SCHEMA_PATH: &str = "/api/agent-client/schema";
pub const INSTRUCTIONS: &str = "Discover the running connection with `codexify agents connection`. Use agent_client over MCP, POST JSON to requestUrl with its headers, or import agent() from the supplied JavaScript helper. Call capabilities, then setup with a stable requestId, clientName, projectPath, model and effort; save teamId. Use status/inbox for progress, spawn for scoped workers, and control for message/set_goal/pause/resume/stop/retire. Use fresh expectedRevision values and reuse identical requestId arguments after errors. Workers implement authorized assignments by default; use readOnly:true for audits. Read-only parents cannot delegate editing. Verify reports and respect stops.";
const JAVASCRIPT: &str = include_str!("agent_client.mjs");

pub(super) fn is_control_path(path: &str) -> bool {
    path == MCP_PATH
        || path.starts_with("/mcp/agents/")
        || path == REQUEST_PATH
        || path == SCHEMA_PATH
}

// This credential authorizes only the agent endpoint, never the owner UI or
// the tunnel. The random owner secret remains private and cannot be recovered
// from this domain-separated digest.
pub(super) fn bearer(owner_token: &str) -> String {
    format!(
        "codexify_agents_{:x}",
        Sha256::digest(format!("codexify-agent-client-v1\0{owner_token}").as_bytes())
    )
}

fn enabled(state: &OwnerState) -> bool {
    state.config.markdown_chat.enabled
        && state.config.markdown_chat.automation.enabled
        && state.config.markdown_chat.features.agent_teams
        && state.config.markdown_chat.features.external_agent_control
        && state.automation.is_some()
}

async fn connection_status(State(state): State<OwnerState>) -> ApiResult {
    if !enabled(&state) {
        return Err((StatusCode::PRECONDITION_REQUIRED,
            "Enable Browser automation, Agent teams and AI agent control in Settings, then restart Codexify".into()));
    }
    Ok(Json(
        json!({"enabled":true,"path":MCP_PATH,"requestPath":REQUEST_PATH,"schemaPath":SCHEMA_PATH,"tool":"agent_client","instructions":INSTRUCTIONS}),
    ))
}

pub(super) fn router(state: &OwnerState) -> Router<OwnerState> {
    let routes = Router::new().route("/api/agent-client", get(connection_status));
    if !enabled(state) {
        return routes;
    }
    let runtime = Arc::new(AgentRuntime::new(
        state.config.clone(),
        state.chats.clone(),
        state.project_bindings.clone(),
        state.continuations.clone(),
        state.automation.as_ref().unwrap().clone(),
    ));
    let tool = validated_tool();
    let mut transport =
        rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default();
    transport.json_response = true;
    transport.allowed_hosts = vec!["127.0.0.1".into()];
    let service = StreamableHttpService::new(
        move || {
            Ok(Handler {
                runtime: runtime.clone(),
                tool: tool.clone(),
            })
        },
        Arc::new(LocalSessionManager::default()),
        transport,
    );
    routes
        .nest_service(MCP_PATH, service)
        .route(REQUEST_PATH, post(request))
        .route(SCHEMA_PATH, get(schema))
}

fn validated_tool() -> Arc<dyn Tool> {
    static TOOL: std::sync::LazyLock<Arc<dyn Tool>> = std::sync::LazyLock::new(|| {
        crate::tool::validate_and_wrap_tool(Box::new(AgentClient))
            .expect("valid agent_client contract")
            .into()
    });
    TOOL.clone()
}

async fn schema() -> Json<Value> {
    Json(
        json!({"name":"agent_client","instructions":INSTRUCTIONS,"inputSchema":validated_tool().input_schema()}),
    )
}

async fn request(State(state): State<OwnerState>, Json(args): Json<Value>) -> ApiResult {
    validated_tool()
        .validate_arguments(&args)
        .and_then(|_| AgentClient.validate_arguments(&args))
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    let runtime = AgentRuntime::new(
        state.config.clone(),
        state.chats.clone(),
        state.project_bindings.clone(),
        state.continuations.clone(),
        state.automation.clone().ok_or_else(|| {
            (
                StatusCode::PRECONDITION_REQUIRED,
                "Enable browser automation".into(),
            )
        })?,
    );
    let cancellation = CancellationToken::new();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let result = tokio::time::timeout(
        Duration::from_secs(180),
        AgentClient::execute(args, &runtime, &cancellation),
    )
    .await
    .map_err(|_| {
        (
            StatusCode::GATEWAY_TIMEOUT,
            "Agent request timed out; retry the same requestId and arguments".into(),
        )
    })?;
    if result.is_error {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, result.joined_text()));
    }
    Ok(Json(result.structured_content.unwrap_or(Value::Null)))
}

#[derive(Clone)]
struct Handler {
    runtime: Arc<AgentRuntime>,
    tool: Arc<dyn Tool>,
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "codexify-agents",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tool = rmcp::model::Tool::new(
            self.tool.name(),
            self.tool.description(),
            self.tool
                .input_schema()
                .as_object()
                .cloned()
                .unwrap_or_default(),
        )
        .with_title(self.tool.title())
        .with_annotations(self.tool.behavior().annotations());
        Ok(ListToolsResult::with_all_items(vec![tool]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if request.name != self.tool.name() {
            return Err(McpError::invalid_params("Unknown agent-control tool", None));
        }
        if ConversationIdentity::from_request_meta(&context.meta).is_some()
            || request
                .meta
                .as_ref()
                .and_then(ConversationIdentity::from_request_meta)
                .is_some()
        {
            return Err(McpError::invalid_params(
                "ChatGPT conversations cannot switch to a coding-app team identity",
                None,
            ));
        }
        let args = Value::Object(request.arguments.unwrap_or_default());
        let result = match self
            .tool
            .validate_arguments(&args)
            .and_then(|_| AgentClient.validate_arguments(&args))
        {
            Err(error) => crate::types::ToolResult::error(error),
            Ok(()) => AgentClient::execute(args, &self.runtime, &context.ct).await,
        };
        let content = vec![ContentBlock::text(result.joined_text())];
        let mut response = if result.is_error {
            CallToolResult::error(content)
        } else {
            CallToolResult::success(content)
        };
        response.structured_content = result.structured_content;
        Ok(response.into())
    }
}

pub(super) fn connection_directory() -> PathBuf {
    runtime_path()
        .parent()
        .expect("owner runtime has a directory")
        .join("agents")
}

fn descriptor(info: &RuntimeInfo, directory: &FilePath) -> Value {
    let base = format!("http://127.0.0.1:{}", info.port);
    json!({"version":1,"enabled":true,"transport":"streamable-http","url":format!("{base}{MCP_PATH}"),
        "requestUrl":format!("{base}{REQUEST_PATH}"),"schemaUrl":format!("{base}{SCHEMA_PATH}"),
        "headers":{"Authorization":format!("Bearer {}",bearer(&info.token))},"tool":"agent_client",
        "javascript":directory.join("client.mjs"),"instructions":INSTRUCTIONS})
}

fn publish_file(directory: &FilePath, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let target = directory.join(name);
    if let Ok(metadata) = std::fs::symlink_metadata(&target) {
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Agent connection assets must be regular files"
        );
    }
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(target)?;
    Ok(())
}

/// Called by normal service startup, before the tunnel starts. No client setup
/// or manual export step is needed, and the helper follows updated ports.
pub(super) fn publish(
    state: &OwnerState,
    info: &RuntimeInfo,
    directory: &FilePath,
) -> anyhow::Result<Option<PathBuf>> {
    let path = directory.join("connection.json");
    if !enabled(state) && !path.exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(directory)?;
    anyhow::ensure!(
        std::fs::symlink_metadata(directory)?.is_dir()
            && !std::fs::symlink_metadata(directory)?
                .file_type()
                .is_symlink(),
        "Agent connection directory must not be a symlink"
    );
    if !enabled(state) {
        publish_file(
            directory,
            "connection.json",
            br#"{"version":1,"enabled":false}"#,
        )?;
        return Ok(None);
    }
    publish_file(directory, "client.mjs", JAVASCRIPT.as_bytes())?;
    publish_file(
        directory,
        "connection.json",
        &serde_json::to_vec_pretty(&descriptor(info, directory))?,
    )?;
    Ok(Some(path))
}

/// Explicit local export of a scoped credential; never called by an MCP tool.
pub async fn connection_profile() -> anyhow::Result<String> {
    connection_profile_at(&runtime_path()).await
}

pub(super) async fn connection_profile_at(path: &FilePath) -> anyhow::Result<String> {
    crate::tls::ensure_crypto_provider();
    let info =
        read_runtime(path)?.context("Start the Codexify service with agentChat enabled first")?;
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?
        .get(format!("http://127.0.0.1:{}/api/agent-client", info.port))
        .bearer_auth(&info.token)
        .send()
        .await?;
    anyhow::ensure!(
        response.status().is_success(),
        "Agent control is unavailable; enable Browser automation, Agent teams and AI agent control, restart, then try again"
    );
    anyhow::ensure!(
        response.json::<Value>().await?["enabled"] == true,
        "Unexpected coding-app connection response"
    );
    Ok(serde_json::to_string_pretty(&descriptor(
        &info,
        &path
            .parent()
            .context("No runtime directory")?
            .join("agents"),
    ))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn universal_agent_connection_has_no_client_profiles() {
        use clap::Parser;
        assert!(crate::config::Cli::try_parse_from(["codexify", "agents", "connection"]).is_ok());
        assert!(crate::config::Cli::try_parse_from(["codexify", "agents", "instructions"]).is_ok());
        assert!(
            crate::config::Cli::try_parse_from([
                "codexify",
                "agents",
                "connection",
                "--format",
                "app"
            ])
            .is_err()
        );
        let info = RuntimeInfo {
            port: 12345,
            token: crate::auth::generate_internal_bearer_token().unwrap(),
        };
        let token = bearer(&info.token);
        assert_ne!(token, info.token);
        assert_eq!(token, bearer(&info.token));
        let parsed = descriptor(&info, FilePath::new("agents"));
        assert_eq!(parsed["url"], "http://127.0.0.1:12345/mcp/agents");
        assert_eq!(
            parsed["headers"]["Authorization"],
            format!("Bearer {token}")
        );
        assert!(
            parsed["javascript"]
                .as_str()
                .unwrap()
                .ends_with("client.mjs")
        );
        assert!(!parsed.to_string().contains(&info.token));
        assert!(INSTRUCTIONS.len() < 1000);
    }
}
