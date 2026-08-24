//! Bridge to upstream MCP servers.
//!
//! codexify can act as an MCP *client* to other MCP servers, discover their
//! tools at startup, and re-expose them through its own
//! `tools/list` / `tools/call` so the ChatGPT-side agent can use them too. Each
//! upstream tool is offered under a `<server>__<tool>` name and calls are
//! forwarded to the upstream verbatim.
//!
//! Local servers use stdio; remote servers use MCP Streamable HTTP.

use std::{collections::HashMap, time::Duration};

use async_trait::async_trait;
use http::{HeaderName, HeaderValue, header::AUTHORIZATION};
use rmcp::{
    RoleClient, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, ContentBlock,
        ServerResult,
    },
    service::{Peer, PeerRequestOptions, RunningService, ServiceError},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::exec_sessions::SessionState;
use crate::process_env::scrub_untrusted_child_env;
use crate::tool::Tool;
use crate::types::{AppConfig, ToolContent, ToolResult};

/// How long to wait for an upstream to start up and answer `tools/list` before
/// giving up on it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// The result of connecting to every configured upstream: the tools to merge
/// into the registry, plus the running services kept alive for the server's
/// lifetime (dropping one closes its transport and any child process).
pub struct Bridge {
    pub tools: Vec<Box<dyn Tool>>,
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
    description: String,
    input_schema: Value,
    output_schema: Option<Value>,
    peer: Peer<RoleClient>,
    tool_timeout: Option<Duration>,
}

#[async_trait]
impl Tool for BridgedTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn output_schema(&self) -> Option<Value> {
        self.output_schema.clone()
    }

    /// Bridged results are passed through verbatim; never synthesise a default
    /// structured result that would not match the upstream's own schema.
    fn fills_structured_content(&self) -> bool {
        false
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
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
        )
        .await
    }
}

async fn forward_tool_call(
    peer: &Peer<RoleClient>,
    params: CallToolRequestParams,
    server: &str,
    tool: &str,
    tool_timeout: Option<Duration>,
) -> ToolResult {
    let result = if let Some(limit) = tool_timeout {
        call_tool_with_timeout(peer, params, limit).await
    } else {
        peer.call_tool(params)
            .await
            .map_err(|error| error.to_string())
    };

    match result {
        Ok(result) => map_call_result(result),
        Err(error) => ToolResult::error(format!(
            "Upstream MCP server '{server}' failed to run '{tool}': {error}"
        )),
    }
}

