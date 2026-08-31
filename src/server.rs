//! MCP server: the [`CodexHandler`] that implements rmcp's `ServerHandler`, plus
//! the axum wiring (`/mcp` Streamable HTTP, `/health`, CORS, bearer auth).
//!
//! Ports `src/server.ts`. rmcp's `StreamableHttpService` owns the transport and
//! MCP session lifecycle: the service factory runs once per transport session.
//! Generic clients keep resident commands in their transport [`SessionState`],
//! while ChatGPT's stable conversation metadata selects server-owned command
//! state and diff checkpoints that survive transport replacement. The embedded
//! UI resources remain presentation-only.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::{Router, extract::State, http::StatusCode, response::Json, routing::get};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        ExtensionCapabilities, Implementation, InitializeResult, ListPromptsResult,
        ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
        ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
        ServerCapabilities, ServerInfo,
    },
    service::{RequestContext, RoleServer},
    transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{Any, CorsLayer};

use crate::artifact_egress::{ARTIFACT_RESOURCE_URI_PREFIX, ArtifactEgressStore};
use crate::audit::{
    AuditLogger, AuditScope, argument_field_names, summarize_arguments, summarize_output,
};
use crate::auth::{generate_internal_bearer_token, require_auth};
use crate::bridged_resources::{BRIDGED_RESOURCE_URI_PREFIX, BridgedResourceStore};
use crate::conversation_auth::{AUTHORIZATION_TOOL_WIRE_NAME, ConversationAuthorizationStore};
use crate::diff::{DiffAvailability, DiffCheckpointManager, DiffOwner};
use crate::diff_ui;
use crate::exec_sessions::{ConversationExecSessionStore, SessionState};
use crate::instructions::build_initial_instructions;
use crate::openai_tunnel::TunnelHealth;
use crate::output_budget::{
    enforce_tool_result_budget, fill_text_mirror_with_budget, tool_output_token_budget,
};
use crate::project_bindings::{ConversationIdentity, ProjectBindingStore};
use crate::registry::load_tools_for_config;
use crate::self_update_ui;
use crate::setup_ui;
use crate::tool::{
    Tool, ToolCallIdentity, ToolRequestContext, validate_and_wrap_tool, validate_and_wrap_tools,
};
use crate::tool_logging::ToolCallLogger;
use crate::tools::set_project_root::{
    ProjectSelection, ProjectSelectionRequest, SetProjectRoot, select_and_render,
};
use crate::types::{AppConfig, ToolContent, ToolResult};
use crate::widget_debug;

const HTTP_SERVER_STOP_TIMEOUT: Duration = Duration::from_secs(10);
// Connector refreshes must observe the current configuration and embedded UI,
// so modern clients may store these responses but must consider them stale at once.
const DEFAULT_CACHE_TTL_MS: u64 = 0;

fn should_emit_ordinary_tool_completion(
    tool_log_call: Option<&crate::tool_logging::ToolLogCall>,
) -> bool {
    !tool_log_call.is_some_and(crate::tool_logging::ToolLogCall::events_enabled)
}

fn ensure_modern_cache_hints(
    protocol_version: Option<&ProtocolVersion>,
    ttl_ms: &mut Option<u64>,
    cache_scope: &mut Option<CacheScope>,
) {
    if protocol_version.is_some_and(|version| version >= &ProtocolVersion::V_2026_07_28) {
        ttl_ms.get_or_insert(DEFAULT_CACHE_TTL_MS);
        cache_scope.get_or_insert(CacheScope::Private);
    }
}

// Tunnel supervision (native OpenAI tunnel mode). The HTTP server stays up across
// tunnel restarts; only the outbound tunnel-client process is replaced.
/// How often the running tunnel's local health endpoints are probed.
const TUNNEL_HEALTH_INTERVAL: Duration = Duration::from_secs(10);
/// Consecutive failed health probes that mark the tunnel dead and trigger a restart.
const TUNNEL_HEALTH_FAIL_THRESHOLD: u32 = 3;
/// Circuit breaker: more than this many restarts inside [`TUNNEL_BREAKER_WINDOW`]
/// means the tunnel is flapping unrecoverably, so supervision gives up.
const TUNNEL_BREAKER_MAX_RESTARTS: usize = 5;
const TUNNEL_BREAKER_WINDOW: Duration = Duration::from_secs(60);
/// Backoff before a restart grows by this step per attempt in the current window,
/// capped at [`TUNNEL_RESTART_BACKOFF_MAX`].
const TUNNEL_RESTART_BACKOFF_STEP: Duration = Duration::from_secs(1);
const TUNNEL_RESTART_BACKOFF_MAX: Duration = Duration::from_secs(5);

type HttpServerTask = JoinHandle<Result<(), std::io::Error>>;

/// One MCP session: shared config and tool registry, plus this session's own
/// mutable state.
pub struct CodexHandler {
    config: Arc<AppConfig>,
    tools: Arc<Vec<Box<dyn Tool>>>,
    project_bindings: Arc<ProjectBindingStore>,
    conversation_authorizations: Arc<ConversationAuthorizationStore>,
    conversation_exec_sessions: Arc<ConversationExecSessionStore>,
    diff_checkpoints: Arc<DiffCheckpointManager>,
    artifact_egress: Arc<ArtifactEgressStore>,
    bridged_resources: Arc<BridgedResourceStore>,
    audit: Option<Arc<AuditLogger>>,
    tool_logging: Option<Arc<ToolCallLogger>>,
    next_tool_call_id: Arc<AtomicU64>,
    session: SessionState,
}

impl CodexHandler {
    fn selected_project_root(
        &self,
        conversation: Option<&ConversationIdentity>,
    ) -> Option<PathBuf> {
        if !self.config.multi_project {
            return Some(self.config.work_dir.clone());
        }
        match conversation {
            Some(identity) => self
                .project_bindings
                .selected_project_root(&self.config, identity)
                .ok()
                .flatten(),
            None => self.session.selected_project_root(),
        }
    }

    fn audit_scope(&self, conversation: Option<&ConversationIdentity>) -> AuditScope {
        let project_root = self.selected_project_root(conversation);
        AuditScope::new(
            self.session.audit_id(),
            conversation,
            &self.config.work_dir,
            project_root.as_deref(),
        )
    }

    fn conversation_auth_error(
        &self,
        tool_name: &str,
        conversation: Option<&ConversationIdentity>,
    ) -> Option<ToolResult> {
        if self.config.conversation_auth_token.is_none()
            || tool_name == AUTHORIZATION_TOOL_WIRE_NAME
            || self
                .conversation_authorizations
                .is_authorized(conversation, &self.session)
        {
            return None;
        }

        let scope = if conversation.is_some() {
            "This ChatGPT conversation"
        } else {
            "This MCP transport session"
        };
        // This authorization failure is model-facing, so it intentionally names
        // the gate only by its innocuous `setup(ref)` wire contract.
        Some(ToolResult::error(format!(
            "{scope} has not completed connector setup. Call the `setup` tool once with the configured `ref`, then retry."
        )))
    }
}

/// Convert this repo's [`ToolResult`] into rmcp's `CallToolResult`.
fn to_call_tool_result(result: ToolResult) -> CallToolResult {
    let blocks: Vec<ContentBlock> = result
        .content
        .into_iter()
        .map(|c| match c {
            ToolContent::Text(t) => ContentBlock::text(t),
            ToolContent::Image { data, mime_type } => ContentBlock::image(data, mime_type),
            ToolContent::ResourceLink(resource) => ContentBlock::resource_link(resource),
        })
        .collect();

    let mut ctr = if result.is_error {
        CallToolResult::error(blocks)
    } else {
        CallToolResult::success(blocks)
    };
    ctr.structured_content = result.structured_content;
    ctr.meta = result.meta;
    ctr
}

fn finalize_model_visible_result(
    tool: Option<&dyn Tool>,
    result: &mut ToolResult,
    config: &AppConfig,
) {
    let budget = tool_output_token_budget(config);
    let manages_budget = tool.is_some_and(Tool::manages_model_output_budget);
    if !manages_budget {
        enforce_tool_result_budget(result, budget);
    }

    let should_fill = tool.is_some_and(|tool| {
        tool.output_schema().is_some()
            && tool.fills_structured_content()
            && !result.is_error
            && result.structured_content.is_none()
    });
    if should_fill {
        if manages_budget {
            result.structured_content = Some(json!({ "content": result.joined_text() }));
        } else {
            fill_text_mirror_with_budget(result, budget);
        }
    }
}

