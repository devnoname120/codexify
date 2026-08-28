//! Shared configuration and result types.
//!
//! Ports the interfaces from the TypeScript `src/types.ts`. Config field names
//! are kept camelCase on the wire (`allowedCommands`, `maxSessions`, …) via
//! serde renames so an existing `codex.config.json` keeps parsing unchanged.

use std::fmt;
use std::ops::Deref;

use rmcp::model::{MetaObject, Resource};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroize;

// ─── Tool results ──────────────────────────────────────────────────────

/// A single MCP content block a tool may return. Text-only tools (the
/// majority) use [`ToolContent::Text`]; `view_image` uses [`ToolContent::Image`].
#[derive(Debug, Clone)]
pub enum ToolContent {
    Text(String),
    Image { data: String, mime_type: String },
    ResourceLink(Resource),
}

/// Metadata retained for operational accounting without retaining tool output.
#[derive(Debug, Clone, Default)]
pub struct ToolAuditMetadata {
    pub truncated: Option<bool>,
    pub original_output_tokens: Option<u64>,
    pub exec_session_id: Option<u64>,
    pub process_id: Option<u32>,
    pub resident: Option<bool>,
}

/// What a tool hands back. Mirrors the repo's `ToolResult`: a list of content
/// blocks, an error flag, and an optional machine-readable form that matches the
/// tool's `outputSchema`.
#[derive(Debug, Clone, Default)]
pub struct ToolResult {
    pub content: Vec<ToolContent>,
    pub is_error: bool,
    pub structured_content: Option<Value>,
    pub meta: Option<MetaObject>,
    pub audit: ToolAuditMetadata,
}

impl ToolResult {
    /// A successful text result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::Text(text.into())],
            is_error: false,
            structured_content: None,
            meta: None,
            audit: ToolAuditMetadata::default(),
        }
    }

    /// An error result carrying a caller-visible message (`isError: true`).
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::Text(text.into())],
            is_error: true,
            structured_content: None,
            meta: None,
            audit: ToolAuditMetadata::default(),
        }
    }

    /// A single image content block.
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::Image {
                data: data.into(),
                mime_type: mime_type.into(),
            }],
            is_error: false,
            structured_content: None,
            meta: None,
            audit: ToolAuditMetadata::default(),
        }
    }

    /// Attach the machine-readable form matching the tool's output schema.
    pub fn with_structured(mut self, value: Value) -> Self {
        self.structured_content = Some(value);
        self
    }

    pub fn with_truncation(mut self, truncated: bool) -> Self {
        self.audit.truncated = Some(truncated);
        self
    }

    /// Concatenate the text blocks with newlines, as the server does when
    /// filling in the default `structuredContent`.
    pub fn joined_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                ToolContent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ─── Plan state ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

impl PlanStepStatus {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanItem {
    pub step: String,
    pub status: PlanStepStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanState {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub explanation: Option<String>,
    pub plan: Vec<PlanItem>,
}

// ─── Config ────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Eq)]
pub struct ConversationAuthToken(String);

impl From<String> for ConversationAuthToken {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ConversationAuthToken {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl Deref for ConversationAuthToken {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for ConversationAuthToken {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ConversationAuthToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl Drop for ConversationAuthToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecMode {
    Allowlist,
    Unrestricted,
}

/// Policy applied to `exec_command`. Every field has a default so a partial
/// config JSON still parses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecConfig {
    pub mode: ExecMode,
    pub extra_allowed_commands: Vec<String>,
    pub max_sessions: usize,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub default_shell: Option<String>,
    /// Milliseconds a resident exec_command session may sit idle (no
    /// `write_stdin` / output yield) before it is killed and reaped. `0`
    /// disables the idle reaper. Guards against abandoned sessions leaking
    /// processes for the lifetime of a long-lived MCP transport.
    pub idle_timeout_ms: u64,
}

/// Governs `AGENTS.md` discovery. Every field is optional; `project_doc.rs`
/// owns the defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDocConfig {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fallback_filenames: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub root_markers: Option<Vec<String>>,
}

/// Working memory. Every field is optional; `memory.rs` owns the defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryConfig {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_bytes: Option<usize>,
}

pub const DEFAULT_REVIEW_MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;

/// Bounds the component-only patch payload without changing checkpoint semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewConfig {
    #[serde(default = "default_review_max_patch_bytes")]
    pub max_patch_bytes: usize,
}

fn default_review_max_patch_bytes() -> usize {
    DEFAULT_REVIEW_MAX_PATCH_BYTES
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            max_patch_bytes: DEFAULT_REVIEW_MAX_PATCH_BYTES,
        }
    }
}

