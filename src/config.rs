//! CLI parsing and config loading. Ports `src/config.ts`.
//!
//! An existing `codex.config.json` keeps working: every field is read with its
//! original camelCase name, absent sections fall back to the same defaults the
//! TypeScript used, and a missing config file is tolerated.

use std::path::{Path, PathBuf};

use clap::Parser;
use serde::Deserialize;

use std::collections::HashMap;

use crate::codex_mcp::{codex_config_path, discover_codex_mcp_servers};
use crate::types::{
    AppConfig, CommandConfig, ExecConfig, ExecMode, IgnoreConfig, McpServerSpec, MemoryConfig,
    OpenAiTunnelConfig, OutputConfig, ProjectDocConfig, SkillsConfig, TreeConfig,
};

#[derive(Parser, Debug)]
#[command(
    name = "codexify",
    about = "Codexify MCP bridge (Rust): expose Codex-style agent tools over Streamable HTTP."
)]
pub struct Cli {
    /// Project directory, or the access root when --multi-project is enabled.
    #[arg(long = "work-dir")]
    pub work_dir: String,

    /// Let each MCP session bind once to a project below --work-dir.
    #[arg(long = "multi-project")]
    pub multi_project: bool,

    /// Server port. Default: 3000 (or the config file's value).
    #[arg(long)]
    pub port: Option<u16>,

    /// Bearer token for auth. When set, every request except /health must carry it.
    #[arg(long = "api-key")]
    pub api_key: Option<String>,

    /// Config file path. Default: ./codex.config.json (tolerated if missing).
    #[arg(long)]
    pub config: Option<String>,

    /// Existing OpenAI Secure MCP Tunnel id. Enables the outbound native tunnel.
    #[arg(long = "openai-tunnel-id")]
    pub openai_tunnel_id: Option<String>,

    /// Runtime API-key reference: env:NAME or file:/path/to/key.
    #[arg(long = "openai-tunnel-api-key-ref")]
    pub openai_tunnel_api_key_ref: Option<String>,

    /// Explicit tunnel-client or tunnel-client-runtime binary.
    #[arg(long = "openai-tunnel-client")]
    pub openai_tunnel_client: Option<String>,

    /// Optional OpenAI organization id sent by tunnel-client.
    #[arg(long = "openai-tunnel-organization-id")]
    pub openai_tunnel_organization_id: Option<String>,
}

