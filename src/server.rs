//! MCP server: the [`CodexHandler`] that implements rmcp's `ServerHandler`, plus
//! the axum wiring (`/mcp` Streamable HTTP, `/health`, CORS, bearer auth).
//!
//! Ports `src/server.ts`. rmcp's `StreamableHttpService` owns the transport and
//! MCP session lifecycle: the service factory runs once per MCP session, so a
//! fresh [`SessionState`] lives inside each handler and is dropped — killing any
//! resident exec processes — when the session ends.

use std::sync::Arc;

use axum::{Router, extract::State, response::Json, routing::get};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo,
    },
    service::{RequestContext, RoleServer},
    transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::{Value, json};
use tower_http::cors::{Any, CorsLayer};

use crate::auth::require_auth;
use crate::exec_sessions::SessionState;
use crate::instructions::build_instructions;
use crate::registry::load_tools;
use crate::tool::Tool;
use crate::types::{AppConfig, ToolContent, ToolResult};

/// One MCP session: shared config and tool registry, plus this session's own
/// mutable state.
pub struct CodexHandler {
    config: Arc<AppConfig>,
    tools: Arc<Vec<Box<dyn Tool>>>,
    session: SessionState,
}

/// Convert this repo's [`ToolResult`] into rmcp's `CallToolResult`.
fn to_call_tool_result(result: ToolResult) -> CallToolResult {
    let blocks: Vec<ContentBlock> = result
        .content
        .into_iter()
        .map(|c| match c {
            ToolContent::Text(t) => ContentBlock::text(t),
            ToolContent::Image { data, mime_type } => ContentBlock::image(data, mime_type),
        })
        .collect();

    let mut ctr = if result.is_error {
        CallToolResult::error(blocks)
    } else {
        CallToolResult::success(blocks)
    };
    ctr.structured_content = result.structured_content;
    ctr
}

impl ServerHandler for CodexHandler {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("codexify", "1.0.1"))
            .with_instructions(build_instructions(&self.config))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = self
            .tools
            .iter()
            .map(|tool| {
                let schema = tool.input_schema().as_object().cloned().unwrap_or_default();
                let mut mcp_tool =
                    rmcp::model::Tool::new(tool.name(), tool.describe(&self.config), schema);
                if let Some(out) = tool.output_schema()
                    && let Some(obj) = out.as_object()
                {
                    mcp_tool = mcp_tool.with_raw_output_schema(Arc::new(obj.clone()));
                }
                mcp_tool
            })
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.as_ref();
        let args = request.arguments.map(Value::Object).unwrap_or(Value::Null);

        let Some(tool) = self.tools.iter().find(|t| t.name() == name) else {
            let result =
                CallToolResult::error(vec![ContentBlock::text(format!("Unknown tool: {name}"))]);
            return Ok(result.into());
        };

        let mut result = tool.call(args, &self.config, &self.session).await;

        // Fill in the `structuredContent` the MCP spec expects from any tool that
        // advertises an `outputSchema`. Most tools use the `{ content: string }`
        // schema, for which the text they return *is* the structured form; tools
        // with a different schema build their own and pass through. Errors are
        // left alone — the spec exempts `isError` results.
        if tool.output_schema().is_some()
            && tool.fills_structured_content()
            && !result.is_error
            && result.structured_content.is_none()
        {
            result.structured_content = Some(json!({ "content": result.joined_text() }));
        }

        tracing::info!(
            "  tool: {name} -> {}",
            if result.is_error { "error" } else { "ok" }
        );
        Ok(to_call_tool_result(result).into())
    }
}

async fn health(State(tool_count): State<usize>) -> Json<Value> {
    Json(json!({ "status": "ok", "tools": tool_count }))
}

/// Build the axum app and serve it. Ports `startHttpServer`.
pub async fn start_http_server(mut config: AppConfig) -> anyhow::Result<()> {
    // Gateway-mode upstreams write their generated skills here; keyed by port so
    // concurrent instances don't clobber each other, and rebuilt fresh per start.
    let gen_dir = std::env::temp_dir()
        .join("codexify-gateway-skills")
        .join(config.port.to_string());
    let _ = std::fs::remove_dir_all(&gen_dir);
    config.generated_skills_dir = Some(gen_dir);
    let config = Arc::new(config);

    // Connect to any configured upstream MCP servers and merge their tools in.
    // The returned services must stay alive for the whole server lifetime, so
    // they are held in `_bridge_services` until `axum::serve` returns.
    let bridge = crate::bridge::connect_upstreams(&config).await;
    let bridge_report = bridge.report;
    let _bridge_services = bridge.services;

    let mut all_tools = load_tools();
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
    // The original bridge accepted any Host so it works behind a tunnel that
    // presents an arbitrary hostname. Preserve that unless the operator lists
    // explicit hosts in the config.
    if config.allowed_hosts.is_empty() {
        http_config.allowed_hosts.clear();
    } else {
        http_config.allowed_hosts = config.allowed_hosts.clone();
    }

    let factory_config = config.clone();
    let factory_tools = tools.clone();
    let service = StreamableHttpService::new(
        move || {
            Ok(CodexHandler {
                config: factory_config.clone(),
                tools: factory_tools.clone(),
                session: SessionState::new(),
            })
        },
        Arc::new(LocalSessionManager::default()),
        http_config,
    );

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers([axum::http::HeaderName::from_static("mcp-session-id")]);

    let app = Router::new()
        .route("/health", get(health))
        .with_state(tool_count)
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(
            config.clone(),
            require_auth,
        ))
        .layer(cors);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.port)).await?;

    println!(
        "\nCodexify MCP Bridge (Rust) running on http://localhost:{}",
        config.port
    );
    println!("Work directory: {}", config.work_dir.display());
    println!(
        "Tools loaded ({tool_count}): {} native + {bridged_count} bridged from upstream MCP servers",
        tool_count - bridged_count
    );
    if !bridge_report.is_empty() {
        println!("Upstream MCP servers:");
        for line in &bridge_report {
            println!("  {line}");
        }
    }
    if config.api_key.is_some() {
        println!("Auth: enabled (bearer token)");
    } else {
        println!("Auth: disabled (no --api-key)");
    }
    println!("\nAdd to ChatGPT > Plugins > New Plugin:");
    println!("  Server URL: https://<your-tunnel>/mcp\n");

    axum::serve(listener, app).await?;
    Ok(())
}
