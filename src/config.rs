//! CLI parsing and config loading. Ports `src/config.ts`.
//!
//! An existing `codex.config.json` keeps working: every field is read with its
//! original camelCase name, absent sections fall back to the same defaults the
//! TypeScript used, and a missing config file is tolerated.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{ArgAction, Args, Parser, Subcommand};
use serde::Deserialize;

use std::collections::HashMap;

use crate::codex_config::codex_config_path;
use crate::codex_mcp::{
    CodexMcpImport, discover_additional_codex_mcp_servers_with_cli, discover_codex_mcp_servers,
};
use crate::openai_tunnel::validate_tunnel_id;
use crate::project_catalog::{ProjectCatalog, discover_project_catalog_at};
use crate::types::{
    AppConfig, ArtifactIngressConfig, AuditConfig, CodexProjectCatalogConfig, CommandConfig,
    ExecConfig, ExecMode, IgnoreConfig, McpServerSpec, MemoryConfig, OpenAiTunnelConfig,
    OutputConfig, ProjectCatalogConfig, ProjectCatalogEntryConfig, ProjectDocConfig, ReviewConfig,
    SkillsConfig, TreeConfig, WorktreeConfig, WorktreeMode, WorktreeUpstreamRefreshMode,
};
use crate::util::home_dir;

#[derive(Parser, Debug)]
#[command(
    name = "codexify",
    about = "Codexify MCP bridge (Rust): expose Codex-style agent tools over Streamable HTTP.",
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// Increase codexify diagnostics. Repeat for trace-level diagnostics.
    #[arg(
        short = 'v',
        long = "verbose",
        visible_alias = "log-tool-calls",
        action = ArgAction::Count,
        global = true
    )]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Option<CliCommand>,

    /// Project directory for serving, or the access root for multi-project and catalogue modes.
    #[arg(long = "work-dir", global = true)]
    pub work_dir: Option<String>,

    /// Let each ChatGPT conversation bind once to a project below --work-dir.
    /// Other MCP clients fall back to transport-session binding.
    #[arg(long = "multi-project")]
    pub multi_project: bool,

    /// Worktree policy for new multi-project conversation bindings.
    #[arg(long = "worktree-mode", value_enum)]
    pub worktree_mode: Option<WorktreeMode>,

    /// Managed-worktree root. Default: Codex's configured Worktree location.
    #[arg(long = "worktree-root")]
    pub worktree_root: Option<String>,

    /// Server port. Default: 3000 (or the config file's value).
    #[arg(long)]
    pub port: Option<u16>,

    /// Bearer token for auth. When set, every request except /health must carry it.
    #[arg(long = "api-key")]
    pub api_key: Option<String>,

    /// Config file path. Default: ./codex.config.json (tolerated if missing).
    #[arg(long, global = true)]
    pub config: Option<String>,

    /// Require Codex CLI-backed MCP discovery. Without this flag, the CLI is
    /// used automatically when available and config.toml remains the fallback.
    #[arg(long = "codex-cli")]
    pub codex_cli: bool,

    /// Append privacy-preserving tool activity events to a JSONL file.
    #[arg(
        long = "audit",
        visible_alias = "audit-log",
        value_name = "FILE",
        global = true
    )]
    pub audit_log: Option<String>,

    /// Include bounded, redacted previews of exec_command and run_command.
    #[arg(long = "audit-command-preview", global = true)]
    pub audit_command_preview: bool,

    /// Redact the value of this environment variable from command previews.
    #[arg(long = "audit-redact-env", value_name = "NAME", action = ArgAction::Append, global = true)]
    pub audit_redact_env: Vec<String>,

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

#[derive(Subcommand, Debug)]
pub enum CliCommand {
    /// Inspect the read-only project catalogue used by multi-project mode.
    Projects {
        #[command(subcommand)]
        command: ProjectsCommand,
    },
    /// Interactively configure a native OpenAI tunnel and ChatGPT connector.
    ///
    /// The wizard reads the global `--config` and `--work-dir` options.
    Quickstart,
}

#[derive(Subcommand, Debug)]
pub enum ProjectsCommand {
    /// List selectable projects without starting the MCP server.
    List(ProjectsListArgs),
}

#[derive(Args, Debug)]
pub struct ProjectsListArgs {
    /// Case-insensitive filter over names, aliases, descriptions, and selectors.
    #[arg(long)]
    pub query: Option<String>,