fn advertised_tool(tool: &dyn Tool, config: &AppConfig) -> rmcp::model::Tool {
    let schema = tool.input_schema().as_object().cloned().unwrap_or_default();
    let mut advertised = rmcp::model::Tool::new(tool.name(), tool.describe(config), schema)
        .with_title(tool.title())
        .with_annotations(
            tool.annotations()
                .unwrap_or_else(|| tool.behavior().annotations()),
        );
    if let Some(icons) = tool.icons() {
        advertised = advertised.with_icons(icons);
    }
    if let Some(meta) = tool.meta() {
        advertised = advertised.with_meta(meta);
    }
    if let Some(output) = tool.output_schema()
        && let Some(object) = output.as_object()
    {
        advertised = advertised.with_raw_output_schema(Arc::new(object.clone()));
    }
    advertised
}

fn builtin_ui_resources() -> Vec<rmcp::model::Resource> {
    vec![
        diff_ui::resource(),
        self_update_ui::resource(),
        setup_ui::resource(),
    ]
}

fn builtin_ui_contents(uri: &str) -> Option<rmcp::model::ResourceContents> {
    diff_ui::contents_for_uri(uri)
        .or_else(|| self_update_ui::contents_for_uri(uri))
        .or_else(|| setup_ui::contents_for_uri(uri))
}

impl ServerHandler for CodexHandler {
    fn get_info(&self) -> ServerInfo {
        let mut capabilities = ServerCapabilities::builder()
            .enable_resources()
            .enable_tools()
            .build();
        let mut extensions = ExtensionCapabilities::new();
        extensions.insert(
            diff_ui::MCP_APPS_EXTENSION_ID.to_string(),
            json!({ "mimeTypes": [diff_ui::DIFF_UI_MIME_TYPE] })
                .as_object()
                .cloned()
                .expect("static MCP Apps capability must be an object"),
        );
        capabilities.extensions = Some(extensions);
        InitializeResult::new(capabilities)
            .with_server_info(Implementation::new("codexify", env!("CARGO_PKG_VERSION")))
            .with_instructions(build_initial_instructions(&self.config))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = self
            .tools
            .iter()
            .map(|tool| advertised_tool(tool.as_ref(), &self.config))
            .collect();
        let mut result = ListToolsResult::with_all_items(tools);
        ensure_modern_cache_hints(
            context.protocol_version().as_ref(),
            &mut result.ttl_ms,
            &mut result.cache_scope,
        );
        Ok(result)
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        let mut result = ListPromptsResult::with_all_items(Vec::new());
        ensure_modern_cache_hints(
            context.protocol_version().as_ref(),
            &mut result.ttl_ms,
            &mut result.cache_scope,
        );
        Ok(result)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let mut result = ListResourcesResult::with_all_items(builtin_ui_resources());
        ensure_modern_cache_hints(
            context.protocol_version().as_ref(),
            &mut result.ttl_ms,
            &mut result.cache_scope,
        );
        Ok(result)
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let mut result = ListResourceTemplatesResult::with_all_items(Vec::new());
        ensure_modern_cache_hints(
            context.protocol_version().as_ref(),
            &mut result.ttl_ms,
            &mut result.cache_scope,
        );
        Ok(result)
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let protocol_version = context.protocol_version();
        match self
            .bridged_resources
            .read_resource(&request.uri, &context.ct)
            .await
        {
            Ok(Some(mut result)) => {
                ensure_modern_cache_hints(
                    protocol_version.as_ref(),
                    &mut result.ttl_ms,
                    &mut result.cache_scope,
                );
                return Ok(result.into());
            }
            Ok(None) => {}
            Err(error) => {
                return Err(McpError::internal_error(error.to_string(), None));
            }
        }
        if request.uri.starts_with(BRIDGED_RESOURCE_URI_PREFIX) {
            return Err(McpError::resource_not_found(
                "Unknown or expired bridged MCP resource".to_string(),
                None,
            ));
        }
        match self
            .artifact_egress
            .read_resource(&request.uri, &context.ct)
            .await
        {
            Ok(Some(contents)) => {
                let mut result = ReadResourceResult::new(vec![contents]);
                ensure_modern_cache_hints(
                    protocol_version.as_ref(),
                    &mut result.ttl_ms,
                    &mut result.cache_scope,
                );
                return Ok(result.into());
            }
            Ok(None) => {}
            Err(error) => {
                return Err(McpError::internal_error(error.to_string(), None));
            }
        }
        if request.uri.starts_with(ARTIFACT_RESOURCE_URI_PREFIX) {
            return Err(McpError::resource_not_found(
                "Unknown or unavailable exported-file resource".to_string(),
                None,
            ));
        }
        let Some(contents) = builtin_ui_contents(&request.uri) else {
            return Err(McpError::resource_not_found(
                format!("Unknown resource: {}", request.uri),
                None,
            ));
        };
        let mut result = ReadResourceResult::new(vec![contents]);
        ensure_modern_cache_hints(
            protocol_version.as_ref(),
            &mut result.ttl_ms,
            &mut result.cache_scope,
        );
        Ok(result.into())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let conversation = ConversationIdentity::from_request_meta(&context.meta).or_else(|| {
            request
                .meta
                .as_ref()
                .and_then(ConversationIdentity::from_request_meta)
        });
        let name = request.name.as_ref().to_string();
        let args = request
            .arguments
            .map(Value::Object)
            .unwrap_or_else(|| json!({}));
        let call_id = self.next_tool_call_id.fetch_add(1, Ordering::Relaxed);
        let tool_context = ToolRequestContext {
            conversation: conversation.clone(),
            conversation_authorizations: self.conversation_authorizations.clone(),
            project_bindings: self.project_bindings.clone(),
            diff_checkpoints: self.diff_checkpoints.clone(),
            artifact_egress: self.artifact_egress.clone(),
            cancellation: context.ct.clone(),
        };

        // Keep `tool` as an Option so that even an unknown-tool call flows through
        // the audit begin/finish pairing below rather than short-circuiting.
        let tool = self.tools.iter().find(|t| t.name() == name);
        let call_identity = tool
            .map(|tool| tool.call_identity(&args))
            .unwrap_or_else(|| ToolCallIdentity::native(name.clone()));

        // Verbose-diagnostics / audit preamble (#15). `needs_scope` gates the cost
        // of building an audit scope and argument summaries to the cases that will
        // actually consume them.
        let needs_scope = self.audit.is_some()
            || tracing::enabled!(target: "codexify::tool", tracing::Level::DEBUG);
        let needs_input_schema = needs_scope
            || self
                .tool_logging
                .as_ref()
                .is_some_and(|logger| logger.logs_requests());
        let input_schema = needs_input_schema
            .then(|| tool.map(|tool| tool.input_schema()))
            .flatten();
        let tool_log_call = self
            .tool_logging
            .as_ref()
            .map(|logger| logger.begin(call_id, &call_identity, &args, input_schema.as_ref()));
        let start_scope = needs_scope.then(|| self.audit_scope(conversation.as_ref()));
        let audit_call = self.audit.as_ref().and_then(|audit| {
            start_scope.as_ref().map(|scope| {
                audit.begin_tool(call_id, &call_identity, &args, input_schema.as_ref(), scope)
            })
        });
        if let Some(scope) = start_scope.as_ref() {
            tracing::debug!(
                target: "codexify::tool",
                call_id,
                tool = %name,
                resolved_tool = %call_identity.resolved_tool(),
                mcp_server = call_identity.mcp_server.as_deref().unwrap_or("-"),
                mcp_tool = call_identity.mcp_tool.as_deref().unwrap_or("-"),
                transport_session_id = scope.transport_session_id,
                conversation_id = scope.conversation_id.as_deref().unwrap_or("-"),
                project_id = scope.project_id.as_deref().unwrap_or("-"),
                argument_fields = %argument_field_names(&args, input_schema.as_ref()),
                "tool started"
            );
        }
        if tracing::enabled!(target: "codexify::tool", tracing::Level::TRACE) {
            tracing::trace!(
                target: "codexify::tool",
                call_id,
                tool = %name,
                resolved_tool = %call_identity.resolved_tool(),
                mcp_server = call_identity.mcp_server.as_deref().unwrap_or("-"),
                mcp_tool = call_identity.mcp_tool.as_deref().unwrap_or("-"),
                argument_summary = %summarize_arguments(&args, input_schema.as_ref()),
                "tool arguments summarized"
            );
        }

        let started = Instant::now();
        let mut result = if let Some(error) =
            self.conversation_auth_error(&name, conversation.as_ref())
        {
            error
        } else if let Some(error) = tool.and_then(|tool| tool.validate_arguments(&args).err()) {
            ToolResult::error(format!("Invalid arguments for `{name}`: {error}"))
        } else {
            // A ChatGPT conversation gets its own exec-session view (#12); generic MCP
            // clients fall back to the transport-owned session.
            let conversation_exec_session = if tool.is_some_and(|t| t.uses_exec_session_state()) {
                conversation.as_ref().map(|identity| {
                    self.conversation_exec_sessions
                        .session_for(identity, &self.session)
                })
            } else {
                None
            };
            let session = conversation_exec_session.as_ref().unwrap_or(&self.session);

            match tool {
                None => ToolResult::error(format!("Unknown tool: {name}")),
                Some(tool) if name == SetProjectRoot::NAME => {
                    if let Some(identity) = conversation.as_ref() {
                        select_and_render(&args, |request| async move {
                            match request {
                                ProjectSelectionRequest::Project(path) => self
                                    .project_bindings
                                    .select_project_root(&self.config, identity, &path)
                                    .await
                                    .map(ProjectSelection::Project),
                                ProjectSelectionRequest::WithoutProject => self
                                    .project_bindings
                                    .select_without_project(&self.config, identity)
                                    .await
                                    .map(ProjectSelection::WithoutProject),
                            }
                        })
                        .await
                    } else {
                        tool.call_with_context(args, &self.config, session, &tool_context)
                            .await
                    }
                }
                Some(tool) if tool.requires_project_root() => {
                    let resolved = match conversation.as_ref() {
                        Some(identity) => self
                            .project_bindings
                            .effective_config(&self.config, identity),
                        None => session.effective_config(&self.config),
                    };
                    match resolved {
                        Err(error) => ToolResult::error(error),
                        Ok(effective_config) => {
                            let owner = match conversation.as_ref() {
                                Some(identity) => DiffOwner::conversation(identity),
                                None => DiffOwner::transport(self.session.diff_state()),
                            };
                            if tool.may_modify_project() {
                                // Fail closed: refuse a mutating tool when the diff
                                // checkpoint cannot be captured. The refusal is returned
                                // as a value (not an early return) so the audit
                                // finish_tool below still pairs with begin_tool.
                                match self
                                    .diff_checkpoints
                                    .begin_mutation(&effective_config, owner)
                                    .await
                                {
                                    Ok((availability, guard)) => {
                                        if let DiffAvailability::Unavailable(reason) = &availability
                                        {
                                            tracing::debug!(
                                                "diff checkpoints unavailable for {name}: {reason}"
                                            );
                                        }
                                        let result = tool
                                            .call_with_context(
                                                args,
                                                &effective_config,
                                                session,
                                                &tool_context,
                                            )
                                            .await;
                                        drop(guard);
                                        result
                                    }
                                    Err(error) => ToolResult::error(format!(
                                        "Refusing to run mutating tool `{name}` because the project diff checkpoint could not be captured: {error}"
                                    )),
                                }
                            } else {
                                match self
                                    .diff_checkpoints
                                    .ensure_initialized(&effective_config, owner)
                                    .await
                                {
                                    Ok(DiffAvailability::Ready) => {}
                                    Ok(DiffAvailability::Unavailable(reason)) => {
                                        tracing::debug!(
                                            "diff checkpoints unavailable for {name}: {reason}"
                                        );
                                    }
                                    Err(error) => {
                                        tracing::debug!(
                                            "diff checkpoint initialization skipped for {name}: {error}"
                                        );
                                    }
                                }
                                tool.call_with_context(
                                    args,
                                    &effective_config,
                                    session,
                                    &tool_context,
                                )
                                .await
                            }
                        }
                    }
                }
                Some(tool) => {
                    tool.call_with_context(args, &self.config, session, &tool_context)
                        .await
                }
            }
        };

        finalize_model_visible_result(tool.map(|tool| tool.as_ref()), &mut result, &self.config);
        if !result.is_error
            && let Some(tool) = tool
            && let Err(error) = tool.validate_structured_output(result.structured_content.as_ref())
        {
            tracing::error!(tool = %name, %error, "tool returned structured content that violates its output schema");
            result = ToolResult::error(format!(
                "Tool `{name}` returned structured content that violates its declared output schema: {error}"
            ));
        }

        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        widget_debug::attach_configured_tool_timing(&self.config, &mut result, &name, duration_ms);
        if let (Some(logger), Some(call)) = (&self.tool_logging, tool_log_call.as_ref()) {
            logger.finish(call, &call_identity, &result, duration_ms);
        }
        if should_emit_ordinary_tool_completion(tool_log_call.as_ref()) {
            tracing::info!(
                target: "codexify::tool",
                call_id,
                tool = %name,
                resolved_tool = %call_identity.resolved_tool(),
                mcp_server = call_identity.mcp_server.as_deref().unwrap_or("-"),
                mcp_tool = call_identity.mcp_tool.as_deref().unwrap_or("-"),
                status = if result.is_error { "error" } else { "ok" },
                duration_ms,
                "tool completed"
            );
        }
        if tracing::enabled!(target: "codexify::tool", tracing::Level::DEBUG) {
            tracing::debug!(
                target: "codexify::tool",
                call_id,
                tool = %name,
                resolved_tool = %call_identity.resolved_tool(),
                mcp_server = call_identity.mcp_server.as_deref().unwrap_or("-"),
                mcp_tool = call_identity.mcp_tool.as_deref().unwrap_or("-"),
                output_summary = %summarize_output(&result),
                "tool output summarized"
            );
        }
        if let (Some(audit), Some(call), Some(start_scope)) =
            (&self.audit, audit_call.as_ref(), start_scope.as_ref())
        {
            if name == SetProjectRoot::NAME {
                let finish_scope = self.audit_scope(conversation.as_ref());
                audit.finish_tool(call, &call_identity, &result, duration_ms, &finish_scope);
            } else {
                audit.finish_tool(call, &call_identity, &result, duration_ms, start_scope);
            }
        }
        Ok(to_call_tool_result(result).into())
    }
}