pub const DEFAULT_ARTIFACT_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
pub const DEFAULT_ARTIFACT_REQUEST_TIMEOUT_MS: u64 = 120_000;
pub const DEFAULT_ARTIFACT_IDLE_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_ARTIFACT_MAX_REDIRECTS: u8 = 3;
pub const DEFAULT_ARTIFACT_MAX_CONCURRENT_DOWNLOADS: usize = 2;

pub const DEFAULT_ARTIFACT_EGRESS_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
pub const DEFAULT_ARTIFACT_EGRESS_MAX_CACHED_BYTES: u64 = 256 * 1024 * 1024;
pub const DEFAULT_ARTIFACT_EGRESS_MAX_REFERENCES: usize = 64;
pub const DEFAULT_ARTIFACT_EGRESS_REFERENCE_TTL_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ArtifactIngressConfig {
    pub enabled: bool,
    pub max_file_bytes: u64,
    pub request_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub max_redirects: u8,
    pub max_concurrent_downloads: usize,
    /// Host patterns the download URL (and every redirect hop) must match.
    /// `"*"` is a wildcard that accepts any public host over HTTPS while still
    /// rejecting internal targets (loopback, private, link-local, ULA,
    /// `localhost`, and the cloud metadata address). A bare host (`files.example.com`)
    /// matches that host exactly; a leading dot (`.example.com`) matches the host
    /// and any subdomain. A host listed explicitly (not via `"*"`) is trusted as
    /// given, so an internal host can be allowed only by naming it here.
    pub allowed_hosts: Vec<String>,
}

impl Default for ArtifactIngressConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_file_bytes: DEFAULT_ARTIFACT_MAX_FILE_BYTES,
            request_timeout_ms: DEFAULT_ARTIFACT_REQUEST_TIMEOUT_MS,
            idle_timeout_ms: DEFAULT_ARTIFACT_IDLE_TIMEOUT_MS,
            max_redirects: DEFAULT_ARTIFACT_MAX_REDIRECTS,
            max_concurrent_downloads: DEFAULT_ARTIFACT_MAX_CONCURRENT_DOWNLOADS,
            allowed_hosts: vec!["*".to_string()],
        }
    }
}

impl ArtifactIngressConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_file_bytes == 0 {
            return Err("artifactIngress.maxFileBytes must be positive".to_string());
        }
        if self.request_timeout_ms == 0 {
            return Err("artifactIngress.requestTimeoutMs must be positive".to_string());
        }
        if self.idle_timeout_ms == 0 || self.idle_timeout_ms > self.request_timeout_ms {
            return Err(
                "artifactIngress.idleTimeoutMs must be positive and no greater than requestTimeoutMs"
                    .to_string(),
            );
        }
        if self.max_redirects > 10 {
            return Err("artifactIngress.maxRedirects must be between 0 and 10".to_string());
        }
        if !(1..=16).contains(&self.max_concurrent_downloads) {
            return Err(
                "artifactIngress.maxConcurrentDownloads must be between 1 and 16".to_string(),
            );
        }
        if self.allowed_hosts.is_empty() {
            return Err(
                "artifactIngress.allowedHosts must list at least one host pattern (use \"*\" to allow any public host)"
                    .to_string(),
            );
        }
        for pattern in &self.allowed_hosts {
            let host = pattern.strip_prefix('.').unwrap_or(pattern);
            if host.is_empty()
                || (pattern != "*"
                    && host
                        .chars()
                        .any(|character| character.is_whitespace() || character.is_control()))
            {
                return Err(format!(
                    "artifactIngress.allowedHosts contains an invalid host pattern: {pattern:?}"
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ArtifactEgressConfig {
    pub enabled: bool,
    pub max_file_bytes: u64,
    pub max_cached_bytes: u64,
    pub max_references: usize,
    pub reference_ttl_ms: u64,
}

impl Default for ArtifactEgressConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_file_bytes: DEFAULT_ARTIFACT_EGRESS_MAX_FILE_BYTES,
            max_cached_bytes: DEFAULT_ARTIFACT_EGRESS_MAX_CACHED_BYTES,
            max_references: DEFAULT_ARTIFACT_EGRESS_MAX_REFERENCES,
            reference_ttl_ms: DEFAULT_ARTIFACT_EGRESS_REFERENCE_TTL_MS,
        }
    }
}

impl ArtifactEgressConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_file_bytes == 0 {
            return Err("artifactEgress.maxFileBytes must be positive".to_string());
        }
        if self.max_cached_bytes < self.max_file_bytes {
            return Err("artifactEgress.maxCachedBytes must be at least maxFileBytes".to_string());
        }
        if !(1..=1024).contains(&self.max_references) {
            return Err("artifactEgress.maxReferences must be between 1 and 1024".to_string());
        }
        if self.reference_ttl_ms == 0 {
            return Err("artifactEgress.referenceTtlMs must be positive".to_string());
        }
        Ok(())
    }
}

