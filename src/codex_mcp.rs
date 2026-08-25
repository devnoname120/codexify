//! Import user-level MCP servers from Codex's `config.toml`, with optional CLI
//! enrichment for plugin-provided entries in Codex's effective catalogue.

use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::codex_config::load_codex_config;
#[cfg(test)]
use crate::codex_config::parse_codex_config;
use crate::types::McpServerSpec;

const CODEX_CLI_TIMEOUT: Duration = Duration::from_secs(30);
const CODEX_CLI_MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct CodexMcpImport {
    pub servers: HashMap<String, McpServerSpec>,
    pub report: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CodexCliListEntry {
    name: String,
}

#[derive(Debug, Deserialize)]
struct CodexCliServer {
    name: String,
    #[serde(default = "default_true")]
    enabled: bool,
    transport: CodexCliTransport,
    #[serde(default)]
    enabled_tools: Option<Vec<String>>,
    #[serde(default)]
    disabled_tools: Option<Vec<String>>,
    #[serde(default)]
    startup_timeout_sec: Option<f64>,
    #[serde(default)]
    tool_timeout_sec: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct CodexCliTransport {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    env: Option<HashMap<String, String>>,
    #[serde(default)]
    env_vars: Option<Vec<CodexCliEnvVar>>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    bearer_token: Option<String>,
    #[serde(default)]
    bearer_token_env_var: Option<String>,
    #[serde(default)]
    http_headers: Option<HashMap<String, String>>,
    #[serde(default)]
    env_http_headers: Option<HashMap<String, String>>,
    #[serde(default)]
    http_headers_helper: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CodexCliEnvVar {
    Name(String),
    Config {
        name: String,
        #[serde(default)]
        source: Option<String>,
    },
}

fn default_true() -> bool {
    true
}

fn run_codex_command(
    command: &OsStr,
    cwd: Option<&Path>,
    args: &[OsString],
) -> Result<Vec<u8>, String> {
    let mut process = Command::new(command);
    process
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(cwd) = cwd.filter(|path| path.is_dir()) {
        process.current_dir(cwd);
    }

    let mut child = process.spawn().map_err(|error| {
        let executable = Path::new(command).display();
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("Codex CLI executable `{executable}` was not found")
        } else {
            format!("failed to start Codex CLI executable `{executable}`: {error}")
        }
    })?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture Codex CLI stdout".to_string())?;
    let (stdout_tx, stdout_rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut output = Vec::new();
        let mut chunk = [0_u8; 8192];
        let mut exceeded = false;
        let result = (|| {
            loop {
                let read = stdout.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                let remaining = CODEX_CLI_MAX_OUTPUT_BYTES.saturating_sub(output.len());
                if remaining > 0 {
                    output.extend_from_slice(&chunk[..read.min(remaining)]);
                }
                exceeded |= read > remaining;
            }
            Ok::<_, std::io::Error>((output, exceeded))
        })();
        let _ = stdout_tx.send(result);
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < CODEX_CLI_TIMEOUT => {
                thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "Codex CLI timed out after {} seconds",
                    CODEX_CLI_TIMEOUT.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("failed while waiting for Codex CLI: {error}"));
            }
        }
    };
    let remaining = CODEX_CLI_TIMEOUT.saturating_sub(started.elapsed());
    let stdout_result = stdout_rx
        .recv_timeout(remaining)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => {
                "Codex CLI stdout did not close before the discovery timeout".to_string()
            }
            mpsc::RecvTimeoutError::Disconnected => {
                "Codex CLI stdout reader stopped unexpectedly".to_string()
            }
        })?;
    let (stdout, output_exceeded) =
        stdout_result.map_err(|error| format!("failed to read Codex CLI stdout: {error}"))?;
    if output_exceeded {
        return Err(format!(
            "Codex CLI output exceeded {} bytes",
            CODEX_CLI_MAX_OUTPUT_BYTES
        ));
    }

    if !status.success() {
        let operation = args
            .iter()
            .take(2)
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        return Err(format!(
            "Codex CLI exited with status {} while running `{operation}`",
            status
        ));
    }
    Ok(stdout)
}

pub fn discover_additional_codex_mcp_servers_with_cli(
    command: &OsStr,
    cwd: Option<&Path>,
    existing: &HashMap<String, McpServerSpec>,
) -> Result<CodexMcpImport, String> {
    let mut runner = |args: &[OsString]| run_codex_command(command, cwd, args);
    discover_additional_codex_mcp_servers_with_runner(
        existing,
        &|name| std::env::var(name).ok(),
        &mut runner,
    )
}

