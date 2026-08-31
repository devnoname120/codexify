//! Bridge to upstream MCP servers.
//!
//! codexify can act as an MCP *client* to other MCP servers and discover their
//! complete tool catalogs at startup. Explicit compatibility modes can re-expose
//! tools directly or through a gateway; catalog mode keeps transitive definitions
//! private and exposes a fixed ranked discovery/schema/call surface instead.
//!
//! Local servers use stdio; remote servers use MCP Streamable HTTP.

use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use http::{HeaderName, HeaderValue, header::AUTHORIZATION};
use rmcp::{
    RoleClient, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotification,
        CancelledNotificationParam, ClientRequest, ContentBlock, Icon, MetaObject, ServerResult,
        ToolAnnotations,
    },
    service::{Peer, PeerRequestOptions, RunningService, ServiceError},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::bridged_resources::BridgedResourceStore;
use crate::exec_sessions::SessionState;
use crate::mcp_catalog::{CatalogSourceInput, build_catalog_tools};
use crate::process_env::scrub_untrusted_child_env;
use crate::tool::{Tool, ToolBehavior, ToolCallIdentity, ToolRequestContext};
use crate::types::{AppConfig, McpToolExposure, ToolContent, ToolResult};

/// How long to wait for an upstream to start up and answer `tools/list` before
/// giving up on it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// The result of connecting to every configured upstream: the tools to merge
/// into the registry, plus the running services kept alive for the server's
/// lifetime (dropping one closes its transport and any child process).
pub struct Bridge {
    pub tools: Vec<Box<dyn Tool>>,
    /// Shared opaque-capability store for resource links returned by upstream tools.
    pub resources: Arc<BridgedResourceStore>,
    /// Held only to keep upstream transports alive; never read directly.
    pub services: Vec<RunningService<RoleClient, ()>>,
    /// One human-readable line per configured server (connected or failed),
    /// printed in the startup banner so a bad path or handshake is never silent.
    pub report: Vec<String>,
}

/// One bridged tool: a thin proxy that forwards `call` to an upstream peer.
struct BridgedTool {
    /// The `<server>__<tool>` name advertised downstream. Leaked to `'static`
    /// because it is created once at startup and lives for the whole program.
    name: &'static str,
    /// The tool's real name on the upstream server.
    original_name: String,
    /// The upstream server's config key, for error messages.
    server: String,
    title: Option<String>,
    description: String,
    input_schema: Value,
    output_schema: Option<Value>,
    annotations: Option<ToolAnnotations>,
    icons: Option<Vec<Icon>>,
    meta: Option<MetaObject>,
    peer: Peer<RoleClient>,
    tool_timeout: Option<Duration>,
    resources: Arc<BridgedResourceStore>,
}

impl BridgedTool {
    async fn run(&self, args: Value, cancellation: Option<&CancellationToken>) -> ToolResult {
        let mut params = CallToolRequestParams::new(self.original_name.clone());
        if let Some(obj) = args.as_object()
            && !obj.is_empty()
        {
            params = params.with_arguments(obj.clone());
        }

        forward_tool_call(
            &self.peer,
            params,
            &self.server,
            &self.original_name,
            self.tool_timeout,
            cancellation,
            &self.resources,
        )
        .await
    }
}

#[async_trait]
impl Tool for BridgedTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn title(&self) -> String {
        self.title
            .clone()
            .unwrap_or_else(|| self.original_name.clone())
    }

    fn behavior(&self) -> ToolBehavior {
        let annotations = self.annotations.as_ref();
        ToolBehavior::new(
            annotations
                .and_then(|value| value.read_only_hint)
                .unwrap_or(false),
            annotations
                .and_then(|value| value.destructive_hint)
                .unwrap_or(true),
            annotations
                .and_then(|value| value.idempotent_hint)
                .unwrap_or(false),
            annotations
                .and_then(|value| value.open_world_hint)
                .unwrap_or(true),
            "Mirrors the upstream MCP annotations; omitted hints use the MCP defaults.",
        )
    }

    fn annotations(&self) -> Option<ToolAnnotations> {
        let behavior = self.behavior();
        let mut annotations = self.annotations.clone().unwrap_or_default();
        annotations.read_only_hint.get_or_insert(behavior.read_only);
        annotations
            .destructive_hint
            .get_or_insert(behavior.destructive);
        annotations
            .idempotent_hint
            .get_or_insert(behavior.idempotent);
        annotations
            .open_world_hint
            .get_or_insert(behavior.open_world);
        Some(annotations)
    }

    fn icons(&self) -> Option<Vec<Icon>> {
        self.icons.clone()
    }

    fn meta(&self) -> Option<MetaObject> {
        self.meta.clone()
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn requires_closed_input_schema(&self) -> bool {
        false
    }

    fn requires_closed_output_schema(&self) -> bool {
        false
    }

    fn call_identity(&self, _args: &Value) -> ToolCallIdentity {
        ToolCallIdentity::mcp(
            self.name(),
            self.server.clone(),
            Some(self.original_name.clone()),
        )
    }

    fn output_schema(&self) -> Option<Value> {
        self.output_schema.clone()
    }

    /// Bridged results are passed through verbatim; never synthesise a default
    /// structured result that would not match the upstream's own schema.
    fn fills_structured_content(&self) -> bool {
        false
    }

    fn permits_missing_structured_content(&self) -> bool {
        true
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        self.run(args, None).await
    }

    async fn call_with_context(
        &self,
        args: Value,
        _config: &AppConfig,
        _session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        self.run(args, Some(&context.cancellation)).await
    }
}

pub(crate) async fn forward_tool_call(
    peer: &Peer<RoleClient>,
    params: CallToolRequestParams,
    server: &str,
    tool: &str,
    tool_timeout: Option<Duration>,
    cancellation: Option<&CancellationToken>,
    resources: &BridgedResourceStore,
) -> ToolResult {
    let result = call_upstream_tool(peer, params, tool_timeout, cancellation).await;

    match result {
        Ok(result) => map_call_result(result, peer, server, tool_timeout, resources),
        Err(error) => ToolResult::error(format!(
            "Upstream MCP server '{server}' failed to run '{tool}': {error}"
        )),
    }
}

async fn call_upstream_tool(
    peer: &Peer<RoleClient>,
    params: CallToolRequestParams,
    timeout: Option<Duration>,
    cancellation: Option<&CancellationToken>,
) -> Result<CallToolResult, String> {
    let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
    let options = timeout
        .map(PeerRequestOptions::with_timeout)
        .unwrap_or_else(PeerRequestOptions::no_options);
    let handle = peer
        .send_cancellable_request(request, options)
        .await
        .map_err(|error| error.to_string())?;

    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        let _ = handle
            .cancel(Some("downstream request cancelled".to_string()))
            .await;
        return Err("cancelled by downstream client".to_string());
    }

    let response = if let Some(cancellation) = cancellation {
        let request_id = handle.id.clone();
        let cancel_peer = peer.clone();
        let response = handle.await_response();
        tokio::pin!(response);
        tokio::select! {
            response = &mut response => response,
            _ = cancellation.cancelled() => {
                let notification = CancelledNotification::new(CancelledNotificationParam::new(
                    Some(request_id),
                    Some("downstream request cancelled".to_string()),
                ));
                if let Err(error) = cancel_peer.send_notification(notification.into()).await {
                    tracing::debug!("could not forward upstream MCP cancellation: {error}");
                }
                return Err("cancelled by downstream client".to_string());
            }
        }
    } else {
        handle.await_response().await
    };

    let response = match response {
        Ok(response) => response,
        Err(ServiceError::Timeout { timeout }) => {
            return Err(format!("timed out after {}s", timeout.as_secs_f64()));
        }
        Err(error) => return Err(error.to_string()),
    };
    match response {
        ServerResult::CallToolResult(result) => Ok(result),
        ServerResult::InputRequiredResult(_) => {
            Err("requested additional interactive input, which Codexify cannot provide".into())
        }
        ServerResult::CreateTaskResult(_) => {
            Err("returned an asynchronous MCP task, which Codexify does not poll".into())
        }
        _ => Err("returned an unexpected response type".into()),
    }
}

/// Translate an upstream `CallToolResult` into this server's [`ToolResult`],
/// preserving text, images, error state, structured content, and result metadata.
/// Content blocks this server has no native representation for are rendered as
/// JSON text so nothing is silently dropped.
fn map_call_result(
    result: CallToolResult,
    peer: &Peer<RoleClient>,
    server: &str,
    tool_timeout: Option<Duration>,
    resources: &BridgedResourceStore,
) -> ToolResult {
    map_call_result_with(result, |resource| {
        match resources.register(peer.clone(), server, tool_timeout, resource) {
            Ok(resource) => ToolContent::ResourceLink(resource),
            Err(error) => ToolContent::Text(format!(
                "Bridged MCP resource link could not be exposed to the downstream client: {error}"
            )),
        }
    })
}

fn map_call_result_with(
    result: CallToolResult,
    mut map_resource: impl FnMut(rmcp::model::Resource) -> ToolContent,
) -> ToolResult {
    let content: Vec<ToolContent> = result
        .content
        .into_iter()
        .map(|block| match block {
            ContentBlock::Text(t) => ToolContent::Text(t.text),
            ContentBlock::Image(i) => ToolContent::Image {
                data: i.data,
                mime_type: i.mime_type,
            },
            ContentBlock::ResourceLink(resource) => map_resource(resource),
            other => ToolContent::Text(
                serde_json::to_string(&other).unwrap_or_else(|_| "[unrenderable content]".into()),
            ),
        })
        .collect();

    ToolResult {
        content,
        is_error: result.is_error.unwrap_or(false),
        structured_content: result.structured_content,
        meta: result.meta,
        audit: Default::default(),
    }
}