/// Governs `SKILL.md` discovery. Every field is optional; `skills.rs` owns the
/// defaults and search order.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillsConfig {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub dirs: Option<Vec<String>>,
    /// Also discover skills bundled with installed Claude Code plugins
    /// (`~/.claude/plugins/cache/.../skills/*`). Default true; set false to
    /// expose only the standalone skill directories.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub include_plugins: Option<bool>,
}

/// Governs what the file-walking tools skip. Every field is optional; the
/// implementing module owns the defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IgnoreConfig {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub use_gitignore: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub use_default_patterns: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub custom_patterns: Option<Vec<String>>,
}

/// Ceilings on what one tool call may return. Every field is optional;
/// `output_budget.rs` owns the defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputConfig {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_tool_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_file_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_file_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_entries: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_tree_nodes: Option<usize>,
}

impl OutputConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_tool_output_tokens == Some(0) {
            return Err("output.maxToolOutputTokens must be positive".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeConfig {
    pub default_depth: usize,
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandConfig {
    pub default_timeout: u64,
    pub max_timeout: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpToolExposure {
    Direct,
    Gateway,
    Catalog,
}

impl McpToolExposure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Gateway => "gateway",
            Self::Catalog => "catalog",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum McpServerProvenance {
    #[default]
    Explicit,
    CodexConfig,
    CodexCli,
}

impl McpServerProvenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::CodexConfig => "codex-config",
            Self::CodexCli => "codex-cli",
        }
    }

    pub fn default_exposure(self) -> McpToolExposure {
        match self {
            Self::Explicit => McpToolExposure::Direct,
            Self::CodexConfig | Self::CodexCli => McpToolExposure::Catalog,
        }
    }
}

/// One upstream MCP server to bridge, in the standard `mcpServers` shape. Its
/// tools are discovered at startup and materialized according to the effective
/// exposure mode. A `command` selects stdio; a `url` selects Codex-compatible
/// Streamable HTTP. Legacy SSE and WebSocket transports are rejected explicitly.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerSpec {
    /// The executable to launch (e.g. `idasql`, `npx`, `python`). Absent for
    /// Streamable HTTP servers.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Working directory for the launched stdio server. When absent, the child
    /// inherits codexify's process working directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Skip this server without removing it from the config.
    #[serde(default)]
    pub disabled: bool,
    /// Transport type. `"stdio"` is inferred from `command`; `"http"`,
    /// `"streamable-http"`, and `"streamable_http"` are equivalent when `url`
    /// is present.
    #[serde(rename = "type", default)]
    pub transport: Option<String>,
    /// URL for a Streamable HTTP server.
    #[serde(default)]
    pub url: Option<String>,
    /// Environment variable containing a bearer token for Streamable HTTP.
    #[serde(default)]
    pub bearer_token_env_var: Option<String>,
    /// Static headers sent with every Streamable HTTP request.
    #[serde(default)]
    pub http_headers: std::collections::HashMap<String, String>,
    /// Header-name to environment-variable mappings resolved at connection time.
    #[serde(default)]
    pub env_http_headers: std::collections::HashMap<String, String>,
    /// Timeout for initialization and the first tools listing, in seconds.
    #[serde(default)]
    pub startup_timeout_sec: Option<f64>,
    /// Timeout for each forwarded tool call, in seconds.
    #[serde(default)]
    pub tool_timeout_sec: Option<f64>,
    /// If set, only these upstream tool names are bridged (an allow-list on the
    /// upstream's own names, e.g. `["exec", "machine_list"]`). Use it to keep the
    /// exposed tool count small — LLM clients work better with fewer tools and
    /// some (including ChatGPT) cap how many a connector may expose.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// Upstream tool names removed after applying `tools`, matching Codex's
    /// `disabled_tools` semantics.
    #[serde(default)]
    pub disabled_tools: Option<Vec<String>>,
    /// How the upstream's tools are exposed:
    /// - `"direct"`: each upstream tool becomes its own `<server>__<tool>`.
    /// - `"gateway"`: the whole server collapses into ONE dispatcher tool named
    ///   `<server>` taking `{function, arguments}`, plus an auto-generated skill
    ///   documenting every function.
    /// - `"catalog"`: tools remain private and are reached through the fixed MCP
    ///   source/search/schema/call surface.
    ///
    /// When absent, explicit `mcpServers` entries retain the historical direct
    /// behavior, while automatically imported Codex/config/plugin entries use
    /// catalog mode.
    #[serde(default)]
    pub mode: Option<McpToolExposure>,
    /// Internal origin used only to select the backward-compatible default mode.
    #[serde(skip)]
    pub provenance: McpServerProvenance,
}

