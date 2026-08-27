//! MCP server: the [`CodexHandler`] that implements rmcp's `ServerHandler`, plus
//! the axum wiring (`/mcp` Streamable HTTP, `/health`, CORS, bearer auth).
//!
//! Ports `src/server.ts`. rmcp's `StreamableHttpService` owns the transport and
//! MCP session lifecycle: the service factory runs once per transport session.
//! Generic clients keep resident commands in their transport [`SessionState`],
//! while ChatGPT's stable conversation metadata selects server-owned command
//! state and review checkpoints that survive transport replacement. The embedded
//! review resource remains presentation-only.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{Router, extract::State, response::Json, routing::get};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        ExtensionCapabilities, Implementation, InitializeResult, ListResourcesResult,
        ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
        ReadResourceResult, ServerCapabilities, ServerInfo,
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
use crate::conversation_auth::{AUTHORIZATION_TOOL_WIRE_NAME, ConversationAuthorizationStore};
use crate::exec_sessions::{ConversationExecSessionStore, SessionState};
use crate::instructions::build_initial_instructions;
use crate::openai_tunnel::TunnelHealth;
use crate::project_bindings::{ConversationIdentity, ProjectBindingStore};
use crate::registry::load_tools_for_config;
use crate::review::{ReviewAvailability, ReviewCheckpointManager, ReviewOwner};
use crate::review_ui;
use crate::tool::{Tool, ToolRequestContext};
use crate::tools::set_project_root::{SetProjectRoot, select_and_render};
use crate::types::{AppConfig, ToolContent, ToolResult};

const HTTP_SERVER_STOP_TIMEOUT: Duration = Duration::from_secs(10);

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
    review_checkpoints: Arc<ReviewCheckpointManager>,
    artifact_egress: Arc<ArtifactEgressStore>,
    audit: Option<Arc<AuditLogger>>,
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