#[derive(Clone)]
struct HealthState {
    tool_count: usize,
    ready: Arc<AtomicBool>,
}

async fn health(State(state): State<HealthState>) -> (StatusCode, Json<Value>) {
    if state.ready.load(Ordering::Acquire) {
        (
            StatusCode::OK,
            Json(json!({ "status": "ok", "tools": state.tool_count })),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "starting", "tools": state.tool_count })),
        )
    }
}

/// Build the axum app and serve it. Ports `startHttpServer`.
pub async fn start_http_server(mut config: AppConfig) -> anyhow::Result<()> {
    let native_tunnel = config.openai_tunnel.is_some();
    if native_tunnel {
        if config.api_key.is_some() {
            anyhow::bail!("native tunnel mode cannot use a caller-supplied local MCP API key");
        }
        config.api_key = Some(generate_internal_bearer_token()?);
    }
    let audit = AuditLogger::open(&config)?.map(Arc::new);
    let tool_logging = ToolCallLogger::new(&config).map(Arc::new);
    let next_tool_call_id = Arc::new(AtomicU64::new(1));
    // Gateway-mode upstreams write their generated skills here; keyed by port so
    // concurrent instances don't clobber each other, and rebuilt fresh per start.
    let gen_dir = std::env::temp_dir()
        .join("codexify-gateway-skills")
        .join(config.port.to_string());
    let _ = std::fs::remove_dir_all(&gen_dir);
    config.generated_skills_dir = Some(gen_dir);
    let project_bindings = Arc::new(ProjectBindingStore::for_current_user());
    let conversation_exec_sessions = Arc::new(ConversationExecSessionStore::new());
    conversation_exec_sessions
        .spawn_idle_reaper(Duration::from_millis(config.exec.idle_timeout_ms));
    let diff_checkpoints = Arc::new(DiffCheckpointManager::new());
    let artifact_egress = Arc::new(
        ArtifactEgressStore::new(config.artifact_egress.clone())
            .map_err(|error| anyhow::anyhow!("initialize artifact egress: {error}"))?,
    );
    let conversation_authorizations =
        Arc::new(ConversationAuthorizationStore::for_current_user(&config));

    // Sweep managed worktrees left by conversations that are no longer bound
    // before taking ownership of `config` behind an `Arc`.
    if config.multi_project && config.worktrees.auto_cleanup_enabled {
        match project_bindings.referenced_managed_project_roots(&config) {
            Ok(referenced) => {
                for warning in
                    crate::worktrees::cleanup_managed_worktrees(&config, &referenced).await
                {
                    tracing::warn!("managed-worktree cleanup: {warning}");
                }
            }
            Err(error) => tracing::warn!("managed-worktree cleanup skipped: {error}"),
        }
    }
    let config = Arc::new(config);

    // Connect to any configured upstream MCP servers and merge their tools in.
    // The returned services must stay alive for the whole server lifetime, so
    // they are held in `_bridge_services` until `axum::serve` returns.
    let bridge = crate::bridge::connect_upstreams(&config).await;
    let bridge_report = bridge.report;
    let bridged_resources = bridge.resources;
    let _bridge_services = bridge.services;

    let mut all_tools = validate_and_wrap_tools(load_tools_for_config(&config))
        .map_err(|error| anyhow::anyhow!("invalid native tool contract: {error}"))?;
    let native: std::collections::HashSet<&'static str> =
        all_tools.iter().map(|t| t.name()).collect();
    let mut seen = native.clone();
    for tool in bridge.tools {
        let name = tool.name();
        if seen.contains(name) {
            tracing::warn!("bridged tool '{name}' collides with an existing tool; skipping");
            continue;
        }
        match validate_and_wrap_tool(tool) {
            Ok(tool) => {
                seen.insert(name);
                all_tools.push(tool);
            }
            Err(error) => {
                tracing::warn!(
                    "bridged tool '{name}' has an invalid contract and was skipped: {error}"
                );
            }
        }
    }
    let bridged_count = all_tools.len() - native.len();

    let tools = Arc::new(all_tools);
    let tool_count = tools.len();
    let ready = Arc::new(AtomicBool::new(!native_tunnel));

    // Streamable HTTP transport config. `json_response` mirrors the TS
    // `enableJsonResponse: true` so simple request/response tools return
    // `application/json` rather than SSE.
    let mut http_config =
        rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default();
    http_config.json_response = true;
    let mcp_cancellation = http_config.cancellation_token.clone();
    // The original bridge accepted any Host so it works behind a tunnel that
    // presents an arbitrary hostname. Preserve that unless the operator lists
    // explicit hosts in the config.
    if native_tunnel {
        http_config.allowed_hosts = vec!["127.0.0.1".into(), "localhost".into(), "::1".into()];
    } else if config.allowed_hosts.is_empty() {
        http_config.allowed_hosts.clear();
    } else {
        http_config.allowed_hosts = config.allowed_hosts.clone();
    }

    let factory_config = config.clone();
    let factory_tools = tools.clone();
    let factory_project_bindings = project_bindings.clone();
    let factory_conversation_authorizations = conversation_authorizations.clone();
    let factory_conversation_exec_sessions = conversation_exec_sessions.clone();
    let factory_diff_checkpoints = diff_checkpoints.clone();
    let factory_artifact_egress = artifact_egress.clone();
    let factory_bridged_resources = bridged_resources.clone();
    let factory_audit = audit.clone();
    let factory_tool_logging = tool_logging.clone();
    let factory_next_tool_call_id = next_tool_call_id.clone();
    let service = StreamableHttpService::new(
        move || {
            let session = SessionState::new();
            session.spawn_idle_reaper(Duration::from_millis(factory_config.exec.idle_timeout_ms));
            Ok(CodexHandler {
                config: factory_config.clone(),
                tools: factory_tools.clone(),
                project_bindings: factory_project_bindings.clone(),
                conversation_authorizations: factory_conversation_authorizations.clone(),
                conversation_exec_sessions: factory_conversation_exec_sessions.clone(),
                diff_checkpoints: factory_diff_checkpoints.clone(),
                artifact_egress: factory_artifact_egress.clone(),
                bridged_resources: factory_bridged_resources.clone(),
                audit: factory_audit.clone(),
                tool_logging: factory_tool_logging.clone(),
                next_tool_call_id: factory_next_tool_call_id.clone(),
                session,
            })
        },
        Arc::new(LocalSessionManager::default()),
        http_config,
    );

    let app = Router::new()
        .route("/health", get(health))
        .with_state(HealthState {
            tool_count,
            ready: ready.clone(),
        })
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(
            config.clone(),
            require_auth,
        ));
    let app = if native_tunnel {
        app
    } else {
        app.layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any)
                .expose_headers([axum::http::HeaderName::from_static("mcp-session-id")]),
        )
    };

    let bind_host = if native_tunnel {
        "127.0.0.1"
    } else {
        "0.0.0.0"
    };
    let listener = tokio::net::TcpListener::bind((bind_host, config.port)).await?;

    println!(
        "\nCodexify MCP Bridge (Rust) running on http://{}:{}",
        if native_tunnel {
            "127.0.0.1"
        } else {
            "localhost"
        },
        config.port,
    );
    if config.multi_project {
        println!("Project access root: {}", config.work_dir.display());
        println!("Project mode: persistent ChatGPT conversation binding");
        println!(
            "Conversation bindings: {}",
            project_bindings.base_dir().display()
        );
    } else {
        println!("Work directory: {}", config.work_dir.display());
    }
    println!(
        "Tools loaded ({tool_count}): {} native + {bridged_count} upstream-facing MCP tools",
        tool_count - bridged_count
    );
    if !bridge_report.is_empty() {
        println!("Upstream MCP servers:");
        for line in &bridge_report {
            println!("  {line}");
        }
    }
    if native_tunnel {
        println!("Local MCP auth: enabled (private per-process bearer token)");
    } else if config.api_key.is_some() {
        println!("Auth: enabled (bearer token)");
    } else {
        println!("Auth: disabled (no --api-key)");
    }
    if config.conversation_auth_token.is_some() {
        println!("Conversation authorization: enabled (one token check per chat)");
    } else {
        println!("Conversation authorization: disabled");
    }
    if let Some(logger) = tool_logging.as_ref() {
        println!(
            "Tool payload logging: {} at {} (requests <= {} bytes, responses <= {} bytes; redacted)",
            logger.mode().as_str(),
            logger.level().as_str(),
            logger.max_request_bytes(),
            logger.max_response_bytes()
        );
    } else {
        println!("Tool payload logging: disabled");
    }
    if let Some(audit) = audit.as_ref() {
        println!("Audit log: {}", audit.path().display());
        println!(
            "Audit command previews: {}",
            if audit.command_previews_enabled() {
                "enabled (bounded and redacted)"
            } else {
                "disabled"
            }
        );
    }
    if !native_tunnel {
        println!("\nAdd to ChatGPT > Plugins > New Plugin:");
        println!("  Server URL: https://<your-tunnel>/mcp\n");
        axum::serve(listener, app).await?;
        return Ok(());
    }

    println!("Exposure: loopback only; starting OpenAI Secure MCP Tunnel");
    run_with_openai_tunnel(listener, app, config, mcp_cancellation, ready).await
}