fn discover_additional_codex_mcp_servers_with_runner<F>(
    existing: &HashMap<String, McpServerSpec>,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    runner: &mut F,
) -> Result<CodexMcpImport, String>
where
    F: FnMut(&[OsString]) -> Result<Vec<u8>, String>,
{
    let list_args = ["mcp", "list", "--json"].map(OsString::from);
    let list_output = runner(&list_args)?;
    let mut entries: Vec<CodexCliListEntry> =
        serde_json::from_slice(&list_output).map_err(|error| {
            format!("Codex CLI returned invalid JSON for `mcp list --json`: {error}")
        })?;
    entries.sort_by(|left, right| left.name.cmp(&right.name));

    let mut outcome = CodexMcpImport::default();
    let mut seen = BTreeSet::new();
    for entry in entries {
        let name = entry.name;
        if name.trim().is_empty() {
            outcome
                .report
                .push("<unnamed> -> skipped: Codex CLI returned an empty server name".to_string());
            continue;
        }
        if !seen.insert(name.clone()) {
            outcome.report.push(format!(
                "{name} -> skipped: Codex CLI returned the name more than once"
            ));
            continue;
        }
        if existing.contains_key(&name) {
            continue;
        }

        let get_args = [
            OsString::from("mcp"),
            OsString::from("get"),
            OsString::from("--json"),
            OsString::from("--"),
            OsString::from(&name),
        ];
        let get_output = match runner(&get_args) {
            Ok(output) => output,
            Err(error) => {
                outcome.report.push(format!(
                    "{name} -> skipped: Codex CLI could not return the full server configuration ({error})"
                ));
                continue;
            }
        };
        let server: CodexCliServer = match serde_json::from_slice(&get_output) {
            Ok(server) => server,
            Err(error) => {
                outcome.report.push(format!(
                    "{name} -> skipped: Codex CLI returned invalid JSON for `mcp get` ({error})"
                ));
                continue;
            }
        };
        if server.name != name {
            outcome.report.push(format!(
                "{name} -> skipped: Codex CLI returned configuration for a different server"
            ));
            continue;
        }

        match codex_cli_server_to_spec(server, env_lookup) {
            Ok(spec) => {
                let suffix = if spec.disabled { " (disabled)" } else { "" };
                outcome.servers.insert(name.clone(), spec);
                outcome.report.push(format!(
                    "{name} -> imported from Codex CLI{suffix} (not present in config.toml)"
                ));
            }
            Err(reason) => outcome.report.push(format!("{name} -> skipped: {reason}")),
        }
    }

    Ok(outcome)
}