    /// Maximum number of matching projects to print (1-200).
    #[arg(long, default_value_t = crate::project_catalog::MAX_PROJECT_LIMIT)]
    pub limit: usize,

    /// Emit stable machine-readable JSON.
    #[arg(long)]
    pub json: bool,

    /// Include skipped paths and detailed local diagnostics.
    #[arg(long = "show-skipped")]
    pub show_skipped: bool,
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
        idle_timeout_ms: 300_000,
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
    idle_timeout_ms: Option<u64>,
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
struct PartialAudit {
    log_file: Option<String>,
    include_command_preview: Option<bool>,
    command_preview_max_bytes: Option<usize>,
    redact_env: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartialWorktrees {
    mode: Option<WorktreeMode>,
    root: Option<String>,
    upstream_refresh_mode: Option<WorktreeUpstreamRefreshMode>,
    auto_cleanup_enabled: Option<bool>,
    keep_count: Option<usize>,
    allow_setup_script: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexMcpConfig {
    enabled: Option<bool>,
    use_cli: Option<bool>,
    cli_path: Option<String>,
}

impl CodexMcpConfig {
    fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    fn use_cli(&self) -> bool {
        self.use_cli.unwrap_or(true)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartialCodexProjectCatalogConfig {
    enabled: Option<bool>,
    trusted_only: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartialProjectCatalogEntry {
    path: Option<String>,
    name: Option<String>,
    aliases: Option<Vec<String>>,
    description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartialProjectCatalog {
    codex_config: Option<PartialCodexProjectCatalogConfig>,
    entries: Option<Vec<PartialProjectCatalogEntry>>,
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
    worktrees: Option<PartialWorktrees>,
    allowed_commands: Option<Vec<String>>,
    tree: Option<PartialTree>,
    command: Option<PartialCommand>,
    exec: Option<PartialExec>,
    project_doc: Option<ProjectDocConfig>,
    output: Option<OutputConfig>,
    review: Option<ReviewConfig>,
    artifact_ingress: Option<ArtifactIngressConfig>,
    memory: Option<MemoryConfig>,
    skills: Option<SkillsConfig>,
    ignore: Option<IgnoreConfig>,
    audit: Option<PartialAudit>,
    codex_mcp: Option<CodexMcpConfig>,
    project_catalog: Option<PartialProjectCatalog>,
    allowed_hosts: Option<Vec<String>>,
    openai_tunnel: Option<PartialOpenAiTunnel>,
    mcp_servers: Option<HashMap<String, PartialMcpServerSpec>>,
}

fn resolve_project_catalog(file: &mut FileConfig) -> ProjectCatalogConfig {
    let catalog = file.project_catalog.take().unwrap_or_default();
    let codex = catalog.codex_config.unwrap_or_default();
    ProjectCatalogConfig {
        codex_config: CodexProjectCatalogConfig {
            enabled: codex.enabled.unwrap_or(true),
            trusted_only: codex.trusted_only.unwrap_or(true),
        },
        entries: catalog
            .entries
            .unwrap_or_default()
            .into_iter()
            .map(|entry| ProjectCatalogEntryConfig {
                path: entry.path,
                name: entry.name,
                aliases: entry.aliases.unwrap_or_default(),
                description: entry.description,
            })
            .collect(),
    }
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

fn codex_cli_command(settings: &CodexMcpConfig) -> OsString {
    let raw = settings
        .cli_path
        .as_ref()
        .filter(|value| !value.trim().is_empty())
        .map(OsString::from)
        .or_else(|| std::env::var_os("CODEX_CLI_PATH").filter(|value| !value.is_empty()))
        .unwrap_or_else(|| OsString::from("codex"));
    let path = PathBuf::from(&raw);
    if path.is_absolute() || path.components().count() > 1 {
        if path.is_absolute() {
            path.into_os_string()
        } else {
            std::env::current_dir()
                .unwrap_or_default()
                .join(path)
                .into_os_string()
        }
    } else {
        raw
    }
}

fn print_discovery_report(header: &str, report: &[String]) {
    if report.is_empty() {
        return;
    }
    println!("{header}");
    for line in report {
        println!("  {line}");
    }
}

fn resolve_mcp_servers(
    file: &mut FileConfig,
    cli: &Cli,
) -> Result<HashMap<String, McpServerSpec>, String> {
    resolve_mcp_servers_from(file, cli, codex_config_path(), None)
}

fn resolve_mcp_servers_from(
    file: &mut FileConfig,
    cli: &Cli,
    config_path_result: Result<PathBuf, String>,
    command_override: Option<OsString>,
) -> Result<HashMap<String, McpServerSpec>, String> {
    let settings = file.codex_mcp.take().unwrap_or_default();
    let discovery_enabled = cli.codex_cli || settings.enabled();
    let explicit = file.mcp_servers.take().unwrap_or_default();

    if !discovery_enabled {
        println!("Codex MCP discovery: disabled by codexMcp.enabled=false");
        return Ok(merge_mcp_servers(HashMap::new(), explicit).0);
    }

    let mut imported = CodexMcpImport::default();
    let config_path = match config_path_result {
        Ok(path) => Some(path),
        Err(error) => {
            println!("Codex MCP config discovery: skipped ({error})");
            None
        }
    };

    if let Some(path) = &config_path {
        match discover_codex_mcp_servers(path) {
            Ok(Some(discovery)) => imported = discovery,
            Ok(None) => {}
            Err(error) => {
                println!(
                    "Codex MCP config discovery: failed for {} ({error})",
                    path.display()
                );
            }
        }
        print_discovery_report(
            &format!("Codex MCP config discovery: {}", path.display()),
            &imported.report,
        );
    }

    if cli.codex_cli || settings.use_cli() {
        let command = command_override.unwrap_or_else(|| codex_cli_command(&settings));
        let cwd = config_path
            .as_deref()
            .and_then(Path::parent)
            .filter(|path| path.is_dir());
        match discover_additional_codex_mcp_servers_with_cli(&command, cwd, &imported.servers) {
            Ok(cli_import) => {
                let command_display = Path::new(&command).display();
                if cli_import.report.is_empty() {
                    println!(
                        "Codex CLI MCP discovery: {command_display} (no additional MCP servers)"
                    );
                } else {
                    print_discovery_report(
                        &format!("Codex CLI MCP discovery: {command_display}"),
                        &cli_import.report,
                    );
                }
                for (name, spec) in cli_import.servers {
                    imported.servers.entry(name).or_insert(spec);
                }
            }
            Err(error) if cli.codex_cli => {
                return Err(format!(
                    "Codex CLI MCP discovery was required by --codex-cli, but failed: {error}"
                ));
            }
            Err(error) => {
                eprintln!(
                    "Warning: Codex CLI MCP discovery is unavailable ({error}). Continuing without CLI enrichment; only directly parsed config.toml MCP servers are available, so servers contributed by Codex plugins may be missing. Pass --codex-cli to make this a startup error."
                );
            }
        }
    }

    let (servers, overlay_report) = merge_mcp_servers(imported.servers, explicit);
    print_discovery_report("Codex MCP overrides:", &overlay_report);
    Ok(servers)
}

/// A fully-defaulted config for a given work directory, matching what
/// `load_config` produces from an empty config file. Handy for tests and for
/// embedding the server without a config file.
pub fn default_config(work_dir: std::path::PathBuf) -> AppConfig {
    AppConfig {
        work_dir,
        multi_project: false,
        project_catalog: ProjectCatalogConfig::default(),
        worktrees: default_worktree_config(),
        api_key: None,
        port: 3000,
        allowed_commands: default_allowed_commands(),
        tree: default_tree(),
        command: default_command(),
        exec: default_exec(),
        project_doc: ProjectDocConfig::default(),
        output: OutputConfig::default(),
        review: ReviewConfig::default(),
        artifact_ingress: ArtifactIngressConfig::default(),
        memory: MemoryConfig::default(),
        skills: SkillsConfig::default(),
        ignore: IgnoreConfig::default(),
        audit: AuditConfig::default(),
        allowed_hosts: Vec::new(),
        openai_tunnel: None,
        mcp_servers: HashMap::new(),
        generated_skills_dir: None,
    }
}

#[derive(Debug, Default)]
struct NativeWorktreeSettings {
    root: Option<PathBuf>,
    upstream_refresh_mode: Option<WorktreeUpstreamRefreshMode>,
    auto_cleanup_enabled: Option<bool>,
    keep_count: Option<usize>,
}

fn codex_home_path() -> PathBuf {
    codex_config_path()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .or_else(|| home_dir().map(|path| path.join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

fn default_worktree_config() -> WorktreeConfig {
    WorktreeConfig {
        mode: WorktreeMode::Auto,
        root: codex_home_path().join("worktrees"),
        upstream_refresh_mode: WorktreeUpstreamRefreshMode::Never,
        auto_cleanup_enabled: true,
        keep_count: 15,
        allow_setup_script: false,
    }
}

fn native_worktree_settings() -> NativeWorktreeSettings {
    let Ok(path) = codex_config_path() else {
        return NativeWorktreeSettings::default();
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return NativeWorktreeSettings::default();
        }
        Err(_) => {
            println!(
                "Codex worktree settings: could not read {} — using built-in defaults",
                path.display()
            );
            return NativeWorktreeSettings::default();
        }
    };
    let root: toml::Value = match toml::from_str(&raw) {
        Ok(root) => root,
        Err(_) => {
            println!(
                "Codex worktree settings: invalid TOML at {} — using built-in defaults",
                path.display()
            );
            return NativeWorktreeSettings::default();
        }
    };
    let Some(desktop) = root.get("desktop").and_then(toml::Value::as_table) else {
        return NativeWorktreeSettings::default();
    };

    let root = desktop
        .get("git-worktree-root")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(resolve_path);
    let upstream_refresh_mode = match desktop
        .get("worktree-upstream-refresh-mode")
        .and_then(toml::Value::as_str)
    {
        Some("best-effort") => Some(WorktreeUpstreamRefreshMode::BestEffort),
        Some("never") => Some(WorktreeUpstreamRefreshMode::Never),
        _ => None,
    };
    let auto_cleanup_enabled = desktop
        .get("worktree-auto-cleanup-enabled")
        .and_then(toml::Value::as_bool);
    let keep_count = desktop
        .get("worktree-keep-count")
        .and_then(toml::Value::as_integer)
        .and_then(|value| usize::try_from(value).ok());

    NativeWorktreeSettings {
        root,
        upstream_refresh_mode,
        auto_cleanup_enabled,
        keep_count,
    }
}

fn resolve_worktree_config(file: Option<PartialWorktrees>, cli: &Cli) -> WorktreeConfig {
    let native = native_worktree_settings();
    let file = file.unwrap_or_default();
    let mut config = default_worktree_config();
    config.mode = cli.worktree_mode.or(file.mode).unwrap_or_default();
    config.root = cli
        .worktree_root
        .as_deref()
        .map(resolve_path)
        .or_else(|| file.root.as_deref().map(resolve_path))
        .or(native.root)
        .unwrap_or(config.root);
    config.upstream_refresh_mode = file
        .upstream_refresh_mode
        .or(native.upstream_refresh_mode)
        .unwrap_or_default();
    config.auto_cleanup_enabled = file
        .auto_cleanup_enabled
        .or(native.auto_cleanup_enabled)
        .unwrap_or(true);
    config.keep_count = file.keep_count.or(native.keep_count).unwrap_or(15).max(1);
    // Setup-script execution is opt-in and has no native Codex-desktop
    // equivalent, so it is only ever enabled through explicit file config.
    config.allow_setup_script = file.allow_setup_script.unwrap_or(false);
    config
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

fn resolve_cli_work_dir(cli: &Cli) -> Result<PathBuf, String> {
    let raw = cli
        .work_dir
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "missing required --work-dir".to_string())?;
    let work_dir = resolve_work_dir(raw);
    match std::fs::metadata(&work_dir) {
        Ok(metadata) if metadata.is_dir() => Ok(work_dir),
        Ok(_) => Err(format!(
            "work-dir is not a directory: {}",
            work_dir.display()
        )),
        Err(_) => Err(format!("work-dir does not exist: {}", work_dir.display())),
    }
}

fn config_file_path(cli: &Cli) -> PathBuf {
    cli.config
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("codex.config.json"))
}

fn load_file_config(cli: &Cli, announce: bool) -> Result<FileConfig, String> {
    let config_path = config_file_path(cli);
    let display_path = if config_path.is_absolute() {
        config_path.clone()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(&config_path)
    };
    match std::fs::read_to_string(&config_path) {
        Ok(text) => {
            if announce {
                println!("Config: {}", display_path.display());
            }
            serde_json::from_str(&text)
                .map_err(|error| format!("invalid config file {}: {error}", config_path.display()))
        }
        Err(_) => {
            if announce {
                println!(
                    "Config: no file at {} — using built-in defaults (pass --config to point elsewhere)",
                    display_path.display()
                );
            }
            Ok(FileConfig::default())
        }
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
    validate_tunnel_id(&tunnel_id).map_err(|error| error.to_string())?;

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

fn resolve_audit(file: Option<PartialAudit>, cli: &Cli) -> Result<AuditConfig, String> {
    const MAX_COMMAND_PREVIEW_BYTES: usize = 16 * 1024;

    let file = file.unwrap_or_default();
    let log_file = cli
        .audit_log
        .as_deref()
        .or(file.log_file.as_deref())
        .map(resolve_path);
    let include_command_preview =
        cli.audit_command_preview || file.include_command_preview.unwrap_or(false);
    let command_preview_max_bytes = file
        .command_preview_max_bytes
        .unwrap_or_else(|| AuditConfig::default().command_preview_max_bytes);
    if !(1..=MAX_COMMAND_PREVIEW_BYTES).contains(&command_preview_max_bytes) {
        return Err(format!(
            "audit.commandPreviewMaxBytes must be between 1 and {MAX_COMMAND_PREVIEW_BYTES}"
        ));
    }
    if include_command_preview && log_file.is_none() {
        return Err("audit command previews require audit.logFile or --audit <FILE>".to_string());
    }

    let mut redact_env = file.redact_env.unwrap_or_default();
    redact_env.extend(cli.audit_redact_env.iter().cloned());
    for name in &redact_env {
        if !valid_env_name(name) {
            return Err(format!(
                "audit.redactEnv contains an invalid environment-variable name: {name}"
            ));
        }
    }
    redact_env.sort();
    redact_env.dedup();

    Ok(AuditConfig {
        log_file,
        include_command_preview,
        command_preview_max_bytes,
        redact_env,
    })
}

/// Load and merge config. Errors are returned as strings for the caller to
/// print and exit on, mirroring the TS which validates and `process.exit`s.
pub fn load_config(cli: Cli) -> Result<AppConfig, String> {
    let work_dir = resolve_cli_work_dir(&cli)?;
    let mut file = load_file_config(&cli, true)?;

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
        if let Some(idle) = e.idle_timeout_ms {
            exec.idle_timeout_ms = idle;
        }
    }

    let project_catalog = resolve_project_catalog(&mut file);
    let mcp_servers = resolve_mcp_servers(&mut file, &cli)?;
    let openai_tunnel = resolve_openai_tunnel(file.openai_tunnel, &cli)?;
    let audit = resolve_audit(file.audit, &cli)?;
    let worktrees = resolve_worktree_config(file.worktrees.take(), &cli);
    let api_key = cli.api_key.or(file.api_key);
    if api_key.is_some() && openai_tunnel.is_some() {
        return Err(
            "apiKey/--api-key cannot be combined with openaiTunnel: native tunnel mode generates a private per-process bearer for the loopback MCP hop"
                .into(),
        );
    }

    let artifact_ingress = file.artifact_ingress.unwrap_or_default();
    artifact_ingress.validate()?;

    Ok(AppConfig {
        work_dir,
        multi_project: cli.multi_project || file.multi_project.unwrap_or(false),
        project_catalog,
        worktrees,
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
        review: file.review.unwrap_or_default(),
        artifact_ingress,
        memory: file.memory.unwrap_or_default(),
        skills: file.skills.unwrap_or_default(),
        ignore: file.ignore.unwrap_or_default(),
        audit,
        allowed_hosts: file.allowed_hosts.unwrap_or_default(),
        openai_tunnel,
        mcp_servers,
        generated_skills_dir: None,
    })
}

pub fn load_project_catalog_for_cli(cli: &Cli) -> Result<ProjectCatalog, String> {
    let work_dir = resolve_cli_work_dir(cli)?;
    let mut file = load_file_config(cli, false)?;
    let project_catalog = resolve_project_catalog(&mut file);
    let codex_path = if project_catalog.codex_config.enabled {
        Some(codex_config_path()?)
    } else {
        None
    };
    discover_project_catalog_at(&work_dir, &project_catalog, codex_path.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            verbose: 0,
            command: None,
            work_dir: Some(work_dir.to_string_lossy().into_owned()),
            multi_project: false,
            worktree_mode: None,
            worktree_root: None,
            port: None,
            api_key: None,
            config: Some(config.to_string_lossy().into_owned()),
            codex_cli: false,
            audit_log: None,
            audit_command_preview: false,
            audit_redact_env: Vec::new(),
            openai_tunnel_id: None,
            openai_tunnel_api_key_ref: None,
            openai_tunnel_client: None,
            openai_tunnel_organization_id: None,
        }
    }

    #[test]
    fn cli_parses_verbose_and_audit_controls() {
        let parsed = Cli::try_parse_from([
            "codexify",
            "-v",
            "--log-tool-calls",
            "--work-dir",
            "/tmp/project",
            "--audit-log",
            "/tmp/audit.jsonl",
            "--audit-command-preview",
            "--audit-redact-env",
            "GITHUB_TOKEN",
            "--audit-redact-env",
            "CUSTOM_SECRET",
        ])
        .unwrap();

        assert_eq!(parsed.verbose, 2);
        assert_eq!(parsed.audit_log.as_deref(), Some("/tmp/audit.jsonl"));
        assert!(parsed.audit_command_preview);
        assert_eq!(parsed.audit_redact_env, ["GITHUB_TOKEN", "CUSTOM_SECRET"]);
    }

    #[test]
    fn cli_audit_settings_override_and_extend_file_settings() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.json");
        let file_log = root.path().join("from-file.jsonl");
        let cli_log = root.path().join("from-cli.jsonl");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&json!({
                "codexMcp": { "enabled": false },
                "audit": {
                    "logFile": file_log,
                    "includeCommandPreview": false,
                    "commandPreviewMaxBytes": 1024,
                    "redactEnv": ["FILE_SECRET"]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let mut args = cli(root.path(), &config_path);
        args.audit_log = Some(cli_log.to_string_lossy().into_owned());
        args.audit_command_preview = true;
        args.audit_redact_env = vec!["CLI_SECRET".to_string(), "FILE_SECRET".to_string()];

        let config = load_config(args).unwrap();
        assert_eq!(config.audit.log_file.as_deref(), Some(cli_log.as_path()));
        assert!(config.audit.include_command_preview);
        assert_eq!(config.audit.command_preview_max_bytes, 1024);
        assert_eq!(config.audit.redact_env, ["CLI_SECRET", "FILE_SECRET"]);
    }

    #[test]
    fn command_previews_require_an_audit_log() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.json");
        std::fs::write(
            &config_path,
            r#"{"codexMcp":{"enabled":false},"audit":{"includeCommandPreview":true}}"#,
        )
        .unwrap();

        let error = load_config(cli(root.path(), &config_path)).unwrap_err();
        assert!(error.contains("require audit.logFile"));
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
                "codexMcp": {
                    "enabled": false,
                    "useCli": false,
                    "cliPath": "/opt/codex/bin/codex"
                },
                "mcpServers": {
                    "demo": {
                        "mode": "gateway",
                        "disabledTools": ["write"]
                    }
                }
            }"#,
        )
        .unwrap();

        let codex_mcp = file.codex_mcp.as_ref().unwrap();
        assert!(!codex_mcp.enabled());
        assert!(!codex_mcp.use_cli());
        assert_eq!(codex_mcp.cli_path.as_deref(), Some("/opt/codex/bin/codex"));
        let demo = file.mcp_servers.as_ref().unwrap().get("demo").unwrap();
        assert_eq!(demo.mode.as_deref(), Some("gateway"));
        assert_eq!(
            demo.disabled_tools.as_deref(),
            Some(&["write".to_string()][..])
        );
        assert!(demo.command.is_none());
    }

    #[test]
    fn codex_cli_flag_marks_cli_discovery_as_required() {
        let parsed = Cli::try_parse_from(["codexify", "--work-dir", ".", "--codex-cli"]).unwrap();
        assert!(parsed.codex_cli);
    }

    #[test]
    fn automatic_cli_failure_falls_back_but_explicit_flag_errors() {
        let root = tempfile::tempdir().unwrap();
        let codex_config = root.path().join("config.toml");
        std::fs::write(
            &codex_config,
            "[mcp_servers.global]\ncommand = \"global-server\"\n",
        )
        .unwrap();
        let missing_cli = root.path().join("missing-codex-cli").into_os_string();

        let mut automatic_file = FileConfig {
            codex_mcp: Some(CodexMcpConfig {
                enabled: Some(true),
                use_cli: Some(true),
                cli_path: None,
            }),
            ..Default::default()
        };
        let automatic = resolve_mcp_servers_from(
            &mut automatic_file,
            &cli(root.path(), &root.path().join("codex.config.json")),
            Ok(codex_config.clone()),
            Some(missing_cli.clone()),
        )
        .unwrap();
        assert_eq!(
            automatic
                .get("global")
                .and_then(|server| server.command.as_deref()),
            Some("global-server")
        );

        let mut required_file = FileConfig {
            codex_mcp: Some(CodexMcpConfig {
                enabled: Some(false),
                use_cli: Some(false),
                cli_path: None,
            }),
            ..Default::default()
        };
        let mut required_cli = cli(root.path(), &root.path().join("codex.config.json"));
        required_cli.codex_cli = true;
        let error = resolve_mcp_servers_from(
            &mut required_file,
            &required_cli,
            Ok(codex_config),
            Some(missing_cli),
        )
        .unwrap_err();
        assert!(error.contains("required by --codex-cli"));
        assert!(error.contains("was not found"));
    }

    #[test]
    fn use_cli_false_keeps_direct_import_without_starting_the_cli() {
        let root = tempfile::tempdir().unwrap();
        let codex_config = root.path().join("config.toml");
        std::fs::write(
            &codex_config,
            "[mcp_servers.global]\ncommand = \"global-server\"\n",
        )
        .unwrap();
        let mut file = FileConfig {
            codex_mcp: Some(CodexMcpConfig {
                enabled: Some(true),
                use_cli: Some(false),
                cli_path: None,
            }),
            ..Default::default()
        };

        let servers = resolve_mcp_servers_from(
            &mut file,
            &cli(root.path(), &root.path().join("codex.config.json")),
            Ok(codex_config),
            Some(root.path().join("missing-codex-cli").into_os_string()),
        )
        .unwrap();

        assert_eq!(
            servers
                .get("global")
                .and_then(|server| server.command.as_deref()),
            Some("global-server")
        );
    }

    #[test]
    fn project_catalog_config_is_independent_from_codex_mcp_discovery() {
        let mut file: FileConfig = serde_json::from_str(
            r#"{
                "codexMcp": { "enabled": false },
                "projectCatalog": {
                    "codexConfig": {
                        "enabled": true,
                        "trustedOnly": false
                    },
                    "entries": [
                        {
                            "path": "codexify",
                            "name": "Codexify",
                            "aliases": ["bridge"],
                            "description": "Rust MCP bridge"
                        }
                    ]
                }
            }"#,
        )
        .unwrap();

        assert!(!file.codex_mcp.as_ref().unwrap().enabled());
        let catalog = resolve_project_catalog(&mut file);
        assert!(catalog.codex_config.enabled);
        assert!(!catalog.codex_config.trusted_only);
        assert_eq!(catalog.entries.len(), 1);
        assert_eq!(catalog.entries[0].path.as_deref(), Some("codexify"));
        assert_eq!(catalog.entries[0].aliases, ["bridge"]);
    }

    #[test]
    fn projects_list_cli_accepts_global_options_after_the_subcommand() {
        let parsed = Cli::try_parse_from([
            "codexify",
            "projects",
            "list",
            "--work-dir",
            "/tmp/projects",
            "--config",
            "/tmp/config.json",
            "--query",
            "bridge",
            "--json",
        ])
        .unwrap();

        assert_eq!(parsed.work_dir.as_deref(), Some("/tmp/projects"));
        assert_eq!(parsed.config.as_deref(), Some("/tmp/config.json"));
        let Some(CliCommand::Projects {
            command: ProjectsCommand::List(args),
        }) = parsed.command
        else {
            panic!("projects list subcommand was not parsed");
        };
        assert_eq!(args.query.as_deref(), Some("bridge"));
        assert!(args.json);
    }

    #[test]
    fn existing_server_cli_syntax_remains_valid() {
        let parsed = Cli::try_parse_from([
            "codexify",
            "--work-dir",
            "/tmp/project",
            "--multi-project",
            "--port",
            "4000",
        ])
        .unwrap();

        assert!(parsed.command.is_none());
        assert_eq!(parsed.work_dir.as_deref(), Some("/tmp/project"));
        assert!(parsed.multi_project);
        assert_eq!(parsed.port, Some(4000));
    }

    #[test]
    fn catalogue_cli_loading_does_not_validate_unrelated_tunnel_settings() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config_path = root.path().join("config.json");
        std::fs::write(
            &config_path,
            r#"{
                "openaiTunnel": {
                    "tunnelId": "not-a-valid-tunnel-id",
                    "apiKeyRef": "literal-secret"
                },
                "projectCatalog": {
                    "codexConfig": { "enabled": false },
                    "entries": [{ "path": "project" }]
                }
            }"#,
        )
        .unwrap();

        let catalog = load_project_catalog_for_cli(&cli(root.path(), &config_path)).unwrap();
        assert_eq!(catalog.projects.len(), 1);
        assert_eq!(catalog.projects[0].selector, "project");
    }

    #[test]
    fn review_config_accepts_an_empty_block_and_camel_case_override() {
        let empty: FileConfig = serde_json::from_str(r#"{"review":{}}"#).unwrap();
        assert_eq!(
            empty.review.unwrap().max_patch_bytes,
            crate::types::DEFAULT_REVIEW_MAX_PATCH_BYTES
        );

        let configured: FileConfig =
            serde_json::from_str(r#"{"review":{"maxPatchBytes":1234}}"#).unwrap();
        assert_eq!(configured.review.unwrap().max_patch_bytes, 1234);
    }

    #[test]
    fn artifact_ingress_config_accepts_defaults_and_camel_case_overrides() {
        let empty: FileConfig = serde_json::from_str(r#"{"artifactIngress":{}}"#).unwrap();
        let empty = empty.artifact_ingress.unwrap();
        assert!(empty.enabled);
        assert_eq!(
            empty.max_file_bytes,
            crate::types::DEFAULT_ARTIFACT_MAX_FILE_BYTES
        );

        let configured: FileConfig = serde_json::from_str(
            r#"{
                "artifactIngress": {
                    "enabled": false,
                    "maxFileBytes": 4096,
                    "requestTimeoutMs": 5000,
                    "idleTimeoutMs": 1000,
                    "maxRedirects": 1,
                    "maxConcurrentDownloads": 4
                }
            }"#,
        )
        .unwrap();
        let configured = configured.artifact_ingress.unwrap();
        assert!(!configured.enabled);
        assert_eq!(configured.max_file_bytes, 4096);
        assert_eq!(configured.request_timeout_ms, 5000);
        assert_eq!(configured.idle_timeout_ms, 1000);
        assert_eq!(configured.max_redirects, 1);
        assert_eq!(configured.max_concurrent_downloads, 4);
    }

    #[test]
    fn artifact_ingress_config_rejects_unsafe_limits() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.json");
        for (json, expected) in [
            (
                r#"{"artifactIngress":{"maxFileBytes":0}}"#,
                "maxFileBytes must be positive",
            ),
            (
                r#"{"artifactIngress":{"requestTimeoutMs":0}}"#,
                "requestTimeoutMs must be positive",
            ),
            (
                r#"{"artifactIngress":{"requestTimeoutMs":100,"idleTimeoutMs":101}}"#,
                "idleTimeoutMs",
            ),
            (r#"{"artifactIngress":{"maxRedirects":11}}"#, "maxRedirects"),
            (
                r#"{"artifactIngress":{"maxConcurrentDownloads":0}}"#,
                "maxConcurrentDownloads",
            ),
        ] {
            std::fs::write(&config_path, json).unwrap();
            let error = load_config(cli(root.path(), &config_path)).unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn loads_native_tunnel_with_a_secret_reference_default() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.json");
        std::fs::write(
            &config_path,
            r#"{"codexMcp":{"useCli":false},"openaiTunnel":{"tunnelId":"tunnel_0123456789abcdef0123456789abcdef"}}"#,
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
            r#"{"codexMcp":{"useCli":false},"openaiTunnel":{"tunnelId":"tunnel_0123456789abcdef0123456789abcdef","apiKeyRef":"sk-literal-secret-value"}}"#,
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
            r#"{"codexMcp":{"useCli":false},"apiKey":"local-token","openaiTunnel":{"tunnelId":"tunnel_0123456789abcdef0123456789abcdef"}}"#,
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
            r#"{"codexMcp":{"useCli":false},"openaiTunnel":{"tunnelId":"tunnel_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","apiKeyRef":"env:OLD_KEY"}}"#,
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
            r#"{"codexMcp":{"useCli":false},"openaiTunnel":{"tunnelId":"tunnel_NOT_HEX"}}"#,
        )
        .unwrap();

        let error = load_config(cli(root.path(), &config_path)).unwrap_err();
        assert!(error.contains("32 lowercase letters or digits"));

        std::fs::write(
            &config_path,
            r#"{"codexMcp":{"useCli":false},"openaiTunnel":{"tunnelId":"tunnel_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}}"#,
        )
        .unwrap();
        let error = load_config(cli(root.path(), &config_path)).unwrap_err();
        assert!(error.contains("32 lowercase letters or digits"));

        std::fs::write(
            &config_path,
            r#"{"codexMcp":{"useCli":false},"openaiTunnel":{"tunnelId":"tunnel_0123456789abcdefghijklmnopqrstuv"}}"#,
        )
        .unwrap();
        assert!(load_config(cli(root.path(), &config_path)).is_ok());
    }
}
