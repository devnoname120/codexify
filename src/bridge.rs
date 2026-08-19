//! Bridge to upstream MCP servers.
//!
//! codexify can act as an MCP *client* to other local MCP servers (e.g. `idasql`),
//! discover their tools at startup, and re-expose them through its own
//! `tools/list` / `tools/call` so the ChatGPT-side agent can use them too. Each
//! upstream tool is offered under a `<server>__<tool>` name and calls are
//! forwarded to the upstream verbatim.
//!
//! Only stdio (command-launched) upstreams are supported: the server is spawned
//! as a child process and driven over its stdin/stdout, which is how most local
//! MCP servers run.

use std::time::Duration;

use async_trait::async_trait;
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ContentBlock},
    service::{Peer, RunningService},
    transport::TokioChildProcess,
};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::exec_sessions::SessionState;
use crate::tool::Tool;
use crate::types::{AppConfig, ToolContent, ToolResult};

/// How long to wait for an upstream to start up and answer `tools/list` before
/// giving up on it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// The result of connecting to every configured upstream: the tools to merge
/// into the registry, plus the running services kept alive for the server's
/// lifetime (dropping a service tears down its child process).
pub struct Bridge {
    pub tools: Vec<Box<dyn Tool>>,
    /// Held only to keep the upstream connections (and their child processes)
    /// alive; never read directly.
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

    async fn call(&self, args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        let mut params = CallToolRequestParams::new(self.original_name.clone());
        if let Some(obj) = args.as_object()
            && !obj.is_empty()
        {
            params = params.with_arguments(obj.clone());
        }

        match self.peer.call_tool(params).await {
            Ok(result) => map_call_result(result),
            Err(e) => ToolResult::error(format!(
                "Upstream MCP server '{}' failed to run '{}': {e}",
                self.server, self.original_name
            )),
        }
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

        match connect_one(server_name, spec).await {
            Ok((service, upstream_tools)) => {
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

        match self.peer.call_tool(params).await {
            Ok(result) => map_call_result(result),
            Err(e) => ToolResult::error(format!(
                "Upstream MCP server '{}' failed to run '{function}': {e}",
                self.server
            )),
        }
    }
}

/// Launch and initialise one upstream, returning its running service and tools.
async fn connect_one(
    server_name: &str,
    spec: &crate::types::McpServerSpec,
) -> Result<(RunningService<RoleClient, ()>, Vec<rmcp::model::Tool>), String> {
    // Recognise url-based transports and report them clearly instead of failing
    // to launch a nonexistent command.
    let kind = spec
        .transport
        .as_deref()
        .unwrap_or("stdio")
        .to_ascii_lowercase();
    if matches!(
        kind.as_str(),
        "sse" | "http" | "streamable-http" | "websocket" | "ws"
    ) {
        return Err(format!(
            "type \"{kind}\" (url transport) is not supported yet; only stdio (command) servers are bridged"
        ));
    }
    if spec.command.is_none() && spec.url.is_some() {
        return Err(
            "url-based (sse/http) servers are not supported yet; only stdio (command) servers are bridged"
                .to_string(),
        );
    }
    let Some(command_path) = spec.command.as_deref().filter(|c| !c.is_empty()) else {
        return Err("no \"command\" specified (a stdio server needs a command path)".to_string());
    };

    let mut command = Command::new(command_path);
    command.args(&spec.args);
    for (key, value) in &spec.env {
        command.env(key, value);
    }

    let transport = TokioChildProcess::new(command)
        .map_err(|e| format!("could not launch '{command_path}': {e}"))?;

    let connect = async {
        let service = ().serve(transport).await.map_err(|e| e.to_string())?;
        let tools = service.list_all_tools().await.map_err(|e| e.to_string())?;
        Ok::<_, String>((service, tools))
    };

    match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok((service, mut tools))) => {
            // Apply the optional per-server allow-list on the upstream's own names.
            if let Some(allow) = &spec.tools {
                let set: std::collections::HashSet<&str> =
                    allow.iter().map(|s| s.as_str()).collect();
                tools.retain(|t| set.contains(t.name.as_ref()));
            }
            Ok((service, tools))
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!(
            "timed out after {}s waiting for '{server_name}' to initialise",
            CONNECT_TIMEOUT.as_secs()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::McpServerSpec;
    use rmcp::model::CallToolResult;
    use std::collections::HashMap;

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

    #[tokio::test]
    async fn skips_upstream_that_fails_to_launch() {
        let mut config = crate::config::default_config(std::env::temp_dir());
        config.mcp_servers.insert(
            "bad".into(),
            McpServerSpec {
                command: Some("codexify-nonexistent-binary-xyz".into()),
                args: vec![],
                env: HashMap::new(),
                disabled: false,
                transport: None,
                url: None,
                tools: None,
                mode: None,
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
                command: None,
                args: vec![],
                env: HashMap::new(),
                disabled: false,
                transport: Some("sse".into()),
                url: Some("http://localhost:9/sse".into()),
                tools: None,
                mode: None,
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
                args: vec![],
                env: HashMap::new(),
                disabled: true,
                transport: None,
                url: None,
                tools: None,
                mode: None,
            },
        );
        let bridge = connect_upstreams(&config).await;
        assert!(bridge.tools.is_empty());
    }
}