fn codex_cli_server_to_spec(
    server: CodexCliServer,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<McpServerSpec, String> {
    let kind = server.transport.kind.to_ascii_lowercase();
    let startup_timeout_sec =
        validate_cli_timeout("startup_timeout_sec", server.startup_timeout_sec)?;
    let tool_timeout_sec = validate_cli_timeout("tool_timeout_sec", server.tool_timeout_sec)?;

    if matches!(
        kind.as_str(),
        "http" | "streamable-http" | "streamable_http"
    ) {
        let Some(url) = server.transport.url.filter(|url| !url.trim().is_empty()) else {
            return Err("no non-empty Streamable HTTP `url` is configured".to_string());
        };
        if server.transport.command.is_some()
            || server.transport.args.is_some()
            || server.transport.env.is_some()
            || server.transport.env_vars.is_some()
            || server.transport.cwd.is_some()
        {
            return Err("streamable HTTP transport contains stdio-only fields".to_string());
        }
        if server.transport.bearer_token.is_some() {
            return Err(
                "literal bearer tokens are rejected; use `bearer_token_env_var`".to_string(),
            );
        }
        if server.transport.http_headers_helper.is_some() {
            return Err("`http_headers_helper` is not supported".to_string());
        }

        return Ok(McpServerSpec {
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            disabled: !server.enabled,
            transport: Some("streamable-http".to_string()),
            url: Some(url),
            bearer_token_env_var: server.transport.bearer_token_env_var,
            http_headers: server.transport.http_headers.unwrap_or_default(),
            env_http_headers: server.transport.env_http_headers.unwrap_or_default(),
            startup_timeout_sec,
            tool_timeout_sec,
            tools: server.enabled_tools,
            disabled_tools: server.disabled_tools,
            mode: None,
        });
    }
    if matches!(kind.as_str(), "sse" | "websocket" | "ws") {
        return Err(format!("unsupported transport type `{kind}`"));
    }
    if kind != "stdio" {
        return Err(format!("unsupported transport type `{kind}`"));
    }
    if server.transport.url.is_some()
        || server.transport.bearer_token.is_some()
        || server.transport.bearer_token_env_var.is_some()
        || server.transport.http_headers.is_some()
        || server.transport.env_http_headers.is_some()
        || server.transport.http_headers_helper.is_some()
    {
        return Err("stdio transport contains HTTP-only fields".to_string());
    }

    let Some(command) = server
        .transport
        .command
        .filter(|command| !command.trim().is_empty())
    else {
        return Err("no non-empty stdio `command` is configured".to_string());
    };

    let mut env = server.transport.env.unwrap_or_default();
    for env_var in server.transport.env_vars.unwrap_or_default() {
        let (name, source) = match env_var {
            CodexCliEnvVar::Name(name) => (name, None),
            CodexCliEnvVar::Config { name, source } => (name, source),
        };
        match source.as_deref() {
            None | Some("local") => {
                if !env.contains_key(&name)
                    && let Some(value) = env_lookup(&name)
                {
                    env.insert(name, value);
                }
            }
            Some("remote") => {
                return Err("remote-sourced `env_vars` are not supported".to_string());
            }
            Some(_) => {
                return Err("`env_vars.source` must be `local` or `remote`".to_string());
            }
        }
    }

    Ok(McpServerSpec {
        command: Some(command),
        args: server.transport.args.unwrap_or_default(),
        env,
        cwd: server.transport.cwd,
        disabled: !server.enabled,
        transport: None,
        url: None,
        bearer_token_env_var: None,
        http_headers: HashMap::new(),
        env_http_headers: HashMap::new(),
        startup_timeout_sec,
        tool_timeout_sec,
        tools: server.enabled_tools,
        disabled_tools: server.disabled_tools,
        mode: None,
    })
}

fn validate_cli_timeout(key: &str, timeout: Option<f64>) -> Result<Option<f64>, String> {
    if timeout.is_some_and(|value| !value.is_finite() || value < 0.0) {
        return Err(format!("`{key}` must be a non-negative finite number"));
    }
    Ok(timeout)
}

pub fn discover_codex_mcp_servers(path: &Path) -> Result<Option<CodexMcpImport>, String> {
    let Some(root) = load_codex_config(path)? else {
        return Ok(None);
    };
    parse_codex_mcp_table(&root, &|name| std::env::var(name).ok()).map(Some)
}

#[cfg(test)]
fn parse_codex_mcp_servers(
    contents: &str,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<CodexMcpImport, String> {
    let root = parse_codex_config(contents)?;
    parse_codex_mcp_table(&root, env_lookup)
}

fn parse_codex_mcp_table(
    root: &toml::Table,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<CodexMcpImport, String> {
    let Some(value) = root.get("mcp_servers") else {
        return Ok(CodexMcpImport::default());
    };
    let Some(servers) = value.as_table() else {
        return Err("Codex `mcp_servers` must be a TOML table".to_string());
    };

    let mut names: Vec<&String> = servers.keys().collect();
    names.sort();

    let mut outcome = CodexMcpImport::default();
    for name in names {
        let value = &servers[name];
        let Some(table) = value.as_table() else {
            outcome
                .report
                .push(format!("{name} -> skipped: server entry must be a table"));
            continue;
        };

        match parse_server(table, env_lookup) {
            Ok(ServerImport::Imported {
                spec,
                ignored_fields,
            }) => {
                let mut line = if spec.disabled {
                    format!("{name} -> imported from Codex config (disabled)")
                } else {
                    format!("{name} -> imported from Codex config")
                };
                if !ignored_fields.is_empty() {
                    line.push_str("; unsupported fields ignored: ");
                    line.push_str(&ignored_fields.join(", "));
                }
                outcome.servers.insert(name.clone(), *spec);
                outcome.report.push(line);
            }
            Ok(ServerImport::Skipped(reason)) | Err(reason) => {
                outcome.report.push(format!("{name} -> skipped: {reason}"));
            }
        }
    }

    Ok(outcome)
}

enum ServerImport {
    Imported {
        spec: Box<McpServerSpec>,
        ignored_fields: Vec<String>,
    },
    Skipped(String),
}

fn parse_server(
    table: &toml::Table,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<ServerImport, String> {
    let command = optional_string(table, "command")?;
    let url = optional_string(table, "url")?;
    if command.is_some() && url.is_some() {
        return Ok(ServerImport::Skipped(
            "both `command` and `url` are configured".to_string(),
        ));
    }

    let environment_id = optional_string(table, "environment_id")?;
    let experimental_environment = optional_string(table, "experimental_environment")?;
    if environment_id
        .as_deref()
        .is_some_and(|value| value != "local")
        || experimental_environment.as_deref() == Some("remote")
    {
        return Ok(ServerImport::Skipped(
            "non-local execution environments are not supported".to_string(),
        ));
    }

    let supported_fields = [
        "args",
        "bearer_token",
        "bearer_token_env_var",
        "command",
        "cwd",
        "disabled_tools",
        "enabled",
        "enabled_tools",
        "env",
        "env_http_headers",
        "env_vars",
        "environment_id",
        "experimental_environment",
        "http_headers",
        "http_headers_helper",
        "startup_timeout_ms",
        "startup_timeout_sec",
        "tool_timeout_sec",
        "url",
    ];
    let supported: BTreeSet<&str> = supported_fields.into_iter().collect();
    let mut ignored_fields = table
        .keys()
        .filter(|key| !supported.contains(key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    ignored_fields.sort();

    let startup_timeout_seconds = optional_timeout_seconds(table, "startup_timeout_sec")?;
    let startup_timeout_milliseconds = optional_timeout_milliseconds(table, "startup_timeout_ms")?;
    let startup_timeout_sec = startup_timeout_seconds.or(startup_timeout_milliseconds);
    let tool_timeout_sec = optional_timeout_seconds(table, "tool_timeout_sec")?;
    let disabled = !optional_bool(table, "enabled")?.unwrap_or(true);
    let tools = optional_string_list(table, "enabled_tools")?;
    let disabled_tools = optional_string_list(table, "disabled_tools")?;

    if let Some(command) = command.filter(|value| !value.trim().is_empty()) {
        for field in [
            "bearer_token",
            "bearer_token_env_var",
            "env_http_headers",
            "http_headers",
            "http_headers_helper",
        ] {
            if table.contains_key(field) {
                return Ok(ServerImport::Skipped(format!(
                    "stdio transport cannot configure `{field}`"
                )));
            }
        }

        let mut env = optional_string_map(table, "env")?.unwrap_or_default();
        for env_var in optional_env_vars(table)?.unwrap_or_default() {
            match env_var.source.as_deref() {
                None | Some("local") => {
                    if !env.contains_key(&env_var.name)
                        && let Some(value) = env_lookup(&env_var.name)
                    {
                        env.insert(env_var.name, value);
                    }
                }
                Some("remote") => {
                    return Ok(ServerImport::Skipped(
                        "remote-sourced `env_vars` are not supported".to_string(),
                    ));
                }
                Some(_) => {
                    return Err("`env_vars.source` must be `local` or `remote`".to_string());
                }
            }
        }

        return Ok(ServerImport::Imported {
            spec: Box::new(McpServerSpec {
                command: Some(command),
                args: optional_string_list(table, "args")?.unwrap_or_default(),
                env,
                cwd: optional_string(table, "cwd")?,
                disabled,
                transport: None,
                url: None,
                bearer_token_env_var: None,
                http_headers: HashMap::new(),
                env_http_headers: HashMap::new(),
                startup_timeout_sec,
                tool_timeout_sec,
                tools,
                disabled_tools,
                mode: None,
            }),
            ignored_fields,
        });
    }

    let Some(url) = url.filter(|value| !value.trim().is_empty()) else {
        return Ok(ServerImport::Skipped(
            "neither a non-empty stdio `command` nor Streamable HTTP `url` is configured"
                .to_string(),
        ));
    };

    for field in ["args", "cwd", "env", "env_vars"] {
        if table.contains_key(field) {
            return Ok(ServerImport::Skipped(format!(
                "streamable HTTP transport cannot configure `{field}`"
            )));
        }
    }
    if table.contains_key("bearer_token") {
        return Ok(ServerImport::Skipped(
            "literal `bearer_token` values are rejected; use `bearer_token_env_var`".to_string(),
        ));
    }
    if table.contains_key("http_headers_helper") {
        return Ok(ServerImport::Skipped(
            "`http_headers_helper` is not supported".to_string(),
        ));
    }

    Ok(ServerImport::Imported {
        spec: Box::new(McpServerSpec {
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            disabled,
            transport: Some("streamable-http".to_string()),
            url: Some(url),
            bearer_token_env_var: optional_string(table, "bearer_token_env_var")?,
            http_headers: optional_string_map(table, "http_headers")?.unwrap_or_default(),
            env_http_headers: optional_string_map(table, "env_http_headers")?.unwrap_or_default(),
            startup_timeout_sec,
            tool_timeout_sec,
            tools,
            disabled_tools,
            mode: None,
        }),
        ignored_fields,
    })
}

#[derive(Debug)]
struct EnvVarRef {
    name: String,
    source: Option<String>,
}

fn optional_env_vars(table: &toml::Table) -> Result<Option<Vec<EnvVarRef>>, String> {
    let Some(value) = table.get("env_vars") else {
        return Ok(None);
    };
    let Some(values) = value.as_array() else {
        return Err("`env_vars` must be an array".to_string());
    };

    let mut result = Vec::with_capacity(values.len());
    for value in values {
        if let Some(name) = value.as_str() {
            result.push(EnvVarRef {
                name: name.to_string(),
                source: None,
            });
            continue;
        }
        let Some(config) = value.as_table() else {
            return Err("each `env_vars` entry must be a string or table".to_string());
        };
        let name = optional_string(config, "name")?
            .ok_or_else(|| "an `env_vars` table is missing `name`".to_string())?;
        let source = optional_string(config, "source")?;
        result.push(EnvVarRef { name, source });
    }
    Ok(Some(result))
}

fn optional_string(table: &toml::Table, key: &str) -> Result<Option<String>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(|value| Some(value.to_string()))
            .ok_or_else(|| format!("`{key}` must be a string")),
    }
}

fn optional_bool(table: &toml::Table, key: &str) -> Result<Option<bool>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a boolean")),
    }
}

fn optional_timeout_seconds(table: &toml::Table, key: &str) -> Result<Option<f64>, String> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let seconds = value
        .as_float()
        .or_else(|| value.as_integer().map(|value| value as f64))
        .ok_or_else(|| format!("`{key}` must be a non-negative number"))?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(format!("`{key}` must be a non-negative finite number"));
    }
    Ok(Some(seconds))
}