fn default_allowed_commands() -> Vec<String> {
    [
        "bun", "npm", "npx", "node", "git", "python", "pip", "cargo", "make",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn default_extra_allowed() -> Vec<String> {
    [
        "ls", "cat", "grep", "find", "head", "tail", "wc", "echo", "pwd", "which", "rg", "sed",
        "awk", "sort", "uniq", "diff", "true", "false",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn default_tree() -> TreeConfig {
    TreeConfig {
        default_depth: 3,
        ignore: ["node_modules", ".git", "dist", ".next", "__pycache__"]
            .into_iter()
            .map(String::from)
            .collect(),
    }
}

fn default_command() -> CommandConfig {
    CommandConfig {
        default_timeout: 30_000,
        max_timeout: 120_000,
    }
}

fn default_exec() -> ExecConfig {
    ExecConfig {
        mode: ExecMode::Allowlist,
        extra_allowed_commands: default_extra_allowed(),
        max_sessions: 8,
        default_shell: None,
    }
}

// ─── File config (all optional) ────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartialTree {
    default_depth: Option<usize>,
    ignore: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartialCommand {
    default_timeout: Option<u64>,
    max_timeout: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartialExec {
    mode: Option<ExecMode>,
    extra_allowed_commands: Option<Vec<String>>,
    max_sessions: Option<usize>,
    default_shell: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartialOpenAiTunnel {
    tunnel_id: Option<String>,
    api_key_ref: Option<String>,
    client_path: Option<String>,
    organization_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexMcpConfig {
    enabled: Option<bool>,
}

impl CodexMcpConfig {
    fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartialMcpServerSpec {
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<HashMap<String, String>>,
    cwd: Option<String>,
    disabled: Option<bool>,
    #[serde(rename = "type")]
    transport: Option<String>,
    url: Option<String>,
    tools: Option<Vec<String>>,
    disabled_tools: Option<Vec<String>>,
    mode: Option<String>,
}

impl PartialMcpServerSpec {
    fn overlay(self, mut base: McpServerSpec) -> McpServerSpec {
        let Self {
            command,
            args,
            env,
            cwd,
            disabled,
            transport,
            url,
            tools,
            disabled_tools,
            mode,
        } = self;
        let sets_command = command.is_some();
        let sets_url = url.is_some();
        let sets_transport = transport.is_some();

        if let Some(command) = command {
            base.command = Some(command);
        }
        if let Some(args) = args {
            base.args = args;
        }
        if let Some(env) = env {
            base.env = env;
        }
        if let Some(cwd) = cwd {
            base.cwd = Some(cwd);
        }
        if let Some(disabled) = disabled {
            base.disabled = disabled;
        }
        if let Some(transport) = transport {
            base.transport = Some(transport);
        }
        if let Some(url) = url {
            base.url = Some(url);
        }
        if let Some(tools) = tools {
            base.tools = Some(tools);
        }
        if let Some(disabled_tools) = disabled_tools {
            base.disabled_tools = Some(disabled_tools);
        }
        if let Some(mode) = mode {
            base.mode = Some(mode);
        }

        // Naming a different transport replaces the imported transport rather
        // than leaving an impossible command+URL hybrid behind.
        if sets_command && !sets_url {
            base.url = None;
            if !sets_transport {
                base.transport = None;
            }
        } else if sets_url && !sets_command {
            base.command = None;
            if !sets_transport {
                base.transport = Some("streamable-http".to_string());
            }
        }

        base
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileConfig {
    api_key: Option<String>,
    port: Option<u16>,
    multi_project: Option<bool>,
    allowed_commands: Option<Vec<String>>,
    tree: Option<PartialTree>,
    command: Option<PartialCommand>,
    exec: Option<PartialExec>,
    project_doc: Option<ProjectDocConfig>,
    output: Option<OutputConfig>,
    memory: Option<MemoryConfig>,
    skills: Option<SkillsConfig>,
    ignore: Option<IgnoreConfig>,
    codex_mcp: Option<CodexMcpConfig>,
    allowed_hosts: Option<Vec<String>>,
    openai_tunnel: Option<PartialOpenAiTunnel>,
    mcp_servers: Option<HashMap<String, PartialMcpServerSpec>>,
}

fn merge_mcp_servers(
    mut imported: HashMap<String, McpServerSpec>,
    explicit: HashMap<String, PartialMcpServerSpec>,
) -> (HashMap<String, McpServerSpec>, Vec<String>) {
    let mut entries: Vec<(String, PartialMcpServerSpec)> = explicit.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut report = Vec::new();
    for (name, overlay) in entries {
        let imported_entry = imported.remove(&name);
        if imported_entry.is_some() {
            report.push(format!(
                "{name} -> imported fields overlaid by codex.config.json"
            ));
        }
        let base = imported_entry.unwrap_or_default();
        imported.insert(name, overlay.overlay(base));
    }
    (imported, report)
}

fn resolve_mcp_servers(file: &mut FileConfig) -> HashMap<String, McpServerSpec> {
    let discovery_enabled = file.codex_mcp.take().unwrap_or_default().enabled();
    let explicit = file.mcp_servers.take().unwrap_or_default();

    if !discovery_enabled {
        println!("Codex MCP discovery: disabled by codexMcp.enabled=false");
        return merge_mcp_servers(HashMap::new(), explicit).0;
    }

    let path = match codex_config_path() {
        Ok(path) => path,
        Err(error) => {
            println!("Codex MCP discovery: skipped ({error})");
            return merge_mcp_servers(HashMap::new(), explicit).0;
        }
    };

    let discovery = match discover_codex_mcp_servers(&path) {
        Ok(Some(discovery)) => discovery,
        Ok(None) => return merge_mcp_servers(HashMap::new(), explicit).0,
        Err(error) => {
            println!(
                "Codex MCP discovery: failed for {} ({error})",
                path.display()
            );
            return merge_mcp_servers(HashMap::new(), explicit).0;
        }
    };

    let (servers, overlay_report) = merge_mcp_servers(discovery.servers, explicit);
    let mut report = discovery.report;
    report.extend(overlay_report);
    if !report.is_empty() {
        println!("Codex MCP discovery: {}", path.display());
        for line in report {
            println!("  {line}");
        }
    }
    servers
}

/// A fully-defaulted config for a given work directory, matching what
/// `load_config` produces from an empty config file. Handy for tests and for
/// embedding the server without a config file.
pub fn default_config(work_dir: std::path::PathBuf) -> AppConfig {
    AppConfig {
        work_dir,
        multi_project: false,
        api_key: None,
        port: 3000,
        allowed_commands: default_allowed_commands(),
        tree: default_tree(),
        command: default_command(),
        exec: default_exec(),
        project_doc: ProjectDocConfig::default(),
        output: OutputConfig::default(),
        memory: MemoryConfig::default(),
        skills: SkillsConfig::default(),
        ignore: IgnoreConfig::default(),
        allowed_hosts: Vec::new(),
        openai_tunnel: None,
        mcp_servers: HashMap::new(),
        generated_skills_dir: None,
    }
}

/// Resolve `work_dir` against the current directory when relative. The path is
/// stored as-is (matching the TS, which keeps `cli.workDir` verbatim for display);
/// `memory_dir` normalises separately when hashing so trailing-slash variants
/// still key the same per-project state.
fn resolve_work_dir(raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

fn resolve_path(raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

fn valid_tunnel_id(value: &str) -> bool {
    value.strip_prefix("tunnel_").is_some_and(|suffix| {
        suffix.len() == 32
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    })
}

fn valid_env_name(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn resolve_api_key_ref(raw: &str) -> Result<String, String> {
    if let Some(name) = raw.strip_prefix("env:") {
        if valid_env_name(name) {
            return Ok(raw.to_string());
        }
        return Err("openaiTunnel.apiKeyRef has an invalid environment-variable name".into());
    }
    if let Some(path) = raw.strip_prefix("file:") {
        if path.trim().is_empty() {
            return Err("openaiTunnel.apiKeyRef file path is empty".into());
        }
        return Ok(format!("file:{}", resolve_path(path).display()));
    }
    Err(
        "openaiTunnel.apiKeyRef must be env:NAME or file:/path; literal API keys are rejected"
            .into(),
    )
}

fn resolve_openai_tunnel(
    file: Option<PartialOpenAiTunnel>,
    cli: &Cli,
) -> Result<Option<OpenAiTunnelConfig>, String> {
    let requested = file.is_some()
        || cli.openai_tunnel_id.is_some()
        || cli.openai_tunnel_api_key_ref.is_some()
        || cli.openai_tunnel_client.is_some()
        || cli.openai_tunnel_organization_id.is_some();
    if !requested {
        return Ok(None);
    }

    let file = file.unwrap_or_default();
    let tunnel_id = cli
        .openai_tunnel_id
        .clone()
        .or(file.tunnel_id)
        .ok_or_else(|| "openaiTunnel requires tunnelId (or --openai-tunnel-id)".to_string())?;
    if !valid_tunnel_id(&tunnel_id) {
        return Err(
            "OpenAI tunnel id must be tunnel_ followed by 32 lowercase letters or digits".into(),
        );
    }

    let api_key_ref = resolve_api_key_ref(
        cli.openai_tunnel_api_key_ref
            .as_deref()
            .or(file.api_key_ref.as_deref())
            .unwrap_or("env:CONTROL_PLANE_API_KEY"),
    )?;
    let client_path = cli
        .openai_tunnel_client
        .as_deref()
        .or(file.client_path.as_deref())
        .map(resolve_path);
    let organization_id = cli
        .openai_tunnel_organization_id
        .clone()
        .or(file.organization_id)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if organization_id
        .as_deref()
        .is_some_and(|value| value.chars().any(char::is_control))
    {
        return Err("openaiTunnel.organizationId must not contain control characters".into());
    }

    Ok(Some(OpenAiTunnelConfig {
        tunnel_id,
        api_key_ref,
        organization_id,
        client_path,
    }))
}

/// Load and merge config. Errors are returned as strings for the caller to
/// print and exit on, mirroring the TS which validates and `process.exit`s.
pub fn load_config(cli: Cli) -> Result<AppConfig, String> {
    let work_dir = resolve_work_dir(&cli.work_dir);

    // Validate work-dir exists and is a directory.
    match std::fs::metadata(&work_dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "work-dir is not a directory: {}",
                work_dir.display()
            ));
        }
        Err(_) => return Err(format!("work-dir does not exist: {}", work_dir.display())),
    }

    let config_path = cli
        .config
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("codex.config.json"));

    // Show the absolute path of the config actually loaded, so it is obvious
    // when codexify picked up a different file than the one being edited.
    let display_path = if config_path.is_absolute() {
        config_path.clone()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(&config_path)
    };
    let mut file: FileConfig = match std::fs::read_to_string(&config_path) {
        Ok(text) => {
            println!("Config: {}", display_path.display());
            serde_json::from_str(&text)
                .map_err(|e| format!("invalid config file {}: {e}", config_path.display()))?
        }
        Err(_) => {
            println!(
                "Config: no file at {} — using built-in defaults (pass --config to point elsewhere)",
                display_path.display()
            );
            FileConfig::default()
        }
    };

    let mut tree = default_tree();
    if let Some(t) = file.tree.take() {
        if let Some(d) = t.default_depth {
            tree.default_depth = d;
        }
        if let Some(ig) = t.ignore {
            tree.ignore = ig;
        }
    }

    let mut command = default_command();
    if let Some(c) = file.command.take() {
        if let Some(d) = c.default_timeout {
            command.default_timeout = d;
        }
        if let Some(m) = c.max_timeout {
            command.max_timeout = m;
        }
    }

    let mut exec = default_exec();
    if let Some(e) = file.exec.take() {
        if let Some(m) = e.mode {
            exec.mode = m;
        }
        if let Some(x) = e.extra_allowed_commands {
            exec.extra_allowed_commands = x;
        }
        if let Some(s) = e.max_sessions {
            exec.max_sessions = s;
        }
        if e.default_shell.is_some() {
            exec.default_shell = e.default_shell;
        }
    }

    let mcp_servers = resolve_mcp_servers(&mut file);
    let openai_tunnel = resolve_openai_tunnel(file.openai_tunnel, &cli)?;
    let api_key = cli.api_key.or(file.api_key);
    if api_key.is_some() && openai_tunnel.is_some() {
        return Err(
            "apiKey/--api-key cannot be combined with openaiTunnel: native tunnel mode generates a private per-process bearer for the loopback MCP hop"
                .into(),
        );
    }

    Ok(AppConfig {
        work_dir,
        multi_project: cli.multi_project || file.multi_project.unwrap_or(false),
        api_key,
        port: cli.port.or(file.port).unwrap_or(3000),
        allowed_commands: file
            .allowed_commands
            .unwrap_or_else(default_allowed_commands),
        tree,
        command,
        exec,
        project_doc: file.project_doc.unwrap_or_default(),
        output: file.output.unwrap_or_default(),
        memory: file.memory.unwrap_or_default(),
        skills: file.skills.unwrap_or_default(),
        ignore: file.ignore.unwrap_or_default(),
        allowed_hosts: file.allowed_hosts.unwrap_or_default(),
        openai_tunnel,
        mcp_servers,
        generated_skills_dir: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imported_server() -> McpServerSpec {
        McpServerSpec {
            command: Some("codex-server".to_string()),
            args: vec!["--stdio".to_string()],
            env: HashMap::from([("TOKEN".to_string(), "secret".to_string())]),
            cwd: Some("/codex/cwd".to_string()),
            disabled: true,
            transport: None,
            url: None,
            tools: Some(vec!["read".to_string()]),
            disabled_tools: Some(vec!["write".to_string()]),
            mode: None,
        }
    }

    fn cli(work_dir: &Path, config: &Path) -> Cli {
        Cli {
            work_dir: work_dir.to_string_lossy().into_owned(),
            multi_project: false,
            port: None,
            api_key: None,
            config: Some(config.to_string_lossy().into_owned()),
            openai_tunnel_id: None,
            openai_tunnel_api_key_ref: None,
            openai_tunnel_client: None,
            openai_tunnel_organization_id: None,
        }
    }

    #[test]
    fn local_entry_can_add_gateway_mode_without_repeating_launch_config() {
        let imported = HashMap::from([("demo".to_string(), imported_server())]);
        let explicit = HashMap::from([(
            "demo".to_string(),
            PartialMcpServerSpec {
                mode: Some("gateway".to_string()),
                disabled: Some(false),
                ..Default::default()
            },
        )]);

        let (merged, report) = merge_mcp_servers(imported, explicit);
        let server = merged.get("demo").unwrap();
        assert_eq!(server.command.as_deref(), Some("codex-server"));
        assert_eq!(server.args, ["--stdio"]);
        assert_eq!(server.env.get("TOKEN").map(String::as_str), Some("secret"));
        assert_eq!(server.mode.as_deref(), Some("gateway"));
        assert!(!server.disabled);
        assert_eq!(report.len(), 1);
    }

    #[test]
    fn explicit_command_replaces_imported_url_transport() {
        let imported = HashMap::from([(
            "demo".to_string(),
            McpServerSpec {
                transport: Some("streamable-http".to_string()),
                url: Some("https://example.invalid/mcp".to_string()),
                ..Default::default()
            },
        )]);
        let explicit = HashMap::from([(
            "demo".to_string(),
            PartialMcpServerSpec {
                command: Some("local-server".to_string()),
                ..Default::default()
            },
        )]);

        let (merged, _) = merge_mcp_servers(imported, explicit);
        let server = merged.get("demo").unwrap();
        assert_eq!(server.command.as_deref(), Some("local-server"));
        assert!(server.url.is_none());
        assert!(server.transport.is_none());
    }

    #[test]
    fn json_config_accepts_partial_camel_case_overlay() {
        let file: FileConfig = serde_json::from_str(
            r#"{
                "codexMcp": { "enabled": false },
                "mcpServers": {
                    "demo": {
                        "mode": "gateway",
                        "disabledTools": ["write"]
                    }
                }
            }"#,
        )
        .unwrap();

        assert!(!file.codex_mcp.as_ref().unwrap().enabled());
        let demo = file.mcp_servers.as_ref().unwrap().get("demo").unwrap();
        assert_eq!(demo.mode.as_deref(), Some("gateway"));
        assert_eq!(
            demo.disabled_tools.as_deref(),
            Some(&["write".to_string()][..])
        );
        assert!(demo.command.is_none());
    }

    #[test]
    fn loads_native_tunnel_with_a_secret_reference_default() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.json");
        std::fs::write(
            &config_path,
            r#"{"openaiTunnel":{"tunnelId":"tunnel_0123456789abcdef0123456789abcdef"}}"#,
        )
        .unwrap();

        let config = load_config(cli(root.path(), &config_path)).unwrap();
        let tunnel = config.openai_tunnel.unwrap();
        assert_eq!(tunnel.tunnel_id, "tunnel_0123456789abcdef0123456789abcdef");
        assert_eq!(tunnel.api_key_ref, "env:CONTROL_PLANE_API_KEY");
        assert!(tunnel.client_path.is_none());
    }

    #[test]
    fn rejects_literal_tunnel_api_keys() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.json");
        std::fs::write(
            &config_path,
            r#"{"openaiTunnel":{"tunnelId":"tunnel_0123456789abcdef0123456789abcdef","apiKeyRef":"sk-literal-secret-value"}}"#,
        )
        .unwrap();

        let error = load_config(cli(root.path(), &config_path)).unwrap_err();
        assert!(error.contains("literal API keys are rejected"));
    }

    #[test]
    fn rejects_local_bearer_auth_in_native_tunnel_mode() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.json");
        std::fs::write(
            &config_path,
            r#"{"apiKey":"local-token","openaiTunnel":{"tunnelId":"tunnel_0123456789abcdef0123456789abcdef"}}"#,
        )
        .unwrap();

        let error = load_config(cli(root.path(), &config_path)).unwrap_err();
        assert!(error.contains("cannot be combined with openaiTunnel"));
    }

    #[test]
    fn cli_tunnel_fields_override_the_file() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.json");
        std::fs::write(
            &config_path,
            r#"{"openaiTunnel":{"tunnelId":"tunnel_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","apiKeyRef":"env:OLD_KEY"}}"#,
        )
        .unwrap();
        let mut args = cli(root.path(), &config_path);
        args.openai_tunnel_id = Some("tunnel_0123456789abcdef0123456789abcdef".to_string());
        args.openai_tunnel_api_key_ref = Some("env:NEW_KEY".to_string());
        args.openai_tunnel_client = Some("bin/tunnel-client".to_string());

        let config = load_config(args).unwrap();
        let tunnel = config.openai_tunnel.unwrap();
        assert_eq!(tunnel.api_key_ref, "env:NEW_KEY");
        assert_eq!(
            tunnel.client_path.unwrap(),
            std::env::current_dir().unwrap().join("bin/tunnel-client")
        );
    }

    #[test]
    fn validates_the_native_tunnel_id_shape() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.json");
        std::fs::write(
            &config_path,
            r#"{"openaiTunnel":{"tunnelId":"tunnel_NOT_HEX"}}"#,
        )
        .unwrap();

        let error = load_config(cli(root.path(), &config_path)).unwrap_err();
        assert!(error.contains("32 lowercase letters or digits"));

        std::fs::write(
            &config_path,
            r#"{"openaiTunnel":{"tunnelId":"tunnel_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}}"#,
        )
        .unwrap();
        let error = load_config(cli(root.path(), &config_path)).unwrap_err();
        assert!(error.contains("32 lowercase letters or digits"));

        std::fs::write(
            &config_path,
            r#"{"openaiTunnel":{"tunnelId":"tunnel_0123456789abcdefghijklmnopqrstuv"}}"#,
        )
        .unwrap();
        assert!(load_config(cli(root.path(), &config_path)).is_ok());
    }
}