fn advertised_tool(tool: &dyn Tool, config: &AppConfig) -> rmcp::model::Tool {
    let schema = tool.input_schema().as_object().cloned().unwrap_or_default();
    let mut advertised = rmcp::model::Tool::new(tool.name(), tool.describe(config), schema);
    if let Some(title) = tool.title() {
        advertised = advertised.with_title(title);
    }
    if let Some(annotations) = tool.annotations() {
        advertised = advertised.with_annotations(annotations);
    }
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

impl ServerHandler for CodexHandler {
    fn get_info(&self) -> ServerInfo {
        let mut capabilities = ServerCapabilities::builder()
            .enable_resources()
            .enable_tools()
            .build();
        let mut extensions = ExtensionCapabilities::new();
        extensions.insert(
            review_ui::MCP_APPS_EXTENSION_ID.to_string(),
            json!({ "mimeTypes": [review_ui::REVIEW_UI_MIME_TYPE] })
                .as_object()
                .cloned()
                .expect("static MCP Apps capability must be an object"),
        );
        capabilities.extensions = Some(extensions);
        InitializeResult::new(capabilities)
            .with_server_info(Implementation::new("codexify", "1.8.0"))
            .with_instructions(build_initial_instructions(&self.config))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = self
            .tools
            .iter()
            .map(|tool| advertised_tool(tool.as_ref(), &self.config))
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(vec![
            review_ui::resource(),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        match self
            .artifact_egress
            .read_resource(&request.uri, &context.ct)
            .await
        {
            Ok(Some(contents)) => {
                return Ok(ReadResourceResult::new(vec![contents]).into());
            }
            Ok(None) => {}
            Err(error) => {
                return Err(McpError::internal_error(error.to_string(), None));
            }
        }
        if request.uri.starts_with(ARTIFACT_RESOURCE_URI_PREFIX) {
            return Err(McpError::resource_not_found(
                "Unknown or expired exported-file resource".to_string(),
                None,
            ));
        }
        let Some(contents) = review_ui::contents_for_uri(&request.uri) else {
            return Err(McpError::resource_not_found(
                format!("Unknown resource: {}", request.uri),
                None,
            ));
        };
        Ok(ReadResourceResult::new(vec![contents]).into())
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
        let args = request.arguments.map(Value::Object).unwrap_or(Value::Null);
        let tool_context = ToolRequestContext {
            conversation: conversation.clone(),
            conversation_authorizations: self.conversation_authorizations.clone(),
            review_checkpoints: self.review_checkpoints.clone(),
            artifact_egress: self.artifact_egress.clone(),
            cancellation: context.ct.clone(),
        };

        // Keep `tool` as an Option so that even an unknown-tool call flows through
        // the audit begin/finish pairing below rather than short-circuiting.
        let tool = self.tools.iter().find(|t| t.name() == name);

        // Verbose-diagnostics / audit preamble (#15). `needs_scope` gates the cost
        // of building an audit scope and argument summaries to the cases that will
        // actually consume them.
        let needs_scope = self.audit.is_some()
            || tracing::enabled!(target: "codexify::tool", tracing::Level::DEBUG);
        let input_schema = needs_scope
            .then(|| tool.map(|tool| tool.input_schema()))
            .flatten();
        let start_scope = needs_scope.then(|| self.audit_scope(conversation.as_ref()));
        let audit_call = self.audit.as_ref().and_then(|audit| {
            start_scope
                .as_ref()
                .map(|scope| audit.begin_tool(&name, &args, input_schema.as_ref(), scope))
        });
        if let Some(scope) = start_scope.as_ref() {
            tracing::debug!(
                target: "codexify::tool",
                tool = %name,
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
                tool = %name,
                argument_summary = %summarize_arguments(&args, input_schema.as_ref()),
                "tool arguments summarized"
            );
        }

        let started = Instant::now();
        let mut result = if let Some(error) =
            self.conversation_auth_error(&name, conversation.as_ref())
        {
            error
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
                        select_and_render(&args, |path| async move {
                            self.project_bindings
                                .select_project_root(&self.config, identity, &path)
                                .await
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
                                Some(identity) => ReviewOwner::conversation(identity),
                                None => ReviewOwner::transport(self.session.review_state()),
                            };
                            if tool.may_modify_project() {
                                // Fail closed: refuse a mutating tool when the review
                                // checkpoint cannot be captured. The refusal is returned
                                // as a value (not an early return) so the audit
                                // finish_tool below still pairs with begin_tool.
                                match self
                                    .review_checkpoints
                                    .begin_mutation(&effective_config, owner)
                                    .await
                                {
                                    Ok((availability, guard)) => {
                                        if let ReviewAvailability::Unavailable(reason) =
                                            &availability
                                        {
                                            tracing::debug!(
                                                "review checkpoints unavailable for {name}: {reason}"
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
                                        "Refusing to run mutating tool `{name}` because the project review checkpoint could not be captured: {error}"
                                    )),
                                }
                            } else {
                                match self
                                    .review_checkpoints
                                    .ensure_initialized(&effective_config, owner)
                                    .await
                                {
                                    Ok(ReviewAvailability::Ready) => {}
                                    Ok(ReviewAvailability::Unavailable(reason)) => {
                                        tracing::debug!(
                                            "review checkpoints unavailable for {name}: {reason}"
                                        );
                                    }
                                    Err(error) => {
                                        tracing::debug!(
                                            "review checkpoint initialization skipped for {name}: {error}"
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

        // Fill in the `structuredContent` the MCP spec expects from any tool that
        // advertises an `outputSchema`. Most tools use the `{ content: string }`
        // schema, for which the text they return *is* the structured form; tools
        // with a different schema build their own and pass through. Errors are
        // left alone — the spec exempts `isError` results.
        if let Some(tool) = tool
            && tool.output_schema().is_some()
            && tool.fills_structured_content()
            && !result.is_error
            && result.structured_content.is_none()
        {
            result.structured_content = Some(json!({ "content": result.joined_text() }));
        }

        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        tracing::info!(
            target: "codexify::tool",
            tool = %name,
            status = if result.is_error { "error" } else { "ok" },
            duration_ms,
            "tool completed"
        );
        if tracing::enabled!(target: "codexify::tool", tracing::Level::DEBUG) {
            tracing::debug!(
                target: "codexify::tool",
                tool = %name,
                output_summary = %summarize_output(&result),
                "tool output summarized"
            );
        }
        if let (Some(audit), Some(call), Some(start_scope)) =
            (&self.audit, audit_call.as_ref(), start_scope.as_ref())
        {
            if name == SetProjectRoot::NAME {
                let finish_scope = self.audit_scope(conversation.as_ref());
                audit.finish_tool(call, &name, &result, duration_ms, &finish_scope);
            } else {
                audit.finish_tool(call, &name, &result, duration_ms, start_scope);
            }
        }
        Ok(to_call_tool_result(result).into())
    }
}

async fn health(State(tool_count): State<usize>) -> Json<Value> {
    Json(json!({ "status": "ok", "tools": tool_count }))
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
    let review_checkpoints = Arc::new(ReviewCheckpointManager::new());
    let artifact_egress = Arc::new(ArtifactEgressStore::new(config.artifact_egress.clone()));
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
    let _bridge_services = bridge.services;

    let mut all_tools = load_tools_for_config(&config);
    let native: std::collections::HashSet<&'static str> =
        all_tools.iter().map(|t| t.name()).collect();
    let mut seen = native.clone();
    for tool in bridge.tools {
        let name = tool.name();
        if seen.contains(name) {
            tracing::warn!("bridged tool '{name}' collides with an existing tool; skipping");
            continue;
        }
        seen.insert(name);
        all_tools.push(tool);
    }
    let bridged_count = all_tools.len() - native.len();

    let tools = Arc::new(all_tools);
    let tool_count = tools.len();

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
    let factory_review_checkpoints = review_checkpoints.clone();
    let factory_artifact_egress = artifact_egress.clone();
    let factory_audit = audit.clone();
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
                review_checkpoints: factory_review_checkpoints.clone(),
                artifact_egress: factory_artifact_egress.clone(),
                audit: factory_audit.clone(),
                session,
            })
        },
        Arc::new(LocalSessionManager::default()),
        http_config,
    );

    let app = Router::new()
        .route("/health", get(health))
        .with_state(tool_count)
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
    run_with_openai_tunnel(listener, app, config, mcp_cancellation).await
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
                    continue;
                }
                TunnelHealth::Unhealthy(detail) | TunnelHealth::Unreachable(detail) => {
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
    use super::*;

    #[test]
    fn advertises_review_resources_and_mcp_apps_extension() {
        let root = tempfile::tempdir().unwrap();
        let handler = CodexHandler {
            config: Arc::new(crate::config::default_config(root.path().to_path_buf())),
            tools: Arc::new(crate::registry::load_tools()),
            project_bindings: Arc::new(ProjectBindingStore::new(root.path().join("bindings"))),
            conversation_authorizations: Arc::new(ConversationAuthorizationStore::new()),
            conversation_exec_sessions: Arc::new(ConversationExecSessionStore::new()),
            review_checkpoints: Arc::new(ReviewCheckpointManager::new()),
            artifact_egress: Arc::new(ArtifactEgressStore::new(
                crate::types::ArtifactEgressConfig::default(),
            )),
            audit: None,
            session: SessionState::new(),
        };

        let info = handler.get_info();
        assert!(info.capabilities.resources.is_some());
        assert!(
            info.capabilities
                .extensions
                .as_ref()
                .is_some_and(|extensions| {
                    extensions.contains_key(review_ui::MCP_APPS_EXTENSION_ID)
                })
        );
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
            review_checkpoints: Arc::new(ReviewCheckpointManager::new()),
            artifact_egress: Arc::new(ArtifactEgressStore::new(
                crate::types::ArtifactEgressConfig::default(),
            )),
            audit: None,
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
        assert_eq!(annotations.idempotent_hint, Some(false));
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
}