async fn call_tool_with_timeout(
    peer: &Peer<RoleClient>,
    params: CallToolRequestParams,
    timeout: Duration,
) -> Result<CallToolResult, String> {
    let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
    let handle = peer
        .send_cancellable_request(request, PeerRequestOptions::with_timeout(timeout))
        .await
        .map_err(|error| error.to_string())?;
    let response = match handle.await_response().await {
        Ok(response) => response,
        Err(ServiceError::Timeout { .. }) => {
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
/// preserving text, images, error flag and structured content. Content blocks
/// this server has no native representation for are rendered as JSON text so
/// nothing is silently dropped.
fn map_call_result(result: CallToolResult) -> ToolResult {
    let content: Vec<ToolContent> = result
        .content
        .into_iter()
        .map(|block| match block {
            ContentBlock::Text(t) => ToolContent::Text(t.text),
            ContentBlock::Image(i) => ToolContent::Image {
                data: i.data,
                mime_type: i.mime_type,
            },
            other => ToolContent::Text(
                serde_json::to_string(&other).unwrap_or_else(|_| "[unrenderable content]".into()),
            ),
        })
        .collect();

    ToolResult {
        content,
        is_error: result.is_error.unwrap_or(false),
        structured_content: result.structured_content,
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
/// build the bridged tool proxies. A server that fails to launch or answer is
/// logged and skipped — it never blocks startup.
pub async fn connect_upstreams(config: &AppConfig) -> Bridge {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    let mut services: Vec<RunningService<RoleClient, ()>> = Vec::new();
    let mut report: Vec<String> = Vec::new();

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

        let is_gateway = spec.mode.as_deref() == Some("gateway");

        match connect_one(server_name, spec, config).await {
            Ok((service, upstream_tools, tool_timeout)) => {
                let peer = service.peer().clone();
                let count = upstream_tools.len();

                if is_gateway {
                    // Collapse the whole server into one dispatcher tool + a
                    // generated skill documenting every function.
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
                    }));
                    report.push(format!(
                        "{server_name} -> gateway ({count} functions via `{sanitized}`)"
                    ));
                    tracing::info!(
                        "bridged MCP server '{server_name}' as gateway: {count} function(s)"
                    );
                } else {
                    report.push(format!("{server_name} -> {count} tool(s)"));
                    // Ensure distinct downstream names: sanitising or 64-byte
                    // truncation can map two upstream tools to the same name.
                    let mut used: std::collections::HashSet<String> =
                        std::collections::HashSet::new();
                    for tool in upstream_tools {
                        let original_name = tool.name.to_string();
                        let display = unique_name(bridged_name(&sanitized, &original_name), &used);
                        used.insert(display.clone());
                        let leaked: &'static str = Box::leak(display.into_boxed_str());
                        tools.push(Box::new(BridgedTool {
                            name: leaked,
                            original_name,
                            server: server_name.clone(),
                            description: tool
                                .description
                                .map(|c| c.to_string())
                                .unwrap_or_default(),
                            input_schema: Value::Object((*tool.input_schema).clone()),
                            output_schema: tool.output_schema.map(|s| Value::Object((*s).clone())),
                            peer: peer.clone(),
                            tool_timeout,
                        }));
                    }
                    tracing::info!("bridged MCP server '{server_name}': {count} tool(s)");
                }
                services.push(service);
            }
            Err(e) => {
                report.push(format!("{server_name} -> FAILED: {e}"));
                tracing::warn!("skipping MCP server '{server_name}': {e}");
            }
        }
    }

    Bridge {
        tools,
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
}

#[async_trait]
impl Tool for GatewayTool {
    fn name(&self) -> &'static str {
        self.name
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

    fn fills_structured_content(&self) -> bool {
        false
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
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
        // `arguments` is optional, but a present-but-malformed value must be
        // rejected rather than silently dropped (which would call the function
        // with no arguments).
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
        )
        .await
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

fn ensure_rustls_crypto_provider() {
    // RMCP avoids bundling AWS-LC; install the ring provider already used by Codexify.
    let _ = rustls::crypto::ring::default_provider().install_default();
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
            ensure_rustls_crypto_provider();
            let headers = build_http_headers(spec, env_lookup);

            let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url);
            if let Some(token) = bearer_token {
                transport_config = transport_config.auth_header(token);
            }
            transport_config = transport_config.custom_headers(headers);
            let transport = StreamableHttpClientTransport::from_config(transport_config);
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
    use crate::types::McpServerSpec;
    use axum::{
        Router,
        extract::{Request, State},
        middleware::Next,
        response::{IntoResponse, Response},
    };
    use rmcp::{
        ErrorData as McpError, ServerHandler,
        model::{
            CallToolResponse, Implementation, InitializeResult, ListToolsResult,
            PaginatedRequestParams, ServerCapabilities, ServerInfo,
        },
        service::{RequestContext, RoleServer},
        transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    };
    use std::sync::Arc;

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
                tokio::time::sleep(Duration::from_millis(200)).await;
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
        let r = map_call_result(CallToolResult::success(vec![ContentBlock::text("hi")]));
        assert!(!r.is_error);
        assert_eq!(r.joined_text(), "hi");

        let s = map_call_result(CallToolResult::structured(serde_json::json!({ "k": "v" })));
        assert_eq!(s.structured_content, Some(serde_json::json!({ "k": "v" })));

        let e = map_call_result(CallToolResult::error(vec![ContentBlock::text("boom")]));
        assert!(e.is_error);
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
            tool_timeout_sec: Some(0.05),
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

        let echo = forward_tool_call(
            service.peer(),
            CallToolRequestParams::new("echo")
                .with_arguments(json!({ "text": "hello" }).as_object().cloned().unwrap()),
            "remote",
            "echo",
            tool_timeout,
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
        )
        .await;
        assert!(slow.is_error);
        assert!(slow.joined_text().contains("timed out after"));

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
}