impl McpServerSpec {
    pub fn exposure(&self) -> McpToolExposure {
        self.mode
            .unwrap_or_else(|| self.provenance.default_exposure())
    }
}

/// Configuration for OpenAI's outbound Secure MCP Tunnel runtime.
#[derive(Debug, Clone)]
pub struct OpenAiTunnelConfig {
    pub tunnel_id: String,
    /// A secret reference accepted by tunnel-client, never a literal API key.
    pub api_key_ref: String,
    pub organization_id: Option<String>,
    /// An explicit full or runtime-only tunnel-client binary. When absent,
    /// Codexify installs and verifies its pinned runtime-only build.
    pub client_path: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone)]
pub struct CodexProjectCatalogConfig {
    pub enabled: bool,
    pub trusted_only: bool,
}

impl Default for CodexProjectCatalogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trusted_only: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
#[derive(Default)]
pub enum WorktreeMode {
    #[default]
    Auto,
    Always,
    Never,
}

impl WorktreeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum WorktreeUpstreamRefreshMode {
    #[default]
    Never,
    BestEffort,
}

impl WorktreeUpstreamRefreshMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::BestEffort => "best-effort",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuditConfig {
    pub log_file: Option<std::path::PathBuf>,
    pub include_command_preview: bool,
    pub command_preview_max_bytes: usize,
    pub redact_env: Vec<String>,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            log_file: None,
            include_command_preview: false,
            command_preview_max_bytes: 512,
            redact_env: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProjectCatalogEntryConfig {
    pub path: Option<String>,
    pub name: Option<String>,
    pub aliases: Vec<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectCatalogConfig {
    pub codex_config: CodexProjectCatalogConfig,
    pub entries: Vec<ProjectCatalogEntryConfig>,
}

#[derive(Debug, Clone)]
pub struct WorktreeConfig {
    pub mode: WorktreeMode,
    pub root: std::path::PathBuf,
    pub upstream_refresh_mode: WorktreeUpstreamRefreshMode,
    pub auto_cleanup_enabled: bool,
    pub keep_count: usize,
    /// Whether a per-worktree setup script (declared through the bound
    /// environment's `environment.toml`) may be executed when a managed
    /// worktree is created. This runs an arbitrary command *outside* the
    /// `allowedCommands`/exec policy, and the environment path is itself
    /// selectable through local git config, so an untrusted repository could
    /// otherwise plant a script that runs on the next binding. It is therefore
    /// opt-in and defaults to `false`; leave it off unless every project that
    /// can reach this server is trusted to run arbitrary setup commands.
    pub allow_setup_script: bool,
}

/// The fully-resolved server configuration handed to every tool.
///
/// `work_dir` and `port` are always concrete. `project_catalog`, `projectDoc`,
/// `output`, `review`, `artifactIngress`, `artifactEgress`, `memory`, `skills`, `ignore` and
/// `audit` carry their resolved/defaultable module settings.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub work_dir: std::path::PathBuf,
    pub multi_project: bool,
    pub project_clone_dir: std::path::PathBuf,
    pub project_catalog: ProjectCatalogConfig,
    pub worktrees: WorktreeConfig,
    pub api_key: Option<String>,
    pub conversation_auth_token: Option<ConversationAuthToken>,
    pub port: u16,
    pub allowed_commands: Vec<String>,
    pub tree: TreeConfig,
    pub command: CommandConfig,
    pub exec: ExecConfig,
    pub project_doc: ProjectDocConfig,
    pub output: OutputConfig,
    pub review: ReviewConfig,
    pub artifact_ingress: ArtifactIngressConfig,
    pub artifact_egress: ArtifactEgressConfig,
    pub memory: MemoryConfig,
    pub skills: SkillsConfig,
    pub ignore: IgnoreConfig,
    pub audit: AuditConfig,
    /// Host authorities accepted for DNS-rebinding protection. Empty means
    /// "accept any Host", which the original bridge did so it works behind a
    /// tunnel that presents an arbitrary hostname.
    pub allowed_hosts: Vec<String>,
    /// OpenAI's outbound tunnel, when enabled. The HTTP listener is restricted
    /// to loopback and its permissive browser CORS layer is disabled in this mode.
    pub openai_tunnel: Option<OpenAiTunnelConfig>,
    /// Upstream MCP servers, keyed by their raw configured names. Their tools are
    /// discovered at startup and exposed according to each server's effective mode.
    pub mcp_servers: std::collections::HashMap<String, McpServerSpec>,
    /// Directory where gateway-mode servers write their auto-generated SKILL.md,
    /// added to skill discovery. Set at startup, not from the config file.
    pub generated_skills_dir: Option<std::path::PathBuf>,
}