/// What ended a supervision cycle for a running tunnel. Terminal events (server
/// exit, shutdown signal) are handled inline in the select and never surface here.
enum SuperviseEvent {
    /// The tunnel process exited on its own; the string is the reason.
    Died(String),
    /// The health-probe interval fired; the tunnel must be probed.
    HealthTick,
}

/// Records a restart at `now` in the sliding window, evicting entries older than
/// [`TUNNEL_BREAKER_WINDOW`], and returns the attempt count (window length). The
/// caller trips the circuit breaker once this reaches [`TUNNEL_BREAKER_MAX_RESTARTS`].
fn record_restart(window: &mut Vec<Instant>, now: Instant) -> usize {
    window.retain(|at| now.duration_since(*at) < TUNNEL_BREAKER_WINDOW);
    window.push(now);
    window.len()
}

/// Backoff before the `attempt`-th restart: linear in the attempt, capped.
fn restart_backoff(attempt: usize) -> Duration {
    (TUNNEL_RESTART_BACKOFF_STEP * attempt as u32).min(TUNNEL_RESTART_BACKOFF_MAX)
}

async fn run_with_openai_tunnel(
    listener: tokio::net::TcpListener,
    app: Router,
    config: Arc<AppConfig>,
    mcp_cancellation: CancellationToken,
    ready: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let mut shutdown_tx = Some(shutdown_tx);
    let mut server_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
    });

    tokio::task::yield_now().await;
    let start_tunnel = crate::openai_tunnel::start(&config);
    tokio::pin!(start_tunnel);

    let mut tunnel = tokio::select! {
        server_result = &mut server_task => {
            return flatten_server_result(server_result);
        }
        tunnel_result = &mut start_tunnel => {
            match tunnel_result {
                Ok(tunnel) => tunnel,
                Err(error) => {
                    stop_http_server(
                        &mut shutdown_tx,
                        &mcp_cancellation,
                        server_task,
                        HTTP_SERVER_STOP_TIMEOUT,
                    ).await?;
                    return Err(error.context("start OpenAI Secure MCP Tunnel"));
                }
            }
        }
        _ = shutdown_signal() => {
            stop_http_server(
                &mut shutdown_tx,
                &mcp_cancellation,
                server_task,
                HTTP_SERVER_STOP_TIMEOUT,
            ).await?;
            return Ok(());
        }
    };

    ready.store(true, Ordering::Release);
    println!("OpenAI Secure MCP Tunnel: ready");
    if config
        .openai_tunnel
        .as_ref()
        .is_some_and(|settings| settings.client_path.is_some())
    {
        println!("Tunnel runtime: operator-supplied compatible tunnel client");
    } else {
        println!(
            "Tunnel runtime: managed OpenAI tunnel-client-runtime v{}",
            crate::openai_tunnel::TUNNEL_CLIENT_VERSION
        );
    }
    println!("Tunnel readiness: {}/readyz", tunnel.health_url());
    println!("Tunnel metrics: {}/metrics", tunnel.health_url());
    println!("\nAdd a ChatGPT developer-mode connector/plugin:");
    println!("  Connection type: Tunnel");
    println!("  Tunnel: select the tunnel configured for this process");
    println!("  Authentication: None");
    println!("  Permissions: Allow all actions\n");

    // Supervise the tunnel. The HTTP server keeps running across restarts; only
    // the outbound tunnel-client process is replaced. A restart is triggered by
    // the process exiting (#10) or by consecutive health-probe failures (#11),
    // and is bounded by a circuit breaker so a flapping tunnel cannot loop
    // forever.
    let health_client = crate::openai_tunnel::build_health_client()?;
    let mut health_interval = tokio::time::interval(TUNNEL_HEALTH_INTERVAL);
    health_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    health_interval.tick().await; // consume the immediate first tick
    let mut consecutive_failures: u32 = 0;
    let mut restart_window: Vec<Instant> = Vec::new();

    loop {
        let event = tokio::select! {
            server_result = &mut server_task => {
                let shutdown_result = tunnel.shutdown().await;
                flatten_server_result(server_result)?;
                return shutdown_result;
            }
            tunnel_error = tunnel.wait_for_exit() => SuperviseEvent::Died(format!("{tunnel_error:#}")),
            _ = health_interval.tick() => SuperviseEvent::HealthTick,
            _ = shutdown_signal() => {
                let (tunnel_result, server_result) = tokio::join!(
                    tunnel.shutdown(),
                    stop_http_server(
                        &mut shutdown_tx,
                        &mcp_cancellation,
                        server_task,
                        HTTP_SERVER_STOP_TIMEOUT,
                    )
                );
                server_result?;
                return tunnel_result;
            }
        };

        let restart_reason = match event {
            SuperviseEvent::Died(reason) => reason,
            SuperviseEvent::HealthTick => match tunnel.check_health(&health_client).await {
                TunnelHealth::Healthy => {
                    consecutive_failures = 0;
                    ready.store(true, Ordering::Release);
                    continue;
                }
                TunnelHealth::Unhealthy(detail) | TunnelHealth::Unreachable(detail) => {
                    ready.store(false, Ordering::Release);
                    consecutive_failures += 1;
                    tracing::warn!(
                        "OpenAI tunnel health probe failed ({consecutive_failures}/{TUNNEL_HEALTH_FAIL_THRESHOLD}): {detail}"
                    );
                    if consecutive_failures < TUNNEL_HEALTH_FAIL_THRESHOLD {
                        continue;
                    }
                    format!(
                        "health probe failed {consecutive_failures} consecutive times: {detail}"
                    )
                }
            },
        };

        // Restart, bounded by the circuit breaker.
        consecutive_failures = 0;
        ready.store(false, Ordering::Release);
        let _ = tunnel.shutdown().await;
        tracing::warn!("OpenAI tunnel down; restarting: {restart_reason}");

        loop {
            let attempt = record_restart(&mut restart_window, Instant::now());
            if attempt >= TUNNEL_BREAKER_MAX_RESTARTS {
                stop_http_server(
                    &mut shutdown_tx,
                    &mcp_cancellation,
                    server_task,
                    HTTP_SERVER_STOP_TIMEOUT,
                )
                .await?;
                return Err(anyhow::anyhow!(
                    "OpenAI tunnel restarted {} times within {}s; giving up ({restart_reason})",
                    attempt - 1,
                    TUNNEL_BREAKER_WINDOW.as_secs()
                ));
            }
            let backoff = restart_backoff(attempt);
            println!(
                "OpenAI Secure MCP Tunnel: restart attempt {attempt} in {}s",
                backoff.as_secs()
            );

            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                server_result = &mut server_task => {
                    return flatten_server_result(server_result);
                }
                _ = shutdown_signal() => {
                    stop_http_server(
                        &mut shutdown_tx,
                        &mcp_cancellation,
                        server_task,
                        HTTP_SERVER_STOP_TIMEOUT,
                    ).await?;
                    return Ok(());
                }
            }

            let start_tunnel = crate::openai_tunnel::start(&config);
            tokio::pin!(start_tunnel);
            tokio::select! {
                started = &mut start_tunnel => match started {
                    Ok(new_tunnel) => {
                        tunnel = new_tunnel;
                        health_interval.reset();
                        ready.store(true, Ordering::Release);
                        println!(
                            "OpenAI Secure MCP Tunnel: restarted; readiness {}/readyz",
                            tunnel.health_url()
                        );
                        break;
                    }
                    Err(error) => {
                        tracing::warn!("OpenAI tunnel restart attempt {attempt} failed: {error:#}");
                        // Fall through to retry within the breaker window.
                    }
                },
                server_result = &mut server_task => {
                    return flatten_server_result(server_result);
                }
                _ = shutdown_signal() => {
                    stop_http_server(
                        &mut shutdown_tx,
                        &mcp_cancellation,
                        server_task,
                        HTTP_SERVER_STOP_TIMEOUT,
                    ).await?;
                    return Ok(());
                }
            }
        }
    }
}