/// Reduce a name to `[A-Za-z0-9_]` — hyphens become underscores. Hyphens are
/// legal in MCP tool names but some function-calling layers (including how
/// ChatGPT maps MCP tools to OpenAI functions) reject them, which would silently
/// drop every bridged tool. Underscore-only names are accepted everywhere.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The downstream tool name `<server>__<tool>`, with both parts sanitised and
/// truncated to the 64-byte MCP limit if the concatenation overruns it.
fn bridged_name(server: &str, tool: &str) -> String {
    let mut name = format!("{}__{}", sanitize(server), sanitize(tool));
    if name.len() > 64 {
        name.truncate(64);
    }
    name
}

/// Make `base` unique against `used`, appending `_2`, `_3`, … (trimming the base
/// to keep the 64-byte limit). `base` is ASCII, so `truncate` is boundary-safe.
fn unique_name(base: String, used: &std::collections::HashSet<String>) -> String {
    if !used.contains(&base) {
        return base;
    }
    for n in 2..10_000 {
        let suffix = format!("_{n}");
        let keep = 64usize.saturating_sub(suffix.len());
        let mut cand = base.clone();
        cand.truncate(keep);
        cand.push_str(&suffix);
        if !used.contains(&cand) {
            return cand;
        }
    }
    base
}

/// Connect to every configured upstream MCP server, discover its tools, and
/// materialize its effective exposure mode. A server that fails to launch or
/// answer is logged and skipped — it never blocks startup.
pub async fn connect_upstreams(config: &AppConfig) -> Bridge {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    let mut services: Vec<RunningService<RoleClient, ()>> = Vec::new();
    let mut report: Vec<String> = Vec::new();
    let mut catalog_sources = Vec::new();
    let resources = Arc::new(BridgedResourceStore::new(config.artifact_egress.clone()));

    // Deterministic order so logs and tool ordering are stable across runs.
    let mut names: Vec<&String> = config.mcp_servers.keys().collect();
    names.sort();

    for server_name in names {
        let spec = &config.mcp_servers[server_name];
        if spec.disabled {
            report.push(format!("{server_name} -> disabled"));
            continue;
        }
        let sanitized = sanitize(server_name);
        let exposure = spec.exposure();

        match connect_one(server_name, spec, config).await {
            Ok((service, upstream_tools, tool_timeout)) => {
                let peer = service.peer().clone();
                let count = upstream_tools.len();

                match exposure {
                    McpToolExposure::Gateway => {
                        let functions: Vec<GatewayFunction> = upstream_tools
                            .iter()
                            .map(|t| GatewayFunction {
                                name: t.name.to_string(),
                                description: t
                                    .description
                                    .as_ref()
                                    .map(|c| c.to_string())
                                    .unwrap_or_default(),
                                input_schema: Value::Object((*t.input_schema).clone()),
                            })
                            .collect();

                        write_gateway_skill(config, server_name, &sanitized, &functions);

                        let leaked: &'static str = Box::leak(sanitized.clone().into_boxed_str());
                        tools.push(Box::new(GatewayTool {
                            name: leaked,
                            server: server_name.clone(),
                            description: gateway_description(server_name, &functions),
                            function_names: functions.iter().map(|f| f.name.clone()).collect(),
                            peer: peer.clone(),
                            tool_timeout,
                            resources: resources.clone(),
                        }));
                        report.push(format!(
                            "{server_name} -> gateway ({count} functions via `{sanitized}`)"
                        ));
                        tracing::info!(
                            "bridged MCP server '{server_name}' as gateway: {count} function(s)"
                        );
                    }
                    McpToolExposure::Direct => {
                        report.push(format!("{server_name} -> direct ({count} tool(s))"));
                        let mut used: std::collections::HashSet<String> =
                            std::collections::HashSet::new();
                        for tool in upstream_tools {
                            let original_name = tool.name.to_string();
                            let display =
                                unique_name(bridged_name(&sanitized, &original_name), &used);
                            used.insert(display.clone());
                            let leaked: &'static str = Box::leak(display.into_boxed_str());
                            let title = tool.title.filter(|title| !title.trim().is_empty());
                            let description = tool
                                .description
                                .map(|description| description.to_string())
                                .filter(|description| !description.trim().is_empty())
                                .unwrap_or_else(|| {
                                    format!(
                                        "Call the `{original_name}` tool on the `{server_name}` MCP server."
                                    )
                                });
                            tools.push(Box::new(BridgedTool {
                                name: leaked,
                                original_name,
                                server: server_name.clone(),
                                title,
                                description,
                                input_schema: Value::Object((*tool.input_schema).clone()),
                                output_schema: tool
                                    .output_schema
                                    .map(|schema| Value::Object((*schema).clone())),
                                annotations: tool.annotations,
                                icons: tool.icons,
                                meta: tool.meta,
                                peer: peer.clone(),
                                tool_timeout,
                                resources: resources.clone(),
                            }));
                        }
                        tracing::info!(
                            "bridged MCP server '{server_name}' directly: {count} tool(s)"
                        );
                    }
                    McpToolExposure::Catalog => {
                        report.push(format!(
                            "{server_name} -> catalog ({count} private tool(s))"
                        ));
                        catalog_sources.push(CatalogSourceInput {
                            raw_name: server_name.clone(),
                            provenance: spec.provenance,
                            transport: transport_label(spec),
                            peer_info: peer.peer_info(),
                            tools: upstream_tools,
                            peer: peer.clone(),
                            tool_timeout,
                        });
                        tracing::info!(
                            "indexed MCP server '{server_name}' privately: {count} tool(s)"
                        );
                    }
                }
                services.push(service);
            }
            Err(e) => {
                report.push(format!("{server_name} -> FAILED: {e}"));
                tracing::warn!("skipping MCP server '{server_name}': {e}");
            }
        }
    }

    let mut catalog_tools = build_catalog_tools(catalog_sources, resources.clone());
    catalog_tools.append(&mut tools);
    tools = catalog_tools;

    Bridge {
        tools,
        resources,
        services,
        report,
    }
}

/// One function exposed by a gateway-mode server.
struct GatewayFunction {
    name: String,
    description: String,
    input_schema: Value,
}

/// The dispatcher tool's description: a compact list of function names + a
/// one-line summary each (kept small to stay well under per-tool size limits).
fn gateway_description(server: &str, functions: &[GatewayFunction]) -> String {
    let list = functions
        .iter()
        .map(|f| {
            let summary = f.description.lines().next().unwrap_or("").trim();
            let summary: String = summary.chars().take(100).collect();
            if summary.is_empty() {
                format!("- {}", f.name)
            } else {
                format!("- {}: {summary}", f.name)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Gateway to the '{server}' MCP server — call any of its {n} functions through this one tool.\n\n\
         Call it as {{ \"function\": \"<name>\", \"arguments\": {{ ... }} }}. For each function's exact \
         arguments, read the '{server}' skill with skills_read (skills_read name=\"{server}\").\n\n\
         Functions:\n{list}",
        n = functions.len(),
    )
}

/// Write the auto-generated SKILL.md for a gateway server, documenting every
/// function and its argument schema. Best-effort: a write failure only means the
/// skill is unavailable, the gateway tool still works.
fn write_gateway_skill(
    config: &AppConfig,
    server: &str,
    sanitized: &str,
    functions: &[GatewayFunction],
) {
    let Some(base) = &config.generated_skills_dir else {
        return;
    };
    let dir = base.join(server);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }

    // Serialise the frontmatter through serde_yaml so a server name containing
    // YAML metacharacters (a colon, `{`, quotes, …) still produces valid YAML
    // and the skill is not silently dropped at parse time.
    let mut fm = serde_yaml::Mapping::new();
    fm.insert("name".into(), serde_yaml::Value::String(server.to_string()));
    fm.insert(
        "description".into(),
        serde_yaml::Value::String(format!(
            "Call the {server} MCP server's {n} functions through the `{sanitized}` gateway tool. Use when a task needs {server} operations.",
            n = functions.len(),
        )),
    );
    let frontmatter = serde_yaml::to_string(&fm).unwrap_or_else(|_| format!("name: {server}\n"));

    let mut md = String::new();
    md.push_str("---\n");
    md.push_str(&frontmatter);
    md.push_str("---\n\n");
    md.push_str(&format!("# {server} — {} functions\n\n", functions.len()));
    md.push_str(&format!(
        "Every function is invoked through the single `{sanitized}` tool:\n\n\
         ```json\n{{ \"function\": \"<name>\", \"arguments\": {{ ... }} }}\n```\n\n\
         The `arguments` object must match the function's schema below.\n\n",
    ));
    for f in functions {
        md.push_str(&format!("## {}\n\n", f.name));
        if !f.description.is_empty() {
            md.push_str(&f.description);
            md.push_str("\n\n");
        }
        md.push_str("Arguments:\n\n```json\n");
        md.push_str(&serde_json::to_string_pretty(&f.input_schema).unwrap_or_else(|_| "{}".into()));
        md.push_str("\n```\n\n");
    }

    let _ = std::fs::write(dir.join("SKILL.md"), md);
}

/// One gateway tool proxying a whole upstream server. `call` forwards
/// `{function, arguments}` to the upstream by the function's real name.
struct GatewayTool {
    name: &'static str,
    server: String,
    description: String,
    function_names: Vec<String>,
    peer: Peer<RoleClient>,
    tool_timeout: Option<Duration>,
    resources: Arc<BridgedResourceStore>,
}

impl GatewayTool {
    async fn run(&self, args: Value, cancellation: Option<&CancellationToken>) -> ToolResult {
        let Some(function) = args.get("function").and_then(|v| v.as_str()) else {
            return ToolResult::error("`function` is required (the name of the function to call)");
        };
        if !self.function_names.iter().any(|f| f == function) {
            return ToolResult::error(format!(
                "Unknown {} function '{function}'. Read the '{}' skill for the list.",
                self.server, self.server
            ));
        }

        let mut params = CallToolRequestParams::new(function.to_string());
        match args.get("arguments") {
            None | Some(Value::Null) => {}
            Some(Value::Object(obj)) => {
                if !obj.is_empty() {
                    params = params.with_arguments(obj.clone());
                }
            }
            Some(_) => {
                return ToolResult::error(
                    "`arguments` must be a JSON object (an object mapping the function's parameter names to values)",
                );
            }
        }

        forward_tool_call(
            &self.peer,
            params,
            &self.server,
            function,
            self.tool_timeout,
            cancellation,
            &self.resources,
        )
        .await
    }
}

#[async_trait]
impl Tool for GatewayTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn title(&self) -> String {
        format!("Call {} MCP tool", self.server)
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            true,
            false,
            true,
            "A gateway can select upstream tools with arbitrary local, destructive, or external side effects.",
        )
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "function": {
                    "type": "string",
                    "enum": self.function_names,
                    "description": format!("The {} function to call.", self.server)
                },
                "arguments": {
                    "type": "object",
                    "description": "Arguments for the chosen function (see the skill for each function's schema)."
                }
            },
            "required": ["function"],
            "additionalProperties": false
        })
    }

    fn call_identity(&self, args: &Value) -> ToolCallIdentity {
        ToolCallIdentity::mcp(
            self.name(),
            self.server.clone(),
            args.get("function")
                .and_then(Value::as_str)
                .map(str::to_string),
        )
    }

    fn fills_structured_content(&self) -> bool {
        false
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        self.run(args, None).await
    }

    async fn call_with_context(
        &self,
        args: Value,
        _config: &AppConfig,
        _session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        self.run(args, Some(&context.cancellation)).await
    }
}