fn optional_timeout_milliseconds(table: &toml::Table, key: &str) -> Result<Option<f64>, String> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let milliseconds = value
        .as_integer()
        .ok_or_else(|| format!("`{key}` must be a non-negative integer"))?;
    if milliseconds < 0 {
        return Err(format!("`{key}` must be a non-negative integer"));
    }
    Ok(Some(milliseconds as f64 / 1000.0))
}

fn optional_string_list(table: &toml::Table, key: &str) -> Result<Option<Vec<String>>, String> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let Some(values) = value.as_array() else {
        return Err(format!("`{key}` must be an array of strings"));
    };
    let mut result = Vec::with_capacity(values.len());
    for value in values {
        let Some(value) = value.as_str() else {
            return Err(format!("`{key}` must contain only strings"));
        };
        result.push(value.to_string());
    }
    Ok(Some(result))
}

fn optional_string_map(
    table: &toml::Table,
    key: &str,
) -> Result<Option<HashMap<String, String>>, String> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let Some(values) = value.as_table() else {
        return Err(format!("`{key}` must be a string map"));
    };
    let mut result = HashMap::with_capacity(values.len());
    for (name, value) in values {
        let Some(value) = value.as_str() else {
            return Err(format!("`{key}` must contain only string values"));
        };
        result.insert(name.clone(), value.to_string());
    }
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_stdio_fields_and_filters_without_leaking_values() {
        let contents = r#"
[mcp_servers.demo]
command = "npx"
args = ["-y", "demo-server"]
cwd = "/tmp/demo"
enabled_tools = ["read", "write"]
disabled_tools = ["write"]
required = true
env_vars = ["TOKEN", { name = "SECOND_TOKEN", source = "local" }]

[mcp_servers.demo.env]
STATIC_SECRET = "do-not-log-this"
"#;
        let outcome = parse_codex_mcp_servers(contents, &|name| match name {
            "TOKEN" => Some("resolved-secret".to_string()),
            "SECOND_TOKEN" => Some("second-secret".to_string()),
            _ => None,
        })
        .unwrap();
        let server = outcome.servers.get("demo").unwrap();
        assert_eq!(server.command.as_deref(), Some("npx"));
        assert_eq!(server.args, ["-y", "demo-server"]);
        assert_eq!(server.cwd.as_deref(), Some("/tmp/demo"));
        assert_eq!(
            server.tools.as_ref().unwrap(),
            &vec!["read".to_string(), "write".to_string()]
        );
        assert_eq!(
            server.disabled_tools.as_ref().unwrap(),
            &vec!["write".to_string()]
        );
        assert_eq!(
            server.env.get("TOKEN").map(String::as_str),
            Some("resolved-secret")
        );
        assert_eq!(
            server.env.get("SECOND_TOKEN").map(String::as_str),
            Some("second-secret")
        );
        assert_eq!(
            server.env.get("STATIC_SECRET").map(String::as_str),
            Some("do-not-log-this")
        );
        let report = outcome.report.join("\n");
        assert!(report.contains("required"));
        assert!(!report.contains("resolved-secret"));
        assert!(!report.contains("second-secret"));
        assert!(!report.contains("do-not-log-this"));
    }

    #[test]
    fn imports_disabled_and_streamable_http_but_skips_remote_execution() {
        let contents = r#"
[mcp_servers.off]
command = "off-server"
enabled = false

[mcp_servers.web]
url = "https://example.invalid/mcp"
bearer_token_env_var = "WEB_TOKEN"
startup_timeout_sec = 12
tool_timeout_sec = 34.5
http_headers = { X-Static = "public" }
env_http_headers = { X-Secret = "WEB_HEADER" }

[mcp_servers.remote]
command = "remote-server"
environment_id = "executor"
"#;
        let outcome = parse_codex_mcp_servers(contents, &|_| None).unwrap();
        assert!(outcome.servers.get("off").unwrap().disabled);
        let web = outcome.servers.get("web").unwrap();
        assert_eq!(web.transport.as_deref(), Some("streamable-http"));
        assert_eq!(web.url.as_deref(), Some("https://example.invalid/mcp"));
        assert_eq!(web.bearer_token_env_var.as_deref(), Some("WEB_TOKEN"));
        assert_eq!(
            web.http_headers.get("X-Static").map(String::as_str),
            Some("public")
        );
        assert_eq!(
            web.env_http_headers.get("X-Secret").map(String::as_str),
            Some("WEB_HEADER")
        );
        assert_eq!(web.startup_timeout_sec, Some(12.0));
        assert_eq!(web.tool_timeout_sec, Some(34.5));
        assert!(!outcome.servers.contains_key("remote"));
        let report = outcome.report.join("\n");
        assert!(report.contains("web -> imported from Codex"));
        assert!(report.contains("remote -> skipped: non-local"));
        assert!(!report.contains("public"));
    }

    #[test]
    fn imports_legacy_startup_timeout_milliseconds() {
        let contents = r#"
[mcp_servers.web]
url = "https://example.invalid/mcp"
startup_timeout_ms = 1250
"#;
        let outcome = parse_codex_mcp_servers(contents, &|_| None).unwrap();
        assert_eq!(
            outcome.servers.get("web").unwrap().startup_timeout_sec,
            Some(1.25)
        );
    }

    #[test]
    fn rejects_non_integer_legacy_startup_timeout_even_when_seconds_are_present() {
        let contents = r#"
[mcp_servers.web]
url = "https://example.invalid/mcp"
startup_timeout_sec = 2
startup_timeout_ms = 12.5
"#;
        let outcome = parse_codex_mcp_servers(contents, &|_| None).unwrap();
        assert!(!outcome.servers.contains_key("web"));
        assert!(
            outcome
                .report
                .join("\n")
                .contains("`startup_timeout_ms` must be a non-negative integer")
        );
    }

    #[test]
    fn rejects_literal_remote_bearer_without_echoing_it() {
        let secret = "literal-secret-that-must-not-appear";
        let contents = format!(
            "[mcp_servers.web]\nurl = \"https://example.invalid/mcp\"\nbearer_token = \"{secret}\"\n"
        );
        let outcome = parse_codex_mcp_servers(&contents, &|_| None).unwrap();
        assert!(!outcome.servers.contains_key("web"));
        let report = outcome.report.join("\n");
        assert!(report.contains("literal `bearer_token` values are rejected"));
        assert!(!report.contains(secret));
    }

    #[test]
    fn bad_server_does_not_hide_valid_sibling() {
        let contents = r#"
[mcp_servers.bad]
command = 42

[mcp_servers.good]
command = "good-server"
"#;
        let outcome = parse_codex_mcp_servers(contents, &|_| None).unwrap();
        assert!(!outcome.servers.contains_key("bad"));
        assert!(outcome.servers.contains_key("good"));
        assert!(
            outcome
                .report
                .join("\n")
                .contains("bad -> skipped: `command`")
        );
    }

    #[test]
    fn remote_sourced_env_var_skips_only_that_server() {
        let contents = r#"
[mcp_servers.remote_env]
command = "remote-env-server"
env_vars = [{ name = "TOKEN", source = "remote" }]

[mcp_servers.good]
command = "good-server"
"#;
        let outcome = parse_codex_mcp_servers(contents, &|_| None).unwrap();
        assert!(!outcome.servers.contains_key("remote_env"));
        assert!(outcome.servers.contains_key("good"));
        assert!(
            outcome
                .report
                .join("\n")
                .contains("remote_env -> skipped: remote-sourced `env_vars`")
        );
    }

    #[test]
    fn invalid_toml_error_does_not_echo_source() {
        let secret = "secret-that-must-not-appear";
        let contents = format!("[mcp_servers.demo]\ncommand = \\\"{secret}");
        let error = parse_codex_mcp_servers(&contents, &|_| None).unwrap_err();
        assert!(!error.contains(secret));
    }

    #[test]
    fn cli_adds_servers_missing_from_config_with_complete_tool_policy() {
        let existing = HashMap::from([("global".to_string(), McpServerSpec::default())]);
        let list = br#"[
            {"name":"global"},
            {"name":"plugin"}
        ]"#;
        let get = br#"{
            "name":"plugin",
            "enabled":true,
            "transport":{
                "type":"stdio",
                "command":"uv",
                "args":["run","plugin-mcp"],
                "env":null,
                "env_vars":["TOKEN",{"name":"SECOND_TOKEN","source":"local"}],
                "cwd":"/plugins/example"
            },
            "enabled_tools":["read","write"],
            "disabled_tools":["write"],
            "startup_timeout_sec":4,
            "tool_timeout_sec":9.5
        }"#;
        let mut calls = 0;
        let outcome = {
            let mut runner = |args: &[OsString]| {
                calls += 1;
                match args.get(1).and_then(|value| value.to_str()) {
                    Some("list") => Ok(list.to_vec()),
                    Some("get") => Ok(get.to_vec()),
                    other => panic!("unexpected Codex CLI operation: {other:?}"),
                }
            };

            discover_additional_codex_mcp_servers_with_runner(
                &existing,
                &|name| match name {
                    "TOKEN" => Some("secret-one".to_string()),
                    "SECOND_TOKEN" => Some("secret-two".to_string()),
                    _ => None,
                },
                &mut runner,
            )
            .unwrap()
        };

        assert_eq!(
            calls, 2,
            "the existing config server must not trigger mcp get"
        );
        assert!(!outcome.servers.contains_key("global"));
        let plugin = outcome.servers.get("plugin").unwrap();
        assert_eq!(plugin.command.as_deref(), Some("uv"));
        assert_eq!(plugin.args, ["run", "plugin-mcp"]);
        assert_eq!(plugin.cwd.as_deref(), Some("/plugins/example"));
        assert_eq!(
            plugin.tools.as_deref(),
            Some(["read".to_string(), "write".to_string()].as_slice())
        );
        assert_eq!(
            plugin.disabled_tools.as_deref(),
            Some(["write".to_string()].as_slice())
        );
        assert_eq!(
            plugin.env.get("TOKEN").map(String::as_str),
            Some("secret-one")
        );
        assert_eq!(
            plugin.env.get("SECOND_TOKEN").map(String::as_str),
            Some("secret-two")
        );
        assert_eq!(plugin.startup_timeout_sec, Some(4.0));
        assert_eq!(plugin.tool_timeout_sec, Some(9.5));
        let report = outcome.report.join("\n");
        assert!(report.contains("plugin -> imported from Codex CLI"));
        assert!(!report.contains("secret-one"));
        assert!(!report.contains("secret-two"));
    }

    #[test]
    fn cli_imports_streamable_http_servers_with_auth_headers_and_timeouts() {
        let list = br#"[{"name":"plugin-web"}]"#;
        let get = br#"{
            "name":"plugin-web",
            "enabled":true,
            "transport":{
                "type":"streamable_http",
                "url":"https://example.invalid/mcp",
                "bearer_token_env_var":"WEB_TOKEN",
                "http_headers":{"X-Static":"public"},
                "env_http_headers":{"X-Tenant":"WEB_TENANT"},
                "http_headers_helper":null
            },
            "enabled_tools":["read"],
            "disabled_tools":null,
            "startup_timeout_sec":12,
            "tool_timeout_sec":34.5
        }"#;
        let mut runner = |args: &[OsString]| match args.get(1).and_then(|value| value.to_str()) {
            Some("list") => Ok(list.to_vec()),
            Some("get") => Ok(get.to_vec()),
            other => panic!("unexpected Codex CLI operation: {other:?}"),
        };

        let outcome = discover_additional_codex_mcp_servers_with_runner(
            &HashMap::new(),
            &|_| None,
            &mut runner,
        )
        .unwrap();
        let server = outcome.servers.get("plugin-web").unwrap();
        assert_eq!(server.transport.as_deref(), Some("streamable-http"));
        assert_eq!(server.url.as_deref(), Some("https://example.invalid/mcp"));
        assert_eq!(server.bearer_token_env_var.as_deref(), Some("WEB_TOKEN"));
        assert_eq!(
            server.http_headers.get("X-Static").map(String::as_str),
            Some("public")
        );
        assert_eq!(
            server.env_http_headers.get("X-Tenant").map(String::as_str),
            Some("WEB_TENANT")
        );
        assert_eq!(server.startup_timeout_sec, Some(12.0));
        assert_eq!(server.tool_timeout_sec, Some(34.5));
        assert_eq!(
            server.tools.as_deref(),
            Some(["read".to_string()].as_slice())
        );
    }

    #[test]
    fn cli_rejects_unsafe_or_mixed_transport_configuration_without_echoing_secrets() {
        let secret = "literal-cli-secret";
        let literal: CodexCliServer = serde_json::from_value(serde_json::json!({
            "name": "literal",
            "transport": {
                "type": "http",
                "url": "https://example.invalid/mcp",
                "bearer_token": secret
            }
        }))
        .unwrap();
        let error = codex_cli_server_to_spec(literal, &|_| None).unwrap_err();
        assert!(error.contains("literal bearer tokens are rejected"));
        assert!(!error.contains(secret));

        let mixed: CodexCliServer = serde_json::from_value(serde_json::json!({
            "name": "mixed",
            "transport": {
                "type": "streamable-http",
                "url": "https://example.invalid/mcp",
                "command": "should-not-run"
            }
        }))
        .unwrap();
        assert!(
            codex_cli_server_to_spec(mixed, &|_| None)
                .unwrap_err()
                .contains("stdio-only fields")
        );

        let helper: CodexCliServer = serde_json::from_value(serde_json::json!({
            "name": "helper",
            "transport": {
                "type": "streamable_http",
                "url": "https://example.invalid/mcp",
                "http_headers_helper": "<redacted>"
            }
        }))
        .unwrap();
        assert!(
            codex_cli_server_to_spec(helper, &|_| None)
                .unwrap_err()
                .contains("http_headers_helper")
        );

        let negative_timeout: CodexCliServer = serde_json::from_value(serde_json::json!({
            "name": "negative-timeout",
            "transport": {
                "type": "stdio",
                "command": "server"
            },
            "tool_timeout_sec": -1
        }))
        .unwrap();
        assert!(
            codex_cli_server_to_spec(negative_timeout, &|_| None)
                .unwrap_err()
                .contains("non-negative finite number")
        );
    }

    #[test]
    fn cli_skips_plugin_server_when_full_configuration_cannot_be_read() {
        let list = br#"[{"name":"plugin"}]"#;
        let mut runner = |args: &[OsString]| match args.get(1).and_then(|value| value.to_str()) {
            Some("list") => Ok(list.to_vec()),
            Some("get") => Err("synthetic get failure".to_string()),
            other => panic!("unexpected Codex CLI operation: {other:?}"),
        };

        let outcome = discover_additional_codex_mcp_servers_with_runner(
            &HashMap::new(),
            &|_| None,
            &mut runner,
        )
        .unwrap();

        assert!(outcome.servers.is_empty());
        assert!(outcome.report.join("\n").contains("synthetic get failure"));
    }

    #[test]
    fn missing_codex_cli_is_reported_without_requiring_config_parsing() {
        let temp = tempfile::TempDir::new().unwrap();
        let missing = temp.path().join("missing-codex-cli");
        let error = discover_additional_codex_mcp_servers_with_cli(
            missing.as_os_str(),
            None,
            &HashMap::new(),
        )
        .unwrap_err();
        assert!(error.contains("was not found"));
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let temp = tempfile::TempDir::new().unwrap();
        let result = discover_codex_mcp_servers(&temp.path().join("missing.toml")).unwrap();
        assert!(result.is_none());
    }
}