async fn stop_http_server(
    sender: &mut Option<tokio::sync::oneshot::Sender<()>>,
    cancellation: &CancellationToken,
    mut server_task: HttpServerTask,
    stop_timeout: Duration,
) -> anyhow::Result<()> {
    cancellation.cancel();
    request_server_shutdown(sender);

    match tokio::time::timeout(stop_timeout, &mut server_task).await {
        Ok(result) => flatten_server_result(result),
        Err(_) => {
            tracing::warn!(
                "HTTP server did not stop within {} seconds; aborting remaining connections",
                stop_timeout.as_secs_f64()
            );
            server_task.abort();
            match server_task.await {
                Err(error) if error.is_cancelled() => Ok(()),
                result => flatten_server_result(result),
            }
        }
    }
}

fn request_server_shutdown(sender: &mut Option<tokio::sync::oneshot::Sender<()>>) {
    if let Some(sender) = sender.take() {
        let _ = sender.send(());
    }
}

fn flatten_server_result(
    result: Result<Result<(), std::io::Error>, tokio::task::JoinError>,
) -> anyhow::Result<()> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.into()),
        Err(error) => Err(anyhow::anyhow!("HTTP server task failed: {error}")),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    {
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = ctrl_c => {}
            _ = terminate => {}
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fmt;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use rmcp::model::{CacheScope, ProtocolVersion, ReadResourceRequestParams};
    use rmcp::{ClientLifecycleMode, ClientServiceExt, ServiceExt};
    use tracing::field::{Field, Visit};
    use tracing::subscriber::Interest;
    use tracing::{Event, Metadata, Subscriber};
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::{Layer, Registry};

    use super::*;

    struct NewlyRegisteredTool {
        name: &'static str,
        answer: &'static str,
        is_error: bool,
    }

    struct SecretInputTool;

    struct InvalidStructuredOutputTool;

    #[async_trait]
    impl Tool for NewlyRegisteredTool {
        fn name(&self) -> &'static str {
            self.name
        }

        fn title(&self) -> String {
            "Test tool".to_string()
        }

        fn behavior(&self) -> crate::tool::ToolBehavior {
            crate::tool::ToolBehavior::new(
                true,
                false,
                true,
                false,
                "Test-only tool without side effects.",
            )
        }

        fn description(&self) -> String {
            "A tool registered only by the dispatch-boundary regression test.".to_string()
        }

        fn input_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" },
                    "private": { "type": "string", "writeOnly": true }
                },
                "required": ["value"],
                "additionalProperties": false
            })
        }

        fn requires_project_root(&self) -> bool {
            false
        }

        async fn call(
            &self,
            _args: Value,
            _config: &AppConfig,
            _session: &SessionState,
        ) -> ToolResult {
            if self.is_error {
                ToolResult::error(self.answer)
            } else {
                ToolResult::text(self.answer)
            }
        }
    }

    #[async_trait]
    impl Tool for SecretInputTool {
        fn name(&self) -> &'static str {
            "secret_input_fixture"
        }

        fn title(&self) -> String {
            "Secret input fixture".to_string()
        }

        fn behavior(&self) -> crate::tool::ToolBehavior {
            crate::tool::ToolBehavior::new(
                true,
                false,
                true,
                false,
                "Test-only tool without side effects.",
            )
        }

        fn description(&self) -> String {
            "A test-only tool for input-schema enforcement.".to_string()
        }

        fn input_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "ref": {
                        "type": "string",
                        "pattern": "^[0-9a-f]{64}$",
                        "writeOnly": true
                    }
                },
                "required": ["ref"],
                "additionalProperties": false
            })
        }

        fn output_schema(&self) -> Option<Value> {
            Some(crate::tool::text_output_schema())
        }

        fn requires_project_root(&self) -> bool {
            false
        }

        async fn call(
            &self,
            _args: Value,
            _config: &AppConfig,
            _session: &SessionState,
        ) -> ToolResult {
            ToolResult::text("accepted")
        }
    }

    #[async_trait]
    impl Tool for InvalidStructuredOutputTool {
        fn name(&self) -> &'static str {
            "invalid_structured_output_fixture"
        }

        fn title(&self) -> String {
            "Invalid structured output fixture".to_string()
        }

        fn behavior(&self) -> crate::tool::ToolBehavior {
            crate::tool::ToolBehavior::new(
                true,
                false,
                true,
                false,
                "Test-only tool without side effects.",
            )
        }

        fn description(&self) -> String {
            "A test-only tool for output-schema enforcement.".to_string()
        }

        fn input_schema(&self) -> Value {
            crate::tool::empty_object_schema()
        }

        fn output_schema(&self) -> Option<Value> {
            Some(crate::tool::text_output_schema())
        }

        fn fills_structured_content(&self) -> bool {
            false
        }

        fn requires_project_root(&self) -> bool {
            false
        }

        async fn call(
            &self,
            _args: Value,
            _config: &AppConfig,
            _session: &SessionState,
        ) -> ToolResult {
            ToolResult::text("invalid").with_structured(json!({ "content": 7 }))
        }
    }

    #[derive(Clone, Default)]
    struct ToolPayloadCapture {
        events: Arc<Mutex<Vec<HashMap<String, String>>>>,
        ordinary_info_completions: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl<S> Layer<S> for ToolPayloadCapture
    where
        S: Subscriber,
    {
        fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
            Interest::sometimes()
        }

        fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
            if event.metadata().target() == "codexify::tool"
                && *event.metadata().level() == tracing::Level::INFO
            {
                self.ordinary_info_completions
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if event.metadata().target() != "codexify::tool_payload" {
                return;
            }
            let mut fields = HashMap::new();
            event.record(&mut ToolPayloadFieldVisitor(&mut fields));
            self.events.lock().unwrap().push(fields);
        }
    }

    struct ToolPayloadFieldVisitor<'a>(&'a mut HashMap<String, String>);

    impl Visit for ToolPayloadFieldVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }

        fn record_bool(&mut self, field: &Field, value: bool) {
            self.0.insert(field.name().to_string(), value.to_string());
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    fn handler_with_tools(
        root: &std::path::Path,
        tools: Vec<Box<dyn Tool>>,
        tool_log_level: crate::types::ToolLogLevel,
    ) -> CodexHandler {
        let mut config = crate::config::default_config(root.to_path_buf());
        config.tool_logging.mode = crate::types::ToolLogMode::All;
        config.tool_logging.level = tool_log_level;
        config.audit.log_file = Some(root.join("audit.jsonl"));
        let tool_logging = ToolCallLogger::new(&config).map(Arc::new);
        let audit = AuditLogger::open(&config).unwrap().map(Arc::new);
        let tools = validate_and_wrap_tools(tools).expect("test tool contracts must be valid");
        CodexHandler {
            config: Arc::new(config),
            tools: Arc::new(tools),
            project_bindings: Arc::new(ProjectBindingStore::new(root.join("bindings"))),
            conversation_authorizations: Arc::new(ConversationAuthorizationStore::new()),
            conversation_exec_sessions: Arc::new(ConversationExecSessionStore::new()),
            diff_checkpoints: Arc::new(DiffCheckpointManager::new()),
            artifact_egress: Arc::new(ArtifactEgressStore::new_at(
                crate::types::ArtifactEgressConfig::default(),
                root.join("artifacts"),
            )),
            bridged_resources: Arc::new(BridgedResourceStore::new(
                crate::types::ArtifactEgressConfig::default(),
            )),
            audit,
            tool_logging,
            next_tool_call_id: Arc::new(AtomicU64::new(1)),
            session: SessionState::new(),
        }
    }

    async fn assert_cacheable_response_hints(
        lifecycle: Option<ClientLifecycleMode>,
        expected_ttl_ms: Option<u64>,
        expected_cache_scope: Option<CacheScope>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let handler = handler_with_tools(
            root.path(),
            crate::registry::load_tools(),
            crate::types::ToolLogLevel::Info,
        );
        let (server_transport, client_transport) = tokio::io::duplex(256 * 1024);
        let server_task = tokio::spawn(async move {
            handler
                .serve(server_transport)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let client = match lifecycle {
            Some(lifecycle) => ().serve_with_lifecycle(client_transport, lifecycle).await.unwrap(),
            None => ().serve(client_transport).await.unwrap(),
        };

        macro_rules! assert_hints {
            ($result:expr) => {{
                let result = $result;
                assert_eq!(result.ttl_ms, expected_ttl_ms);
                assert_eq!(result.cache_scope, expected_cache_scope.clone());
                result
            }};
        }

        assert_hints!(client.list_tools(None).await.unwrap());
        assert_hints!(client.list_prompts(None).await.unwrap());
        assert_hints!(client.list_resources(None).await.unwrap());
        assert_hints!(client.list_resource_templates(None).await.unwrap());
        let resource = assert_hints!(
            client
                .read_resource(ReadResourceRequestParams::new(diff_ui::DIFF_UI_URI))
                .await
                .unwrap()
        );
        let contents = serde_json::to_value(resource.contents.first().unwrap()).unwrap();
        assert_eq!(contents["uri"], diff_ui::DIFF_UI_URI);
        assert_eq!(contents["mimeType"], diff_ui::DIFF_UI_MIME_TYPE);

        client.cancel().await.unwrap();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn modern_cacheable_responses_include_required_hints() {
        assert_cacheable_response_hints(
            Some(ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            }),
            Some(0),
            Some(CacheScope::Private),
        )
        .await;
    }

    #[test]
    fn modern_cache_hint_defaults_preserve_an_existing_private_ttl() {
        let mut ttl_ms = Some(12_345);
        let mut cache_scope = Some(CacheScope::Private);

        ensure_modern_cache_hints(
            Some(&ProtocolVersion::V_2026_07_28),
            &mut ttl_ms,
            &mut cache_scope,
        );

        assert_eq!(ttl_ms, Some(12_345));
        assert_eq!(cache_scope, Some(CacheScope::Private));
    }

    #[tokio::test]
    async fn legacy_cacheable_responses_preserve_the_old_wire_shape() {
        assert_cacheable_response_hints(None, None, None).await;
    }

    #[test]
    fn common_dispatch_boundary_observes_new_tools_and_preserves_results() {
        let _tracing_guard = crate::tool_logging::TEST_TRACING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = tempfile::tempdir().unwrap();
        let handler = handler_with_tools(
            root.path(),
            vec![
                Box::new(NewlyRegisteredTool {
                    name: "new_success_fixture",
                    answer: "unchanged success answer",
                    is_error: false,
                }),
                Box::new(NewlyRegisteredTool {
                    name: "new_failure_fixture",
                    answer: "unchanged failure answer",
                    is_error: true,
                }),
            ],
            crate::types::ToolLogLevel::Info,
        );
        let capture = ToolPayloadCapture::default();
        let events = capture.events.clone();
        let ordinary_info_completions = capture.ordinary_info_completions.clone();
        let subscriber = Registry::default().with(capture);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (success, failure) = tracing::subscriber::with_default(subscriber, || {
            crate::tool_logging::rebuild_test_interest_cache();
            runtime.block_on(async move {
                let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
                let server_task = tokio::spawn(async move {
                    handler
                        .serve(server_transport)
                        .await
                        .unwrap()
                        .waiting()
                        .await
                        .unwrap();
                });
                let client = ().serve(client_transport).await.unwrap();
                let arguments = json!({
                    "value": "visible request",
                    "private": "schema-protected request"
                })
                .as_object()
                .unwrap()
                .clone();
                let success = client
                    .call_tool(
                        CallToolRequestParams::new("new_success_fixture")
                            .with_arguments(arguments.clone()),
                    )
                    .await
                    .unwrap();
                let failure = client
                    .call_tool(
                        CallToolRequestParams::new("new_failure_fixture").with_arguments(arguments),
                    )
                    .await
                    .unwrap();
                client.cancel().await.unwrap();
                server_task.await.unwrap();
                (success, failure)
            })
        });

        assert_eq!(success.is_error, Some(false));
        assert_eq!(
            success.content[0].as_text().unwrap().text,
            "unchanged success answer"
        );
        assert_eq!(failure.is_error, Some(true));
        assert_eq!(
            failure.content[0].as_text().unwrap().text,
            "unchanged failure answer"
        );

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(
            ordinary_info_completions.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "payload observation must replace, not duplicate, the ordinary info completion"
        );
        for pair in events.as_chunks::<2>().0 {
            assert_eq!(pair[0]["phase"], "start");
            assert_eq!(pair[1]["phase"], "finish");
            assert_eq!(pair[0]["call_id"], pair[1]["call_id"]);
            assert_eq!(pair[0]["resolved_tool"], pair[0]["tool"]);
            assert!(pair[0]["request"].contains("visible request"));
            assert!(!pair[0]["request"].contains("schema-protected request"));
        }
        assert_eq!(events[1]["status"], "ok");
        assert!(events[1]["response"].contains("unchanged success answer"));
        assert_eq!(events[3]["status"], "error");
        assert!(events[3]["response"].contains("unchanged failure answer"));
        let payload_call_ids = events
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| pair[0]["call_id"].parse::<u64>().unwrap())
            .collect::<Vec<_>>();
        drop(events);

        let audit_events = std::fs::read_to_string(root.path().join("audit.jsonl")).unwrap();
        let audit_events = audit_events
            .lines()
            .skip(1)
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let audit_call_ids = audit_events
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| pair[0]["call_id"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(payload_call_ids, audit_call_ids);
    }

    #[tokio::test]
    async fn dispatch_enforces_input_and_output_schemas_without_echoing_write_only_values() {
        let root = tempfile::tempdir().unwrap();
        let handler = handler_with_tools(
            root.path(),
            vec![
                Box::new(SecretInputTool),
                Box::new(InvalidStructuredOutputTool),
            ],
            crate::types::ToolLogLevel::Info,
        );
        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let server_task = tokio::spawn(async move {
            handler
                .serve(server_transport)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let client = ().serve(client_transport).await.unwrap();

        let protected_value = "not-a-valid-reference-and-must-never-be-returned";
        let invalid_input = client
            .call_tool(
                CallToolRequestParams::new("secret_input_fixture").with_arguments(
                    json!({ "ref": protected_value, "unexpected": true })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert_eq!(invalid_input.is_error, Some(true));
        let invalid_input_text = invalid_input.content[0].as_text().unwrap().text.as_str();
        assert!(invalid_input_text.contains("Invalid arguments"));
        assert!(!invalid_input_text.contains(protected_value));

        let invalid_output = client
            .call_tool(CallToolRequestParams::new(
                "invalid_structured_output_fixture",
            ))
            .await
            .unwrap();
        assert_eq!(invalid_output.is_error, Some(true));
        assert!(
            invalid_output.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("violates its declared output schema")
        );

        client.cancel().await.unwrap();
        server_task.await.unwrap();
    }

    #[test]
    fn filtered_payload_level_falls_back_to_the_ordinary_completion_event() {
        let _tracing_guard = crate::tool_logging::TEST_TRACING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().to_path_buf());
        config.tool_logging.mode = crate::types::ToolLogMode::All;
        config.tool_logging.level = crate::types::ToolLogLevel::Debug;
        let logger = ToolCallLogger::new(&config).unwrap();
        let capture = ToolPayloadCapture::default();
        let events = capture.events.clone();
        let subscriber = Registry::default().with(capture).with(LevelFilter::INFO);
        let call = tracing::subscriber::with_default(subscriber, || {
            crate::tool_logging::rebuild_test_interest_cache();
            logger.begin(
                1,
                &ToolCallIdentity::native("filtered_payload_fixture"),
                &json!({ "value": "visible request" }),
                None,
            )
        });

        assert!(events.lock().unwrap().is_empty());
        assert!(should_emit_ordinary_tool_completion(Some(&call)));
    }

    #[test]
    fn advertises_all_mcp_apps_resources_and_extension() {
        let root = tempfile::tempdir().unwrap();
        let handler = CodexHandler {
            config: Arc::new(crate::config::default_config(root.path().to_path_buf())),
            tools: Arc::new(crate::registry::load_tools()),
            project_bindings: Arc::new(ProjectBindingStore::new(root.path().join("bindings"))),
            conversation_authorizations: Arc::new(ConversationAuthorizationStore::new()),
            conversation_exec_sessions: Arc::new(ConversationExecSessionStore::new()),
            diff_checkpoints: Arc::new(DiffCheckpointManager::new()),
            artifact_egress: Arc::new(ArtifactEgressStore::new_at(
                crate::types::ArtifactEgressConfig::default(),
                root.path().join("artifacts"),
            )),
            bridged_resources: Arc::new(BridgedResourceStore::new(
                crate::types::ArtifactEgressConfig::default(),
            )),
            audit: None,
            tool_logging: None,
            next_tool_call_id: Arc::new(AtomicU64::new(1)),
            session: SessionState::new(),
        };

        let info = handler.get_info();
        assert!(info.capabilities.resources.is_some());
        assert!(
            info.capabilities
                .extensions
                .as_ref()
                .is_some_and(|extensions| {
                    extensions.contains_key(diff_ui::MCP_APPS_EXTENSION_ID)
                })
        );

        let resources = builtin_ui_resources();
        assert_eq!(resources.len(), 3);
        assert_eq!(resources[0].uri, diff_ui::DIFF_UI_URI);
        assert_eq!(resources[1].uri, crate::self_update_ui::SELF_UPDATE_UI_URI);
        assert_eq!(resources[2].uri, crate::setup_ui::SETUP_UI_URI);

        let contents = builtin_ui_contents(crate::self_update_ui::SELF_UPDATE_UI_URI)
            .expect("self-update MCP App resource must be readable");
        let contents = serde_json::to_value(contents).unwrap();
        assert_eq!(contents["uri"], crate::self_update_ui::SELF_UPDATE_UI_URI);
        assert_eq!(
            contents["mimeType"],
            crate::self_update_ui::SELF_UPDATE_UI_MIME_TYPE
        );
        assert!(
            contents["text"]
                .as_str()
                .is_some_and(|text| text.contains("self_update_status"))
        );

        let setup_contents = builtin_ui_contents(crate::setup_ui::SETUP_UI_URI)
            .expect("setup MCP App resource must be readable");
        let setup_contents = serde_json::to_value(setup_contents).unwrap();
        assert!(setup_contents["text"].as_str().is_some_and(|text| {
            text.contains("Check for updates") && text.contains("\"Doctor\"")
        }));
    }

    #[test]
    fn conversation_auth_gate_is_scoped_to_the_stable_chat_identity() {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().to_path_buf());
        config.conversation_auth_token =
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into());
        let tools = crate::registry::load_tools_for_config(&config);
        let authorizations = Arc::new(ConversationAuthorizationStore::new());
        let handler = CodexHandler {
            config: Arc::new(config),
            tools: Arc::new(tools),
            project_bindings: Arc::new(ProjectBindingStore::new(root.path().join("bindings"))),
            conversation_authorizations: authorizations.clone(),
            conversation_exec_sessions: Arc::new(ConversationExecSessionStore::new()),
            diff_checkpoints: Arc::new(DiffCheckpointManager::new()),
            artifact_egress: Arc::new(ArtifactEgressStore::new_at(
                crate::types::ArtifactEgressConfig::default(),
                root.path().join("artifacts"),
            )),
            bridged_resources: Arc::new(BridgedResourceStore::new(
                crate::types::ArtifactEgressConfig::default(),
            )),
            audit: None,
            tool_logging: None,
            next_tool_call_id: Arc::new(AtomicU64::new(1)),
            session: SessionState::new(),
        };
        let first = ConversationIdentity::from_openai_session("first-chat").unwrap();
        let second = ConversationIdentity::from_openai_session("second-chat").unwrap();

        let blocked = handler
            .conversation_auth_error("read_file", Some(&first))
            .unwrap();
        assert!(blocked.is_error);
        assert!(
            blocked
                .joined_text()
                .contains("has not completed connector setup")
        );
        assert!(!blocked.joined_text().to_ascii_lowercase().contains("auth"));
        assert!(
            !blocked
                .joined_text()
                .to_ascii_lowercase()
                .contains("checksum")
        );
        assert!(
            handler
                .conversation_auth_error(AUTHORIZATION_TOOL_WIRE_NAME, Some(&first))
                .is_none()
        );

        authorizations
            .authorize(Some(&first), &handler.session)
            .unwrap();
        assert!(
            handler
                .conversation_auth_error("read_file", Some(&first))
                .is_none()
        );
        assert!(
            handler
                .conversation_auth_error("read_file", Some(&second))
                .is_some()
        );
    }

    #[test]
    fn tool_descriptor_preserves_native_file_metadata_and_annotations() {
        let root = tempfile::tempdir().unwrap();
        let config = crate::config::default_config(root.path().to_path_buf());
        let tool = crate::tools::import_host_file::ImportHostFile::default();

        let advertised = advertised_tool(&tool, &config);
        assert_eq!(advertised.title.as_deref(), Some("Import attached file"));
        assert_eq!(
            advertised
                .meta
                .as_ref()
                .and_then(|meta| meta.get("openai/fileParams")),
            Some(&json!(["file"]))
        );
        let annotations = advertised.annotations.unwrap();
        assert_eq!(annotations.read_only_hint, Some(false));
        assert_eq!(annotations.destructive_hint, Some(false));
        assert_eq!(annotations.idempotent_hint, Some(true));
        assert_eq!(annotations.open_world_hint, Some(true));
    }

    #[test]
    fn call_result_preserves_upstream_result_metadata() {
        let mut meta = rmcp::model::MetaObject::new();
        meta.0.insert("trace".to_string(), json!("upstream"));
        let converted = to_call_tool_result(ToolResult {
            content: vec![ToolContent::Text("ok".to_string())],
            is_error: false,
            structured_content: Some(json!({ "value": 1 })),
            meta: Some(meta),
            audit: Default::default(),
        });

        assert_eq!(converted.structured_content, Some(json!({ "value": 1 })));
        assert_eq!(
            converted
                .meta
                .as_ref()
                .and_then(|metadata| metadata.0.get("trace")),
            Some(&json!("upstream"))
        );
    }

    #[test]
    fn finalizer_bounds_text_and_generated_structured_mirror() {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().to_path_buf());
        config.output.max_tool_output_tokens = Some(20);
        let tool = crate::tools::grep::Grep;
        let mut result = ToolResult::text("\"\\\n".repeat(1_000));

        finalize_model_visible_result(Some(&tool), &mut result, &config);

        assert!(!result.is_error);
        assert_eq!(result.audit.truncated, Some(true));
        let structured = result.structured_content.as_ref().unwrap();
        let text = result.joined_text();
        assert_eq!(
            structured.get("content").and_then(Value::as_str),
            Some(text.as_str())
        );
        let serialized = serde_json::to_string(structured).unwrap();
        assert!(crate::output_budget::approx_token_count(&serialized) <= 20);
    }

    #[test]
    fn finalizer_rejects_oversized_explicit_structured_content() {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().to_path_buf());
        config.output.max_tool_output_tokens = Some(20);
        let tool = crate::tools::grep::Grep;
        let mut result = ToolResult::text("fallback").with_structured(json!({
            "payload": "x".repeat(10_000)
        }));

        finalize_model_visible_result(Some(&tool), &mut result, &config);

        assert!(result.is_error);
        assert!(result.structured_content.is_none());
        assert!(
            result
                .joined_text()
                .contains("Retry with narrower arguments")
        );
        assert!(crate::output_budget::approx_token_count(&result.joined_text()) <= 20);
    }

    #[test]
    fn call_result_serializes_exported_files_as_resource_links() {
        let resource = rmcp::model::Resource::new(
            "codexify://artifact/abcdefghijklmnopqrstuvwxyz0123456789_-ABCDE",
            "report.bin",
        )
        .with_mime_type("application/octet-stream")
        .with_size(42);
        let converted = to_call_tool_result(ToolResult {
            content: vec![ToolContent::ResourceLink(resource)],
            is_error: false,
            structured_content: None,
            meta: None,
            audit: Default::default(),
        });
        let value = serde_json::to_value(converted).unwrap();

        assert_eq!(value["content"][0]["type"], "resource_link");
        assert_eq!(value["content"][0]["name"], "report.bin");
        assert_eq!(value["content"][0]["mimeType"], "application/octet-stream");
        assert_eq!(value["content"][0]["size"], 42);
    }

    #[tokio::test]
    async fn http_shutdown_aborts_after_the_grace_period() {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let mut shutdown_tx = Some(shutdown_tx);
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(async move {
            let _ = shutdown_rx.await;
            std::future::pending::<Result<(), std::io::Error>>().await
        });

        stop_http_server(
            &mut shutdown_tx,
            &cancellation,
            task,
            Duration::from_millis(10),
        )
        .await
        .unwrap();

        assert!(shutdown_tx.is_none());
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn restart_window_evicts_entries_older_than_the_breaker_window() {
        let base = Instant::now();
        let mut window = Vec::new();

        // Restarts spaced 40s apart: by t=80s the first (t=0) has aged out of the
        // 60s window, so the count stays at 2 rather than growing to 3.
        assert_eq!(record_restart(&mut window, base), 1);
        assert_eq!(
            record_restart(&mut window, base + Duration::from_secs(40)),
            2
        );
        assert_eq!(
            record_restart(&mut window, base + Duration::from_secs(80)),
            2
        );
    }

    #[test]
    fn restart_window_trips_the_breaker_after_five_rapid_restarts() {
        let base = Instant::now();
        let mut window = Vec::new();
        let mut last = 0;
        for i in 0..TUNNEL_BREAKER_MAX_RESTARTS {
            last = record_restart(&mut window, base + Duration::from_secs(i as u64));
        }
        // Five restarts inside the 60s window: the attempt count reaches the cap.
        assert_eq!(last, TUNNEL_BREAKER_MAX_RESTARTS);
        assert!(last >= TUNNEL_BREAKER_MAX_RESTARTS);
    }

    #[test]
    fn restart_backoff_is_linear_then_capped() {
        assert_eq!(restart_backoff(1), Duration::from_secs(1));
        assert_eq!(restart_backoff(4), Duration::from_secs(4));
        assert_eq!(restart_backoff(5), TUNNEL_RESTART_BACKOFF_MAX);
        assert_eq!(restart_backoff(100), TUNNEL_RESTART_BACKOFF_MAX);
    }

    #[tokio::test]
    async fn health_reports_tunnel_readiness() {
        let ready = Arc::new(AtomicBool::new(false));
        let state = HealthState {
            tool_count: 38,
            ready: ready.clone(),
        };

        let (status, Json(payload)) = health(State(state.clone())).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(payload, json!({ "status": "starting", "tools": 38 }));

        ready.store(true, Ordering::Release);
        let (status, Json(payload)) = health(State(state)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload, json!({ "status": "ok", "tools": 38 }));
    }
}