/// Launch and initialise one upstream, returning its running service and tools.
async fn connect_one(
    server_name: &str,
    spec: &crate::types::McpServerSpec,
    config: &AppConfig,
) -> Result<
    (
        RunningService<RoleClient, ()>,
        Vec<rmcp::model::Tool>,
        Option<Duration>,
    ),
    String,
> {
    connect_one_with_env(server_name, spec, config, &|name| std::env::var(name).ok()).await
}

#[derive(Debug)]
enum UpstreamTransport<'a> {
    Stdio(&'a str),
    StreamableHttp(&'a str),
}

fn transport_label(spec: &crate::types::McpServerSpec) -> String {
    if spec.command.is_some() {
        "stdio".to_string()
    } else {
        "streamable-http".to_string()
    }
}

fn select_transport(spec: &crate::types::McpServerSpec) -> Result<UpstreamTransport<'_>, String> {
    if spec.command.is_some() && spec.url.is_some() {
        return Err("both \"command\" and \"url\" are configured".to_string());
    }
    if spec.command.is_some() {
        let mut incompatible = Vec::new();
        if spec.bearer_token_env_var.is_some() {
            incompatible.push("bearerTokenEnvVar");
        }
        if !spec.http_headers.is_empty() {
            incompatible.push("httpHeaders");
        }
        if !spec.env_http_headers.is_empty() {
            incompatible.push("envHttpHeaders");
        }
        if !incompatible.is_empty() {
            return Err(format!(
                "stdio transport cannot configure {}",
                incompatible.join(", ")
            ));
        }
    }
    if spec.url.is_some() {
        let mut incompatible = Vec::new();
        if !spec.args.is_empty() {
            incompatible.push("args");
        }
        if !spec.env.is_empty() {
            incompatible.push("env");
        }
        if spec.cwd.is_some() {
            incompatible.push("cwd");
        }
        if !incompatible.is_empty() {
            return Err(format!(
                "Streamable HTTP transport cannot configure {}",
                incompatible.join(", ")
            ));
        }
    }

    let kind = spec.transport.as_deref().map(str::to_ascii_lowercase);
    match kind.as_deref() {
        None | Some("stdio") if spec.command.is_some() => spec
            .command
            .as_deref()
            .filter(|command| !command.trim().is_empty())
            .map(UpstreamTransport::Stdio)
            .ok_or_else(|| "a stdio server needs a non-empty \"command\"".to_string()),
        None | Some("http" | "streamable-http" | "streamable_http")
            if spec.url.is_some() =>
        {
            spec.url
                .as_deref()
                .filter(|url| !url.trim().is_empty())
                .map(UpstreamTransport::StreamableHttp)
                .ok_or_else(|| {
                    "a Streamable HTTP server needs a non-empty \"url\"".to_string()
                })
        }
        Some("sse") => Err(
            "legacy SSE transport is not supported by current Codex; configure a Streamable HTTP endpoint"
                .to_string(),
        ),
        Some("websocket" | "ws") => Err(
            "WebSocket transport is not supported by current Codex; configure stdio or Streamable HTTP"
                .to_string(),
        ),
        Some("stdio") => Err("type \"stdio\" requires \"command\", not \"url\"".to_string()),
        Some("http" | "streamable-http" | "streamable_http") => Err(
            "Streamable HTTP transport requires \"url\", not \"command\"".to_string(),
        ),
        Some(other) => Err(format!("unsupported MCP transport type \"{other}\"")),
        None => Err("neither \"command\" nor \"url\" is configured".to_string()),
    }
}

fn configured_timeout(
    value: Option<f64>,
    field: &str,
    default: Option<Duration>,
) -> Result<Option<Duration>, String> {
    match value {
        Some(seconds) => Duration::try_from_secs_f64(seconds)
            .map(Some)
            .map_err(|_| format!("{field} must be a non-negative finite number")),
        None => Ok(default),
    }
}

fn resolve_bearer_token(
    server_name: &str,
    env_var: Option<&str>,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<String>, String> {
    let Some(env_var) = env_var else {
        return Ok(None);
    };
    let value = env_lookup(env_var).ok_or_else(|| {
        format!("environment variable {env_var} for MCP server '{server_name}' is not set")
    })?;
    if value.is_empty() {
        return Err(format!(
            "environment variable {env_var} for MCP server '{server_name}' is empty"
        ));
    }
    Ok(Some(value))
}

fn insert_header(
    headers: &mut HashMap<HeaderName, HeaderValue>,
    name: &str,
    value: &str,
    source: &str,
) {
    let header_name = match HeaderName::from_bytes(name.as_bytes()) {
        Ok(name) => name,
        Err(error) => {
            tracing::warn!("invalid upstream MCP HTTP header name `{name}` from {source}: {error}");
            return;
        }
    };
    let header_value = match HeaderValue::from_str(value) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                "invalid upstream MCP HTTP header value for `{name}` from {source}: {error}"
            );
            return;
        }
    };
    headers.insert(header_name, header_value);
}

fn build_http_headers(
    spec: &crate::types::McpServerSpec,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> HashMap<HeaderName, HeaderValue> {
    let mut headers = HashMap::new();
    insert_header(
        &mut headers,
        "user-agent",
        concat!("codexify/", env!("CARGO_PKG_VERSION")),
        "Codexify",
    );

    let mut static_headers: Vec<_> = spec.http_headers.iter().collect();
    static_headers.sort_by(|left, right| left.0.cmp(right.0));
    for (name, value) in static_headers {
        insert_header(&mut headers, name, value, "httpHeaders");
    }

    let mut env_headers: Vec<_> = spec.env_http_headers.iter().collect();
    env_headers.sort_by(|left, right| left.0.cmp(right.0));
    for (name, env_var) in env_headers {
        let Some(value) = env_lookup(env_var) else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        insert_header(&mut headers, name, &value, env_var);
    }
    headers
}

fn configures_authorization_header(spec: &crate::types::McpServerSpec) -> bool {
    spec.http_headers
        .keys()
        .chain(spec.env_http_headers.keys())
        .any(|name| name.eq_ignore_ascii_case(AUTHORIZATION.as_str()))
}

/// Build the reqwest client used for upstream Streamable HTTP servers.
///
/// The redirect policy is set to `none` in our own code rather than relying on
/// RMCP's default client: a redirect would otherwise let the upstream replay our
/// caller-supplied `Authorization` and custom headers to an arbitrary location,
/// so we own that guarantee here where it cannot regress under a dependency
/// bump. Idle connection pooling is disabled for the same reason RMCP's default
/// client disables it — to avoid TCP Delayed-ACK stalls when a response body is
/// not fully drained before the pooled connection is reused.
fn build_upstream_client() -> Result<reqwest::Client, String> {
    crate::tls::client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|error| format!("could not build the upstream HTTP client: {error}"))
}

async fn connect_one_with_env(
    server_name: &str,
    spec: &crate::types::McpServerSpec,
    config: &AppConfig,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<
    (
        RunningService<RoleClient, ()>,
        Vec<rmcp::model::Tool>,
        Option<Duration>,
    ),
    String,
> {
    let startup_timeout = configured_timeout(
        spec.startup_timeout_sec,
        "startupTimeoutSec",
        Some(CONNECT_TIMEOUT),
    )?
    .expect("startup timeout has a default");
    let tool_timeout = configured_timeout(spec.tool_timeout_sec, "toolTimeoutSec", None)?;

    let connect = match select_transport(spec)? {
        UpstreamTransport::Stdio(command_path) => {
            let mut command = Command::new(command_path);
            command.args(&spec.args);
            for (key, value) in &spec.env {
                command.env(key, value);
            }
            if let Some(cwd) = spec.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
                command.current_dir(cwd);
            }
            scrub_untrusted_child_env(&mut command, config);

            let transport = TokioChildProcess::new(command)
                .map_err(|error| format!("could not launch '{command_path}': {error}"))?;
            let connect = async {
                let service = ().serve(transport).await.map_err(|error| error.to_string())?;
                let tools = service
                    .list_all_tools()
                    .await
                    .map_err(|error| error.to_string())?;
                Ok::<_, String>((service, tools))
            };
            tokio::time::timeout(startup_timeout, connect).await
        }
        UpstreamTransport::StreamableHttp(url) => {
            let bearer_token = resolve_bearer_token(
                server_name,
                spec.bearer_token_env_var.as_deref(),
                env_lookup,
            )?;
            if bearer_token.is_some() && configures_authorization_header(spec) {
                return Err(
                    "configure either bearerTokenEnvVar or an Authorization HTTP header, not both"
                        .to_string(),
                );
            }
            let headers = build_http_headers(spec, env_lookup);

            let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url);
            if let Some(token) = bearer_token {
                transport_config = transport_config.auth_header(token);
            }
            transport_config = transport_config.custom_headers(headers);
            let transport = StreamableHttpClientTransport::with_client(
                build_upstream_client()?,
                transport_config,
            );
            let connect = async {
                let service = ().serve(transport).await.map_err(|error| error.to_string())?;
                let tools = service
                    .list_all_tools()
                    .await
                    .map_err(|error| error.to_string())?;
                Ok::<_, String>((service, tools))
            };
            tokio::time::timeout(startup_timeout, connect).await
        }
    };

    match connect {
        Ok(Ok((service, mut tools))) => {
            // Codex applies the deny-list after the allow-list; use the same
            // order so imported filtering has identical results.
            tools.retain(|tool| tool_is_enabled(spec, tool.name.as_ref()));
            Ok((service, tools, tool_timeout))
        }
        Ok(Err(error)) => Err(error),
        Err(_) => Err(format!(
            "timed out after {}s waiting for '{server_name}' to initialise",
            startup_timeout.as_secs_f64()
        )),
    }
}

fn tool_is_enabled(spec: &crate::types::McpServerSpec, name: &str) -> bool {
    let allowed = spec
        .tools
        .as_ref()
        .is_none_or(|tools| tools.iter().any(|tool| tool == name));
    let denied = spec
        .disabled_tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|tool| tool == name));
    allowed && !denied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_catalog::{MCP_CALL_TOOL, MCP_GET_TOOL, MCP_LIST_SOURCES, MCP_SEARCH_TOOLS};
    use crate::types::{McpServerProvenance, McpServerSpec, McpToolExposure, ToolContent};
    use axum::{
        Router,
        extract::{Request, State},
        middleware::Next,
        response::{IntoResponse, Response},
    };
    use rmcp::{
        ErrorData as McpError, ServerHandler,
        model::{
            CallToolResponse, Icon, Implementation, InitializeResult, ListToolsResult, MetaObject,
            PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
            ReadResourceResult, Resource, ResourceContents, ServerCapabilities, ServerInfo,
            ToolAnnotations,
        },
        service::{RequestContext, RoleServer},
        transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    };
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct ExpectedHeaders {
        authorization: &'static str,
        static_value: &'static str,
        env_value: &'static str,
    }

    async fn require_expected_headers(
        State(expected): State<ExpectedHeaders>,
        request: Request,
        next: Next,
    ) -> Response {
        let headers = request.headers();
        let matches = |name: &str, value: &str| {
            headers.get(name).and_then(|actual| actual.to_str().ok()) == Some(value)
        };
        if !matches("authorization", expected.authorization)
            || !matches("x-static", expected.static_value)
            || !matches("x-env", expected.env_value)
        {
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
        next.run(request).await
    }

    #[derive(Clone)]
    struct TestHttpMcp;

    impl ServerHandler for TestHttpMcp {
        fn get_info(&self) -> ServerInfo {
            InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
                .with_server_info(Implementation::new("test-http-upstream", "1.0.0"))
                .with_instructions("Use echo for immediate replies and slow to test timeouts.")
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, McpError> {
            let schema = json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
                "additionalProperties": false
            })
            .as_object()
            .cloned()
            .unwrap();
            Ok(ListToolsResult::with_all_items(vec![
                rmcp::model::Tool::new("echo", "Echo the supplied text", schema.clone()),
                rmcp::model::Tool::new("slow", "Return after a delay", schema),
            ]))
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, McpError> {
            let text = request
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if request.name.as_ref() == "slow" {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
        }
    }

    async fn spawn_http_upstream() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let mut config = StreamableHttpServerConfig::default();
        config.json_response = true;
        let service = StreamableHttpService::new(
            || Ok(TestHttpMcp),
            Arc::new(LocalSessionManager::default()),
            config,
        );
        let expected = ExpectedHeaders {
            authorization: "Bearer remote-token",
            static_value: "static-value",
            env_value: "env-value",
        };
        let app = Router::new().nest_service("/mcp", service).layer(
            axum::middleware::from_fn_with_state(expected, require_expected_headers),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/mcp"), task)
    }

    #[derive(Clone)]
    struct ResourceTestMcp;

    impl ServerHandler for ResourceTestMcp {
        fn get_info(&self) -> ServerInfo {
            InitializeResult::new(
                ServerCapabilities::builder()
                    .enable_tools()
                    .enable_resources()
                    .build(),
            )
            .with_server_info(Implementation::new("resource-test-upstream", "1.0.0"))
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, McpError> {
            let schema = object_schema(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }));
            Ok(ListToolsResult::with_all_items(vec![
                rmcp::model::Tool::new(
                    "download",
                    "Expose a downloadable binary resource",
                    schema.clone(),
                ),
                rmcp::model::Tool::new(
                    "slow_download",
                    "Expose a resource whose read is intentionally slow",
                    schema,
                ),
            ]))
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, McpError> {
            let (uri, name) = match request.name.as_ref() {
                "download" => ("fixture://artifact/report.bin", "report.bin"),
                "slow_download" => ("fixture://artifact/slow.bin", "slow.bin"),
                _ => {
                    return Err(McpError::invalid_params(
                        "unknown resource fixture tool".to_string(),
                        None,
                    ));
                }
            };
            let resource = Resource::new(uri, name)
                .with_mime_type("application/octet-stream")
                .with_size(4);
            Ok(CallToolResult::success(vec![ContentBlock::resource_link(resource)]).into())
        }

        async fn read_resource(
            &self,
            request: ReadResourceRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<ReadResourceResponse, McpError> {
            match request.uri.as_str() {
                "fixture://artifact/report.bin" => Ok(ReadResourceResult::new(vec![
                    ResourceContents::blob("AAEC/w==", request.uri)
                        .with_mime_type("application/octet-stream"),
                ])
                .into()),
                "fixture://artifact/slow.bin" => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Ok(ReadResourceResult::new(vec![
                        ResourceContents::blob("AAEC/w==", request.uri)
                            .with_mime_type("application/octet-stream"),
                    ])
                    .into())
                }
                _ => Err(McpError::resource_not_found(
                    "fixture resource not found".to_string(),
                    None,
                )),
            }
        }
    }

    async fn spawn_resource_upstream() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let mut config = StreamableHttpServerConfig::default();
        config.json_response = true;
        let service = StreamableHttpService::new(
            || Ok(ResourceTestMcp),
            Arc::new(LocalSessionManager::default()),
            config,
        );
        let app = Router::new().nest_service("/mcp", service);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/mcp"), task)
    }

    #[derive(Debug, Clone, PartialEq)]
    struct RecordedCall {
        name: String,
        arguments: Option<serde_json::Map<String, Value>>,
    }

    #[derive(Clone)]
    struct CatalogTestMcp {
        calls: Arc<Mutex<Vec<RecordedCall>>>,
    }

    fn object_schema(value: Value) -> serde_json::Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    fn catalog_test_tools() -> Vec<rmcp::model::Tool> {
        let generic_schema = object_schema(json!({
            "type": "object",
            "properties": {
                "value": {
                    "type": "string",
                    "description": "Value for this generic IDA operation"
                }
            },
            "additionalProperties": false
        }));
        let mut tools = (0..66)
            .map(|index| {
                rmcp::model::Tool::new(
                    format!("ida_operation_{index:02}"),
                    format!("Generic IDA operation number {index}"),
                    generic_schema.clone(),
                )
            })
            .collect::<Vec<_>>();

        let decompile_input = object_schema(json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "object",
                    "description": "Target code location",
                    "properties": {
                        "virtual_address": {
                            "type": "string",
                            "description": "Function virtual address to decompile"
                        }
                    },
                    "required": ["virtual_address"],
                    "additionalProperties": false
                }
            },
            "required": ["location"],
            "additionalProperties": false
        }));
        let decompile_output = object_schema(json!({
            "type": "object",
            "properties": {
                "rawTool": { "type": "string" },
                "address": { "type": "string" }
            },
            "required": ["rawTool", "address"],
            "additionalProperties": false
        }));
        let mut tool_meta = MetaObject::new();
        tool_meta
            .0
            .insert("vendor".into(), Value::String("hex-rays".into()));
        tools.push(
            rmcp::model::Tool::new(
                "decompile_function",
                "Decompile a function and recover high-level pseudocode",
                decompile_input,
            )
            .with_title("Decompile function")
            .with_raw_output_schema(Arc::new(decompile_output))
            .with_annotations(
                ToolAnnotations::with_title("Decompiler action")
                    .read_only(true)
                    .destructive(false)
                    .idempotent(true)
                    .open_world(false),
            )
            .with_icons(vec![Icon::new("https://example.invalid/decompiler.svg")])
            .with_meta(tool_meta),
        );
        tools.push(rmcp::model::Tool::new(
            "rename-function",
            "Rename a function by address",
            generic_schema.clone(),
        ));
        tools.push(rmcp::model::Tool::new(
            "rename_function",
            "Rename a function by symbol",
            generic_schema.clone(),
        ));
        tools.push(rmcp::model::Tool::new(
            "passthrough",
            "Return mixed content, structured data, and result metadata",
            generic_schema.clone(),
        ));
        tools.push(rmcp::model::Tool::new(
            "error_result",
            "Return a caller-visible tool error",
            generic_schema,
        ));
        tools
    }

    impl ServerHandler for CatalogTestMcp {
        fn get_info(&self) -> ServerInfo {
            let mut implementation = Implementation::new("catalog-test-upstream", "9.8.7");
            implementation.title = Some("IDA semantic analysis bridge".to_string());
            implementation.description = Some("Metadata-rich many-tool MCP fixture".to_string());
            InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
                .with_server_info(implementation)
                .with_instructions(
                    "Search for decompilation, renaming, and program-analysis operations.",
                )
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, McpError> {
            Ok(ListToolsResult::with_all_items(catalog_test_tools()))
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, McpError> {
            self.calls.lock().unwrap().push(RecordedCall {
                name: request.name.to_string(),
                arguments: request.arguments.clone(),
            });

            match request.name.as_ref() {
                "decompile_function" => {
                    let address = request
                        .arguments
                        .as_ref()
                        .and_then(|arguments| arguments.get("location"))
                        .and_then(Value::as_object)
                        .and_then(|location| location.get("virtual_address"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    Ok(CallToolResult::structured(json!({
                        "rawTool": request.name,
                        "address": address
                    }))
                    .into())
                }
                "passthrough" => {
                    let mut result_meta = MetaObject::new();
                    result_meta
                        .0
                        .insert("trace".into(), Value::String("fixture-result-meta".into()));
                    let mut result = CallToolResult::success(vec![
                        ContentBlock::text("mixed text"),
                        ContentBlock::image("aGVsbG8=", "image/png"),
                    ])
                    .with_meta(Some(result_meta));
                    result.structured_content = Some(json!({ "kind": "mixed", "ok": true }));
                    Ok(result.into())
                }
                "error_result" => Ok(CallToolResult::error(vec![ContentBlock::text(
                    "fixture tool error",
                )])
                .into()),
                _ => Ok(CallToolResult::success(vec![ContentBlock::text(
                    request.name.to_string(),
                )])
                .into()),
            }
        }
    }

    async fn spawn_catalog_upstream() -> (
        String,
        Arc<Mutex<Vec<RecordedCall>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory_calls = calls.clone();
        let mut config = StreamableHttpServerConfig::default();
        config.json_response = true;
        let service = StreamableHttpService::new(
            move || {
                Ok(CatalogTestMcp {
                    calls: factory_calls.clone(),
                })
            },
            Arc::new(LocalSessionManager::default()),
            config,
        );
        let app = Router::new().nest_service("/mcp", service);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/mcp"), calls, task)
    }

    fn bridge_tool<'a>(bridge: &'a Bridge, name: &str) -> &'a dyn Tool {
        bridge
            .tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(|tool| tool.as_ref())
            .unwrap_or_else(|| panic!("missing bridge tool {name}"))
    }

    async fn call_bridge_tool(bridge: &Bridge, name: &str, args: Value) -> ToolResult {
        let config = crate::config::default_config(std::env::temp_dir());
        bridge_tool(bridge, name)
            .call(args, &config, &SessionState::new())
            .await
    }

    async fn call_resource_fixture(bridge: &Bridge, exposure: McpToolExposure) -> ToolResult {
        match exposure {
            McpToolExposure::Direct => call_bridge_tool(bridge, "files__download", json!({})).await,
            McpToolExposure::Gateway => {
                call_bridge_tool(
                    bridge,
                    "files",
                    json!({ "function": "download", "arguments": {} }),
                )
                .await
            }
            McpToolExposure::Catalog => {
                let listed = call_bridge_tool(bridge, MCP_LIST_SOURCES, json!({}))
                    .await
                    .structured_content
                    .unwrap();
                let source = listed["sources"][0]["id"].as_str().unwrap();
                let searched = call_bridge_tool(
                    bridge,
                    MCP_SEARCH_TOOLS,
                    json!({ "query": "download binary resource", "source": source, "limit": 5 }),
                )
                .await
                .structured_content
                .unwrap();
                let tool = searched["matches"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|entry| entry["toolName"] == "download")
                    .and_then(|entry| entry["toolId"].as_str())
                    .unwrap();
                call_bridge_tool(
                    bridge,
                    MCP_CALL_TOOL,
                    json!({ "source": source, "tool": tool, "arguments": {} }),
                )
                .await
            }
        }
    }

    #[test]
    fn unique_name_dedups_with_suffix() {
        let mut used = std::collections::HashSet::new();
        assert_eq!(unique_name("fresh".into(), &used), "fresh");
        used.insert("s__a_b".to_string());
        let n = unique_name("s__a_b".into(), &used);
        assert_ne!(n, "s__a_b");
        assert!(n.len() <= 64);
        // A collision of two already-max-length names still yields a unique <=64 name.
        let base = "x".repeat(64);
        let mut used2 = std::collections::HashSet::new();
        used2.insert(base.clone());
        let n2 = unique_name(base.clone(), &used2);
        assert_ne!(n2, base);
        assert!(n2.len() <= 64);
    }

    #[test]
    fn gateway_skill_frontmatter_is_yaml_safe() {
        // A server name with YAML metacharacters must still yield parseable
        // frontmatter (so the generated skill is not silently dropped).
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = crate::config::default_config(std::env::temp_dir());
        config.generated_skills_dir = Some(tmp.path().to_path_buf());
        let server = "{weird"; // valid dir name, breaks naive `name: {weird`
        let funcs = vec![GatewayFunction {
            name: "do_it".into(),
            description: "does a thing".into(),
            input_schema: json!({ "type": "object" }),
        }];
        write_gateway_skill(&config, server, "weird", &funcs);
        let contents = std::fs::read_to_string(tmp.path().join(server).join("SKILL.md"))
            .expect("skill written");
        let fm = crate::skills::parse_skill_frontmatter(&contents, server)
            .expect("generated frontmatter must parse");
        assert_eq!(fm.name, server);
    }

    #[test]
    fn sanitize_and_name() {
        assert_eq!(sanitize("ida sql!"), "ida_sql_");
        // Hyphens are replaced so downstream function-calling layers accept them.
        assert_eq!(sanitize("remote-exec"), "remote_exec");
        assert_eq!(
            bridged_name("remote-exec", "machine_add"),
            "remote_exec__machine_add"
        );
        assert_eq!(bridged_name("ida", "decompile"), "ida__decompile");
        // Overlong names truncate to the 64-byte MCP limit.
        let long = bridged_name("server", &"x".repeat(100));
        assert_eq!(long.len(), 64);
    }

    #[test]
    fn maps_text_and_structured_result() {
        let map = |_: rmcp::model::Resource| unreachable!("no resource link in fixture");
        let r = map_call_result_with(CallToolResult::success(vec![ContentBlock::text("hi")]), map);
        assert!(!r.is_error);
        assert_eq!(r.joined_text(), "hi");

        let s = map_call_result_with(
            CallToolResult::structured(serde_json::json!({ "k": "v" })),
            map,
        );
        assert_eq!(s.structured_content, Some(serde_json::json!({ "k": "v" })));

        let e = map_call_result_with(CallToolResult::error(vec![ContentBlock::text("boom")]), map);
        assert!(e.is_error);
    }

    #[tokio::test]
    async fn bridged_resource_links_round_trip_in_every_exposure_mode() {
        for exposure in [
            McpToolExposure::Direct,
            McpToolExposure::Gateway,
            McpToolExposure::Catalog,
        ] {
            let (url, server_task) = spawn_resource_upstream().await;
            let mut config = crate::config::default_config(std::env::temp_dir());
            config.mcp_servers.insert(
                "files".to_string(),
                McpServerSpec {
                    url: Some(url),
                    mode: Some(exposure),
                    ..Default::default()
                },
            );

            let bridge = connect_upstreams(&config).await;
            let result = call_resource_fixture(&bridge, exposure).await;
            assert!(!result.is_error, "{}", result.joined_text());
            let link = result
                .content
                .iter()
                .find_map(|content| match content {
                    ToolContent::ResourceLink(resource) => Some(resource),
                    _ => None,
                })
                .expect("bridged result should expose a resource link");
            assert!(
                link.uri
                    .starts_with(crate::bridged_resources::BRIDGED_RESOURCE_URI_PREFIX)
            );
            assert!(!link.uri.contains("fixture://"));
            assert_eq!(link.name, "report.bin");
            assert_eq!(link.mime_type.as_deref(), Some("application/octet-stream"));
            assert_eq!(link.size, Some(4));

            let read = bridge
                .resources
                .read_resource(&link.uri, &CancellationToken::new())
                .await
                .unwrap()
                .expect("opaque bridged resource should be readable");
            assert_eq!(read.contents.len(), 1);
            assert_eq!(read.cache_scope, Some(rmcp::model::CacheScope::Private));
            match &read.contents[0] {
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    ..
                } => {
                    assert_eq!(uri, &link.uri);
                    assert_eq!(mime_type.as_deref(), Some("application/octet-stream"));
                    assert_eq!(blob, "AAEC/w==");
                    assert!(!uri.contains("fixture://"));
                }
                _ => panic!("expected blob resource content"),
            }

            drop(bridge);
            server_task.abort();
            let _ = server_task.await;
        }
    }

    #[tokio::test]
    async fn bridged_resource_capabilities_expire_and_reads_are_cancellable() {
        let (url, server_task) = spawn_resource_upstream().await;
        let mut config = crate::config::default_config(std::env::temp_dir());
        config.artifact_egress.reference_ttl_ms = 5;
        config.mcp_servers.insert(
            "files".to_string(),
            McpServerSpec {
                url: Some(url.clone()),
                mode: Some(McpToolExposure::Direct),
                ..Default::default()
            },
        );

        let bridge = connect_upstreams(&config).await;
        let result = call_bridge_tool(&bridge, "files__download", json!({})).await;
        let expired_uri = result
            .content
            .iter()
            .find_map(|content| match content {
                ToolContent::ResourceLink(resource) => Some(resource.uri.clone()),
                _ => None,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
        assert!(
            bridge
                .resources
                .read_resource(&expired_uri, &CancellationToken::new())
                .await
                .unwrap()
                .is_none()
        );

        drop(bridge);
        config.artifact_egress = crate::types::ArtifactEgressConfig::default();
        let bridge = connect_upstreams(&config).await;
        let slow = call_bridge_tool(&bridge, "files__slow_download", json!({})).await;
        let slow_uri = slow
            .content
            .iter()
            .find_map(|content| match content {
                ToolContent::ResourceLink(resource) => Some(resource.uri.clone()),
                _ => None,
            })
            .unwrap();
        let cancellation = CancellationToken::new();
        let cancel_task = {
            let cancellation = cancellation.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                cancellation.cancel();
            })
        };
        let started = std::time::Instant::now();
        let error = bridge
            .resources
            .read_resource(&slow_uri, &cancellation)
            .await
            .unwrap_err();
        cancel_task.await.unwrap();
        assert!(
            error
                .to_string()
                .contains("cancelled by the downstream client")
        );
        assert!(started.elapsed() < Duration::from_millis(150));

        drop(bridge);
        server_task.abort();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn catalog_mode_keeps_many_tools_private_and_supports_progressive_discovery() {
        let (url, calls, server_task) = spawn_catalog_upstream().await;
        let mut config = crate::config::default_config(std::env::temp_dir());
        config.mcp_servers.insert(
            "IDA MCP!".to_string(),
            McpServerSpec {
                url: Some(url),
                provenance: McpServerProvenance::CodexConfig,
                ..Default::default()
            },
        );

        let bridge = connect_upstreams(&config).await;
        let names = bridge
            .tools
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                MCP_LIST_SOURCES,
                MCP_SEARCH_TOOLS,
                MCP_GET_TOOL,
                MCP_CALL_TOOL
            ]
        );
        assert!(
            bridge
                .tools
                .iter()
                .all(|tool| !tool.requires_project_root())
        );
        assert!(bridge.tools.iter().all(|tool| !tool.title().is_empty()));
        assert!(
            bridge_tool(&bridge, MCP_LIST_SOURCES)
                .output_schema()
                .is_some()
        );
        assert!(
            bridge_tool(&bridge, MCP_SEARCH_TOOLS)
                .output_schema()
                .is_some()
        );
        assert!(bridge_tool(&bridge, MCP_GET_TOOL).output_schema().is_some());
        assert!(
            bridge_tool(&bridge, MCP_CALL_TOOL)
                .output_schema()
                .is_none()
        );
        let discovery_annotations = bridge_tool(&bridge, MCP_SEARCH_TOOLS)
            .annotations()
            .unwrap();
        assert_eq!(discovery_annotations.read_only_hint, Some(true));
        assert_eq!(discovery_annotations.destructive_hint, Some(false));
        assert_eq!(discovery_annotations.open_world_hint, Some(false));
        let dispatcher_annotations = bridge_tool(&bridge, MCP_CALL_TOOL).annotations().unwrap();
        assert_eq!(dispatcher_annotations.read_only_hint, Some(false));
        assert_eq!(dispatcher_annotations.destructive_hint, Some(true));
        assert_eq!(dispatcher_annotations.open_world_hint, Some(true));
        let source_manifest = bridge_tool(&bridge, MCP_LIST_SOURCES).description();
        assert!(source_manifest.contains("IDA_MCP_"));
        assert!(source_manifest.contains("IDA MCP!"));
        assert!(
            bridge
                .report
                .iter()
                .any(|line| line == "IDA MCP! -> catalog (71 private tool(s))")
        );

        let listed = call_bridge_tool(&bridge, MCP_LIST_SOURCES, json!({})).await;
        assert!(!listed.is_error);
        let listed = listed.structured_content.unwrap();
        assert_eq!(listed["total"], 1);
        assert_eq!(listed["sources"][0]["name"], "IDA MCP!");
        assert_eq!(listed["sources"][0]["provenance"], "codex-config");
        assert_eq!(listed["sources"][0]["transport"], "streamable-http");
        assert_eq!(listed["sources"][0]["toolCount"], 71);
        assert_eq!(
            listed["sources"][0]["implementation"]["name"],
            "catalog-test-upstream"
        );
        assert_eq!(
            listed["sources"][0]["implementation"]["title"],
            "IDA semantic analysis bridge"
        );
        assert!(
            listed["sources"][0]["instructions"]
                .as_str()
                .unwrap()
                .contains("program-analysis")
        );
        let source_id = listed["sources"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(source_id, "IDA_MCP_");
        let unresolved_identity = bridge_tool(&bridge, MCP_CALL_TOOL).call_identity(&json!({
            "source": source_id,
            "tool": "unknown-tool-id"
        }));
        assert_eq!(unresolved_identity.mcp_server.as_deref(), Some("IDA MCP!"));
        assert_eq!(unresolved_identity.mcp_tool, None);
        assert_eq!(
            unresolved_identity.resolved_tool(),
            "mcp:IDA MCP!/<unresolved>"
        );

        let filtered_sources = call_bridge_tool(
            &bridge,
            MCP_LIST_SOURCES,
            json!({ "query": "semantic analysis" }),
        )
        .await
        .structured_content
        .unwrap();
        assert_eq!(filtered_sources["total"], 1);
        let missing_sources = call_bridge_tool(
            &bridge,
            MCP_LIST_SOURCES,
            json!({ "query": "nonexistent system" }),
        )
        .await
        .structured_content
        .unwrap();
        assert_eq!(missing_sources["total"], 0);

        let searched = call_bridge_tool(
            &bridge,
            MCP_SEARCH_TOOLS,
            json!({
                "query": "decompile virtual address",
                "source": source_id,
                "limit": 5
            }),
        )
        .await;
        assert!(!searched.is_error);
        let searched = searched.structured_content.unwrap();
        assert_eq!(searched["matches"][0]["toolName"], "decompile_function");
        let decompile_id = searched["matches"][0]["toolId"]
            .as_str()
            .unwrap()
            .to_string();
        let identity = bridge_tool(&bridge, MCP_CALL_TOOL).call_identity(&json!({
            "source": source_id,
            "tool": decompile_id
        }));
        assert_eq!(identity.downstream_tool, MCP_CALL_TOOL);
        assert_eq!(identity.mcp_server.as_deref(), Some("IDA MCP!"));
        assert_eq!(identity.mcp_tool.as_deref(), Some("decompile_function"));
        assert_eq!(identity.resolved_tool(), "mcp:IDA MCP!/decompile_function");

        let definition = call_bridge_tool(
            &bridge,
            MCP_GET_TOOL,
            json!({ "source": source_id, "tool": decompile_id }),
        )
        .await;
        assert!(!definition.is_error);
        let definition = definition.structured_content.unwrap();
        assert_eq!(definition["tool"]["id"], "decompile_function");
        assert_eq!(definition["tool"]["name"], "decompile_function");
        assert_eq!(definition["tool"]["title"], "Decompile function");
        assert_eq!(
            definition["tool"]["inputSchema"]["properties"]["location"]["properties"]["virtual_address"]
                ["description"],
            "Function virtual address to decompile"
        );
        assert_eq!(definition["tool"]["annotations"]["readOnlyHint"], true);
        assert_eq!(
            definition["tool"]["icons"][0]["src"],
            "https://example.invalid/decompiler.svg"
        );
        assert_eq!(definition["tool"]["_meta"]["vendor"], "hex-rays");
        assert_eq!(
            definition["tool"]["outputSchema"]["properties"]["address"]["type"],
            "string"
        );

        let decompiled = call_bridge_tool(
            &bridge,
            MCP_CALL_TOOL,
            json!({
                "source": source_id,
                "tool": decompile_id,
                "arguments": {
                    "location": { "virtual_address": "0x81000000" }
                }
            }),
        )
        .await;
        assert!(!decompiled.is_error);
        assert_eq!(
            decompiled.structured_content,
            Some(json!({
                "rawTool": "decompile_function",
                "address": "0x81000000"
            }))
        );
        assert_eq!(
            calls.lock().unwrap().last().unwrap().name,
            "decompile_function"
        );

        let call_count = calls.lock().unwrap().len();
        let malformed = call_bridge_tool(
            &bridge,
            MCP_CALL_TOOL,
            json!({
                "source": source_id,
                "tool": decompile_id,
                "arguments": ["not", "an", "object"]
            }),
        )
        .await;
        assert!(malformed.is_error);
        assert!(malformed.joined_text().contains("must be a JSON object"));
        assert_eq!(calls.lock().unwrap().len(), call_count);

        let renamed = call_bridge_tool(
            &bridge,
            MCP_SEARCH_TOOLS,
            json!({ "query": "rename function", "source": source_id, "limit": 10 }),
        )
        .await
        .structured_content
        .unwrap();
        let renamed = renamed["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| {
                entry["toolName"] == "rename-function" || entry["toolName"] == "rename_function"
            })
            .map(|entry| {
                (
                    entry["toolId"].as_str().unwrap().to_string(),
                    entry["toolName"].as_str().unwrap().to_string(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(renamed.len(), 2);
        assert_ne!(renamed[0].0, renamed[1].0);
        for (tool_id, raw_name) in &renamed {
            let result = call_bridge_tool(
                &bridge,
                MCP_CALL_TOOL,
                json!({
                    "source": source_id,
                    "tool": tool_id,
                    "arguments": { "value": "new_name" }
                }),
            )
            .await;
            assert!(!result.is_error);
            assert_eq!(result.joined_text(), *raw_name);
            assert_eq!(calls.lock().unwrap().last().unwrap().name, *raw_name);
        }

        let passthrough = call_bridge_tool(
            &bridge,
            MCP_SEARCH_TOOLS,
            json!({ "query": "passthrough", "source": source_id, "limit": 1 }),
        )
        .await
        .structured_content
        .unwrap();
        let passthrough_id = passthrough["matches"][0]["toolId"].as_str().unwrap();
        let passthrough = call_bridge_tool(
            &bridge,
            MCP_CALL_TOOL,
            json!({ "source": source_id, "tool": passthrough_id, "arguments": {} }),
        )
        .await;
        assert!(!passthrough.is_error);
        assert_eq!(passthrough.content.len(), 2);
        assert!(matches!(
            &passthrough.content[0],
            ToolContent::Text(text) if text == "mixed text"
        ));
        assert!(matches!(
            &passthrough.content[1],
            ToolContent::Image { data, mime_type }
                if data == "aGVsbG8=" && mime_type == "image/png"
        ));
        assert_eq!(
            passthrough.structured_content,
            Some(json!({ "kind": "mixed", "ok": true }))
        );
        assert_eq!(
            passthrough
                .meta
                .as_ref()
                .and_then(|meta| meta.0.get("trace")),
            Some(&Value::String("fixture-result-meta".to_string()))
        );

        let failed = call_bridge_tool(
            &bridge,
            MCP_SEARCH_TOOLS,
            json!({ "query": "error_result", "source": source_id, "limit": 1 }),
        )
        .await
        .structured_content
        .unwrap();
        let failed_id = failed["matches"][0]["toolId"].as_str().unwrap();
        let failed = call_bridge_tool(
            &bridge,
            MCP_CALL_TOOL,
            json!({ "source": source_id, "tool": failed_id }),
        )
        .await;
        assert!(failed.is_error);
        assert_eq!(failed.joined_text(), "fixture tool error");

        drop(bridge);
        server_task.abort();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn catalog_mode_applies_allow_and_deny_filters_before_indexing() {
        let (url, _calls, server_task) = spawn_catalog_upstream().await;
        let mut config = crate::config::default_config(std::env::temp_dir());
        config.mcp_servers.insert(
            "filtered".to_string(),
            McpServerSpec {
                url: Some(url),
                provenance: McpServerProvenance::CodexCli,
                tools: Some(vec![
                    "decompile_function".to_string(),
                    "rename-function".to_string(),
                    "error_result".to_string(),
                ]),
                disabled_tools: Some(vec!["error_result".to_string()]),
                ..Default::default()
            },
        );

        let bridge = connect_upstreams(&config).await;
        assert_eq!(bridge.tools.len(), 4);
        let listed = call_bridge_tool(&bridge, MCP_LIST_SOURCES, json!({}))
            .await
            .structured_content
            .unwrap();
        assert_eq!(listed["sources"][0]["toolCount"], 2);
        assert_eq!(listed["sources"][0]["provenance"], "codex-cli");
        let source_id = listed["sources"][0]["id"].as_str().unwrap();
        let excluded = call_bridge_tool(
            &bridge,
            MCP_SEARCH_TOOLS,
            json!({ "query": "error_result", "source": source_id }),
        )
        .await
        .structured_content
        .unwrap();
        assert_eq!(excluded["total"], 0);
        let included = call_bridge_tool(
            &bridge,
            MCP_SEARCH_TOOLS,
            json!({ "query": "decompile", "source": source_id }),
        )
        .await
        .structured_content
        .unwrap();
        assert_eq!(included["matches"][0]["toolName"], "decompile_function");

        drop(bridge);
        server_task.abort();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn catalog_source_ids_disambiguate_sanitized_name_collisions() {
        let (first_url, first_calls, first_server_task) = spawn_catalog_upstream().await;
        let (second_url, second_calls, second_server_task) = spawn_catalog_upstream().await;
        let mut config = crate::config::default_config(std::env::temp_dir());
        config.mcp_servers.insert(
            "ida-mcp".to_string(),
            McpServerSpec {
                url: Some(first_url),
                mode: Some(McpToolExposure::Catalog),
                ..Default::default()
            },
        );
        config.mcp_servers.insert(
            "ida_mcp".to_string(),
            McpServerSpec {
                url: Some(second_url),
                mode: Some(McpToolExposure::Catalog),
                ..Default::default()
            },
        );

        let bridge = connect_upstreams(&config).await;
        assert_eq!(bridge.tools.len(), 4);
        let listed = call_bridge_tool(&bridge, MCP_LIST_SOURCES, json!({}))
            .await
            .structured_content
            .unwrap();
        assert_eq!(listed["total"], 2);
        let sources = listed["sources"].as_array().unwrap();
        let ids = sources
            .iter()
            .map(|source| source["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        let names = sources
            .iter()
            .map(|source| source["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["ida_mcp", "ida_mcp_2"]);
        assert_eq!(names, ["ida-mcp", "ida_mcp"]);

        let searched = call_bridge_tool(
            &bridge,
            MCP_SEARCH_TOOLS,
            json!({ "query": "decompile", "source": "ida_mcp_2", "limit": 1 }),
        )
        .await
        .structured_content
        .unwrap();
        let tool_id = searched["matches"][0]["toolId"].as_str().unwrap();
        let called = call_bridge_tool(
            &bridge,
            MCP_CALL_TOOL,
            json!({
                "source": "ida_mcp_2",
                "tool": tool_id,
                "arguments": { "location": { "virtual_address": "0x82000000" } }
            }),
        )
        .await;
        assert!(!called.is_error);
        assert!(first_calls.lock().unwrap().is_empty());
        assert_eq!(
            second_calls.lock().unwrap().last().unwrap().name,
            "decompile_function"
        );

        drop(bridge);
        first_server_task.abort();
        second_server_task.abort();
        let _ = first_server_task.await;
        let _ = second_server_task.await;
    }

    #[tokio::test]
    async fn explicit_direct_mode_retains_per_tool_exposure_and_metadata() {
        let (url, calls, server_task) = spawn_catalog_upstream().await;
        let mut config = crate::config::default_config(std::env::temp_dir());
        config.mcp_servers.insert(
            "IDA MCP!".to_string(),
            McpServerSpec {
                url: Some(url),
                mode: Some(McpToolExposure::Direct),
                ..Default::default()
            },
        );

        let bridge = connect_upstreams(&config).await;
        assert_eq!(bridge.tools.len(), 71);
        assert!(bridge.tools.iter().all(|tool| !matches!(
            tool.name(),
            MCP_LIST_SOURCES | MCP_SEARCH_TOOLS | MCP_GET_TOOL | MCP_CALL_TOOL
        )));
        let names = bridge
            .tools
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>();
        assert!(names.contains(&"IDA_MCP___rename_function"));
        assert!(names.contains(&"IDA_MCP___rename_function_2"));

        let decompile = bridge
            .tools
            .iter()
            .find(|tool| tool.name() == "IDA_MCP___decompile_function")
            .unwrap();
        assert_eq!(decompile.title(), "Decompile function");
        assert_eq!(
            decompile
                .annotations()
                .and_then(|annotations| annotations.title),
            Some("Decompiler action".to_string())
        );
        assert_eq!(
            decompile
                .annotations()
                .and_then(|annotations| annotations.read_only_hint),
            Some(true)
        );
        assert_eq!(decompile.icons().unwrap().len(), 1);
        assert_eq!(
            decompile
                .meta()
                .as_ref()
                .and_then(|meta| meta.0.get("vendor")),
            Some(&Value::String("hex-rays".to_string()))
        );
        assert!(decompile.output_schema().is_some());
        let identity = decompile.call_identity(&json!({
            "location": { "virtual_address": "0xDEADBEEF" }
        }));
        assert_eq!(identity.downstream_tool, "IDA_MCP___decompile_function");
        assert_eq!(identity.mcp_server.as_deref(), Some("IDA MCP!"));
        assert_eq!(identity.mcp_tool.as_deref(), Some("decompile_function"));

        let result = call_bridge_tool(
            &bridge,
            "IDA_MCP___decompile_function",
            json!({ "location": { "virtual_address": "0xDEADBEEF" } }),
        )
        .await;
        assert_eq!(
            result.structured_content,
            Some(json!({
                "rawTool": "decompile_function",
                "address": "0xDEADBEEF"
            }))
        );
        assert_eq!(
            calls.lock().unwrap().last().unwrap().name,
            "decompile_function"
        );

        drop(bridge);
        server_task.abort();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn explicit_gateway_mode_remains_a_single_compatible_dispatcher() {
        let (url, calls, server_task) = spawn_catalog_upstream().await;
        let generated = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(std::env::temp_dir());
        config.generated_skills_dir = Some(generated.path().to_path_buf());
        config.mcp_servers.insert(
            "IDA MCP!".to_string(),
            McpServerSpec {
                url: Some(url),
                mode: Some(McpToolExposure::Gateway),
                ..Default::default()
            },
        );

        let bridge = connect_upstreams(&config).await;
        assert_eq!(bridge.tools.len(), 1);
        assert_eq!(bridge.tools[0].name(), "IDA_MCP_");
        assert!(generated.path().join("IDA MCP!").join("SKILL.md").is_file());
        let identity = bridge_tool(&bridge, "IDA_MCP_").call_identity(&json!({
            "function": "rename-function",
            "arguments": { "value": "new_name" }
        }));
        assert_eq!(identity.downstream_tool, "IDA_MCP_");
        assert_eq!(identity.mcp_server.as_deref(), Some("IDA MCP!"));
        assert_eq!(identity.mcp_tool.as_deref(), Some("rename-function"));

        let result = call_bridge_tool(
            &bridge,
            "IDA_MCP_",
            json!({ "function": "rename-function", "arguments": { "value": "new_name" } }),
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(result.joined_text(), "rename-function");
        assert_eq!(
            calls.lock().unwrap().last().unwrap().name,
            "rename-function"
        );

        let malformed = call_bridge_tool(
            &bridge,
            "IDA_MCP_",
            json!({ "function": "rename-function", "arguments": "wrong" }),
        )
        .await;
        assert!(malformed.is_error);

        drop(bridge);
        server_task.abort();
        let _ = server_task.await;
    }

    #[test]
    fn infers_codex_transports_and_rejects_legacy_protocols() {
        let stdio = McpServerSpec {
            command: Some("server".to_string()),
            ..Default::default()
        };
        assert!(matches!(
            select_transport(&stdio).unwrap(),
            UpstreamTransport::Stdio("server")
        ));

        let http = McpServerSpec {
            url: Some("https://example.invalid/mcp".to_string()),
            ..Default::default()
        };
        assert!(matches!(
            select_transport(&http).unwrap(),
            UpstreamTransport::StreamableHttp("https://example.invalid/mcp")
        ));

        let legacy = McpServerSpec {
            transport: Some("sse".to_string()),
            url: Some("https://example.invalid/sse".to_string()),
            ..Default::default()
        };
        assert!(
            select_transport(&legacy)
                .unwrap_err()
                .contains("legacy SSE")
        );
    }

    #[test]
    fn rejects_transport_specific_fields_on_the_wrong_transport() {
        let stdio = McpServerSpec {
            command: Some("server".to_string()),
            bearer_token_env_var: Some("TOKEN".to_string()),
            ..Default::default()
        };
        assert!(
            select_transport(&stdio)
                .unwrap_err()
                .contains("stdio transport cannot configure bearerTokenEnvVar")
        );

        let http = McpServerSpec {
            url: Some("https://example.invalid/mcp".to_string()),
            args: vec!["--stdio".to_string()],
            ..Default::default()
        };
        assert!(
            select_transport(&http)
                .unwrap_err()
                .contains("Streamable HTTP transport cannot configure args")
        );
    }

    #[test]
    fn resolves_remote_credentials_without_exposing_values_in_errors() {
        let resolved = resolve_bearer_token("remote", Some("REMOTE_TOKEN"), &|name| {
            (name == "REMOTE_TOKEN").then(|| "secret-value".to_string())
        })
        .unwrap();
        assert_eq!(resolved.as_deref(), Some("secret-value"));

        let missing = resolve_bearer_token("remote", Some("MISSING_TOKEN"), &|_| None).unwrap_err();
        assert!(missing.contains("MISSING_TOKEN"));
        assert!(!missing.contains("secret-value"));

        let empty = resolve_bearer_token("remote", Some("EMPTY_TOKEN"), &|_| Some(String::new()))
            .unwrap_err();
        assert!(empty.contains("is empty"));
    }

    #[test]
    fn environment_headers_override_static_headers() {
        let spec = McpServerSpec {
            http_headers: HashMap::from([
                ("X-Static".to_string(), "static".to_string()),
                ("X-Override".to_string(), "old".to_string()),
            ]),
            env_http_headers: HashMap::from([
                ("X-Env".to_string(), "ENV_HEADER".to_string()),
                ("X-Override".to_string(), "OVERRIDE_HEADER".to_string()),
            ]),
            ..Default::default()
        };
        let headers = build_http_headers(&spec, &|name| match name {
            "ENV_HEADER" => Some("env".to_string()),
            "OVERRIDE_HEADER" => Some("new".to_string()),
            _ => None,
        });
        let value = |name| {
            headers
                .get(&HeaderName::from_static(name))
                .and_then(|value| value.to_str().ok())
        };
        assert_eq!(value("x-static"), Some("static"));
        assert_eq!(value("x-env"), Some("env"));
        assert_eq!(value("x-override"), Some("new"));
    }

    #[tokio::test]
    async fn rejects_duplicate_authorization_configuration_before_connecting() {
        let spec = McpServerSpec {
            url: Some("https://example.invalid/mcp".to_string()),
            bearer_token_env_var: Some("REMOTE_TOKEN".to_string()),
            env_http_headers: HashMap::from([(
                "Authorization".to_string(),
                "MISSING_AUTH_HEADER".to_string(),
            )]),
            ..Default::default()
        };
        let config = crate::config::default_config(std::env::temp_dir());
        let result = connect_one_with_env("remote", &spec, &config, &|name| {
            (name == "REMOTE_TOKEN").then(|| "secret-value".to_string())
        })
        .await;
        let error = match result {
            Ok(_) => panic!("ambiguous authorization should fail before connecting"),
            Err(error) => error,
        };
        assert!(error.contains("either bearerTokenEnvVar or an Authorization HTTP header"));
        assert!(!error.contains("secret-value"));
    }

    #[tokio::test]
    async fn bridges_authenticated_streamable_http_and_applies_tool_timeout() {
        let (url, server_task) = spawn_http_upstream().await;
        let spec = McpServerSpec {
            transport: Some("streamable_http".to_string()),
            url: Some(url),
            bearer_token_env_var: Some("REMOTE_TOKEN".to_string()),
            http_headers: HashMap::from([("X-Static".to_string(), "static-value".to_string())]),
            env_http_headers: HashMap::from([("X-Env".to_string(), "REMOTE_HEADER".to_string())]),
            startup_timeout_sec: Some(5.0),
            tool_timeout_sec: Some(0.25),
            ..Default::default()
        };
        let config = crate::config::default_config(std::env::temp_dir());
        let (service, tools, tool_timeout) =
            connect_one_with_env("remote", &spec, &config, &|name| match name {
                "REMOTE_TOKEN" => Some("remote-token".to_string()),
                "REMOTE_HEADER" => Some("env-value".to_string()),
                _ => None,
            })
            .await
            .unwrap();
        assert_eq!(tools.len(), 2);
        let resources = BridgedResourceStore::new(config.artifact_egress.clone());

        let echo = forward_tool_call(
            service.peer(),
            CallToolRequestParams::new("echo")
                .with_arguments(json!({ "text": "hello" }).as_object().cloned().unwrap()),
            "remote",
            "echo",
            tool_timeout,
            None,
            &resources,
        )
        .await;
        assert!(!echo.is_error);
        assert_eq!(echo.joined_text(), "hello");

        let slow = forward_tool_call(
            service.peer(),
            CallToolRequestParams::new("slow")
                .with_arguments(json!({ "text": "late" }).as_object().cloned().unwrap()),
            "remote",
            "slow",
            tool_timeout,
            None,
            &resources,
        )
        .await;
        assert!(slow.is_error);
        assert!(slow.joined_text().contains("timed out after"));

        let cancellation = CancellationToken::new();
        let cancel_task = {
            let cancellation = cancellation.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                cancellation.cancel();
            })
        };
        let started = std::time::Instant::now();
        let cancelled = forward_tool_call(
            service.peer(),
            CallToolRequestParams::new("slow")
                .with_arguments(json!({ "text": "cancelled" }).as_object().cloned().unwrap()),
            "remote",
            "slow",
            Some(Duration::from_secs(5)),
            Some(&cancellation),
            &resources,
        )
        .await;
        cancel_task.await.unwrap();
        assert!(cancelled.is_error);
        assert!(
            cancelled
                .joined_text()
                .contains("cancelled by downstream client")
        );
        assert!(started.elapsed() < Duration::from_millis(150));

        drop(service);
        server_task.abort();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn skips_upstream_that_fails_to_launch() {
        let mut config = crate::config::default_config(std::env::temp_dir());
        config.mcp_servers.insert(
            "bad".into(),
            McpServerSpec {
                command: Some("codexify-nonexistent-binary-xyz".into()),
                ..Default::default()
            },
        );
        let bridge = connect_upstreams(&config).await;
        assert!(
            bridge.tools.is_empty(),
            "a failing upstream must be skipped, not fatal"
        );
        assert!(bridge.services.is_empty());
        // The failure is surfaced in the report, not swallowed.
        assert!(bridge.report.iter().any(|l| l.contains("bad -> FAILED")));
    }

    #[tokio::test]
    async fn reports_unsupported_url_transport() {
        let mut config = crate::config::default_config(std::env::temp_dir());
        config.mcp_servers.insert(
            "remote".into(),
            McpServerSpec {
                transport: Some("sse".into()),
                url: Some("http://localhost:9/sse".into()),
                ..Default::default()
            },
        );
        let bridge = connect_upstreams(&config).await;
        assert!(bridge.tools.is_empty());
        assert!(
            bridge.report.iter().any(|l| l.contains("not supported")),
            "sse transport should be reported as unsupported: {:?}",
            bridge.report
        );
    }

    #[tokio::test]
    async fn skips_disabled_upstream() {
        let mut config = crate::config::default_config(std::env::temp_dir());
        config.mcp_servers.insert(
            "off".into(),
            McpServerSpec {
                command: Some("codexify-should-never-run".into()),
                disabled: true,
                ..Default::default()
            },
        );
        let bridge = connect_upstreams(&config).await;
        assert!(bridge.tools.is_empty());
    }

    #[test]
    fn deny_list_is_applied_after_allow_list() {
        let spec = McpServerSpec {
            tools: Some(vec!["read".into(), "write".into()]),
            disabled_tools: Some(vec!["write".into()]),
            ..Default::default()
        };
        assert!(tool_is_enabled(&spec, "read"));
        assert!(!tool_is_enabled(&spec, "write"));
        assert!(!tool_is_enabled(&spec, "other"));
    }

    #[tokio::test]
    async fn upstream_client_does_not_follow_redirects() {
        use axum::http::{StatusCode, header::LOCATION};
        use axum::routing::get;

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route(
                "/redirect",
                get(|| async { (StatusCode::FOUND, [(LOCATION, "/target")]) }),
            )
            .route("/target", get(|| async { "arrived" }));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = build_upstream_client().unwrap();
        let response = client
            .get(format!("http://{address}/redirect"))
            .send()
            .await
            .unwrap();

        // The client must surface the 3xx itself rather than transparently
        // following it — otherwise a redirecting upstream could have our
        // caller-supplied Authorization and custom headers replayed to an
        // arbitrary target.
        assert_eq!(response.status().as_u16(), 302);
        assert_eq!(
            response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok()),
            Some("/target")
        );
        task.abort();
    }
}
