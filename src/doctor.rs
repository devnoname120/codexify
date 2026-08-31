use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::config::{
    Cli, CodexCliDiagnosticConfig, ConfigPathSelection, ConfigPathSource,
    codex_cli_diagnostic_config, config_path_selection, load_config_quiet,
};
use crate::exec_sessions::resolve_shell;
use crate::openai_tunnel::{self, TunnelRuntimeInspection};
use crate::self_update::{LatestVersionInspection, LatestVersionStatus};
use crate::service;
use crate::types::AppConfig;

use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::timeout;

const COMMAND_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const HEALTH_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Pass,
    Warning,
    Failure,
    Skipped,
}

impl DoctorStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warning => "WARN",
            Self::Failure => "FAIL",
            Self::Skipped => "SKIP",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorCheck {
    pub id: String,
    pub status: DoctorStatus,
    pub summary: String,
    pub detail: Option<String>,
    pub remediation: Option<String>,
}

impl DoctorCheck {
    fn new(id: impl Into<String>, status: DoctorStatus, summary: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status,
            summary: summary.into(),
            detail: None,
            remediation: None,
        }
    }

    pub fn pass(id: impl Into<String>, summary: impl Into<String>) -> Self {
        Self::new(id, DoctorStatus::Pass, summary)
    }

    pub fn warning(id: impl Into<String>, summary: impl Into<String>) -> Self {
        Self::new(id, DoctorStatus::Warning, summary)
    }

    pub fn failure(id: impl Into<String>, summary: impl Into<String>) -> Self {
        Self::new(id, DoctorStatus::Failure, summary)
    }

    pub fn skipped(id: impl Into<String>, summary: impl Into<String>) -> Self {
        Self::new(id, DoctorStatus::Skipped, summary)
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_remediation(mut self, remediation: impl Into<String>) -> Self {
        self.remediation = Some(remediation.into());
        self
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DoctorSummary {
    pub passed: usize,
    pub warnings: usize,
    pub failures: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorPlatform {
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub version: String,
    pub platform: DoctorPlatform,
    pub checks: Vec<DoctorCheck>,
    pub summary: DoctorSummary,
}

impl DoctorReport {
    pub fn new(checks: Vec<DoctorCheck>) -> Self {
        let mut summary = DoctorSummary::default();
        for check in &checks {
            match check.status {
                DoctorStatus::Pass => summary.passed += 1,
                DoctorStatus::Warning => summary.warnings += 1,
                DoctorStatus::Failure => summary.failures += 1,
                DoctorStatus::Skipped => summary.skipped += 1,
            }
        }

        Self {
            ok: summary.failures == 0,
            version: env!("CARGO_PKG_VERSION").to_string(),
            platform: DoctorPlatform {
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
            },
            checks,
            summary,
        }
    }

    pub fn render_human(&self) -> String {
        let mut output = String::new();
        let _ = writeln!(output, "Codexify doctor {}", self.version);
        let _ = writeln!(
            output,
            "Platform: {}/{}",
            self.platform.os, self.platform.arch
        );
        output.push('\n');

        for check in &self.checks {
            let _ = writeln!(
                output,
                "{} {}: {}",
                check.status.label(),
                check.id,
                check.summary
            );
            if let Some(detail) = &check.detail {
                let _ = writeln!(output, "  {detail}");
            }
            if let Some(remediation) = &check.remediation {
                let _ = writeln!(output, "  Remediation: {remediation}");
            }
        }

        let result = if self.ok { "healthy" } else { "unhealthy" };
        let _ = write!(
            output,
            "\nResult: {result} ({} passed, {} warnings, {} failures, {} skipped)\n",
            self.summary.passed, self.summary.warnings, self.summary.failures, self.summary.skipped
        );
        output
    }
}

fn runtime_check() -> DoctorCheck {
    let path = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            return DoctorCheck::failure("runtime", "Running executable could not be located")
                .with_detail(error.to_string());
        }
    };
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return DoctorCheck::failure("runtime", "Running executable is not accessible")
                .with_detail(format!("{}: {error}", path.display()));
        }
    };
    if !metadata.is_file() {
        return DoctorCheck::failure("runtime", "Running executable is not a regular file")
            .with_detail(path.display().to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return DoctorCheck::failure("runtime", "Running executable is not executable")
                .with_detail(path.display().to_string());
        }
    }

    DoctorCheck::pass("runtime", "Running executable is usable")
        .with_detail(path.display().to_string())
}

fn config_source(source: ConfigPathSource) -> &'static str {
    match source {
        ConfigPathSource::CommandLine => "--config",
        ConfigPathSource::Environment => "CODEXIFY_CONFIG",
        ConfigPathSource::User => "user config",
        ConfigPathSource::Defaults => "built-in defaults",
    }
}

fn config_path_check(selection: &ConfigPathSelection) -> DoctorCheck {
    let Some(path) = selection.path.as_deref() else {
        return DoctorCheck::warning("config_path", "No config file path is available")
            .with_detail("Codexify will use built-in defaults")
            .with_remediation("Pass --config or set CODEXIFY_CONFIG");
    };
    let detail = format!(
        "{} (from {})",
        path.display(),
        config_source(selection.source)
    );
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            DoctorCheck::pass("config_path", "Selected config file exists").with_detail(detail)
        }
        Ok(_) => DoctorCheck::failure("config_path", "Selected config path is not a file")
            .with_detail(detail)
            .with_remediation("Replace the path with a regular JSON config file"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            DoctorCheck::warning("config_path", "Selected config file does not exist")
                .with_detail(detail)
                .with_remediation("Run codexify quickstart or create the selected config file")
        }
        Err(error) => DoctorCheck::failure("config_path", "Selected config file is inaccessible")
            .with_detail(format!("{detail}: {error}"))
            .with_remediation("Correct the config path or its filesystem permissions"),
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(windows)]
fn windows_extensions(pathext: Option<&OsStr>) -> Vec<OsString> {
    pathext
        .and_then(OsStr::to_str)
        .unwrap_or(".COM;.EXE;.BAT;.CMD")
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(OsString::from)
        .collect()
}

fn resolve_candidate(path: PathBuf, pathext: Option<&OsStr>) -> Option<PathBuf> {
    if is_executable_file(&path) {
        return Some(path);
    }
    #[cfg(windows)]
    if path.extension().is_none() {
        for extension in windows_extensions(pathext) {
            let extension = extension.to_string_lossy();
            let candidate = PathBuf::from(format!("{}{}", path.to_string_lossy(), extension));
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    let _ = pathext;
    None
}

fn resolve_executable(
    command: &OsStr,
    cwd: &Path,
    path_env: Option<&OsStr>,
    pathext: Option<&OsStr>,
) -> Result<PathBuf, String> {
    let command_path = Path::new(command);
    if command_path.is_absolute() || command_path.components().count() > 1 {
        let candidate = if command_path.is_absolute() {
            command_path.to_path_buf()
        } else {
            cwd.join(command_path)
        };
        return resolve_candidate(candidate.clone(), pathext)
            .ok_or_else(|| format!("{} is not an executable file", candidate.display()));
    }

    let path_env = path_env.ok_or_else(|| "PATH is not set".to_string())?;
    for directory in std::env::split_paths(path_env) {
        if let Some(candidate) = resolve_candidate(directory.join(command), pathext) {
            return Ok(candidate);
        }
    }
    Err(format!(
        "{} was not found on PATH",
        command.to_string_lossy()
    ))
}

fn process_path() -> Option<OsString> {
    std::env::var_os("PATH")
}

fn process_pathext() -> Option<OsString> {
    std::env::var_os("PATHEXT")
}

fn first_output_line(output: &std::process::Output) -> String {
    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr)
    } else {
        String::from_utf8_lossy(&output.stdout)
    };
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("version unavailable")
        .chars()
        .take(512)
        .collect()
}

async fn probe_version(
    command: &OsStr,
    cwd: &Path,
    path_env: Option<&OsStr>,
    pathext: Option<&OsStr>,
) -> Result<(PathBuf, String), String> {
    let resolved = resolve_executable(command, cwd, path_env, pathext)?;
    let version = probe_resolved_version(&resolved, cwd, path_env, pathext).await?;
    Ok((resolved, version))
}

async fn probe_resolved_version(
    resolved: &Path,
    cwd: &Path,
    path_env: Option<&OsStr>,
    pathext: Option<&OsStr>,
) -> Result<String, String> {
    let mut process = Command::new(resolved);
    process
        .arg("--version")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if let Some(path_env) = path_env {
        process.env("PATH", path_env);
    }
    #[cfg(windows)]
    if let Some(pathext) = pathext {
        process.env("PATHEXT", pathext);
    }
    #[cfg(not(windows))]
    let _ = pathext;
    let output = timeout(COMMAND_PROBE_TIMEOUT, process.output())
        .await
        .map_err(|_| format!("{} --version timed out", resolved.display()))?
        .map_err(|error| format!("could not run {} --version: {error}", resolved.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} --version exited with {}: {}",
            resolved.display(),
            output.status,
            first_output_line(&output)
        ));
    }
    Ok(first_output_line(&output))
}

fn is_git_checkout(path: &Path) -> bool {
    path.ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
}

async fn git_check(work_dir: &Path) -> DoctorCheck {
    let path = process_path();
    let pathext = process_pathext();
    let resolved = match resolve_executable(
        OsStr::new("git"),
        work_dir,
        path.as_deref(),
        pathext.as_deref(),
    ) {
        Ok(resolved) => resolved,
        Err(error) if is_git_checkout(work_dir) => {
            return DoctorCheck::warning("git", "Git checkout detected but Git is unavailable")
                .with_detail(error)
                .with_remediation("Install Git or fix PATH");
        }
        Err(error) => {
            return DoctorCheck::pass("git", "Git is not required for this work directory")
                .with_detail(error);
        }
    };
    match probe_resolved_version(&resolved, work_dir, path.as_deref(), pathext.as_deref()).await {
        Ok(version) => DoctorCheck::pass("git", "Git is usable")
            .with_detail(format!("{}; {version}", resolved.display())),
        Err(error) => DoctorCheck::warning("git", "Git executable found but could not be run")
            .with_detail(error)
            .with_remediation("Fix the selected Git executable or PATH"),
    }
}

async fn fixed_tool_check(id: &str, display: &str, command: &str) -> DoctorCheck {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let path = process_path();
    let pathext = process_pathext();
    match probe_version(
        OsStr::new(command),
        &cwd,
        path.as_deref(),
        pathext.as_deref(),
    )
    .await
    {
        Ok((resolved, version)) => DoctorCheck::pass(id, format!("{display} is usable"))
            .with_detail(format!("{}; {version}", resolved.display())),
        Err(error) => DoctorCheck::warning(id, format!("{display} is unavailable"))
            .with_detail(error)
            .with_remediation(format!("Install {display} or fix PATH")),
    }
}

fn shell_check(config: &AppConfig) -> DoctorCheck {
    let argv = resolve_shell(config.exec.default_shell.as_deref());
    let command = OsStr::new(&argv[0]);
    let path = process_path();
    let pathext = process_pathext();
    match resolve_executable(
        command,
        &config.work_dir,
        path.as_deref(),
        pathext.as_deref(),
    ) {
        Ok(resolved) => DoctorCheck::pass("shell", "exec_command shell is available")
            .with_detail(resolved.display().to_string()),
        Err(error) => DoctorCheck::failure("shell", "exec_command shell is unavailable")
            .with_detail(error)
            .with_remediation("Correct exec.defaultShell or install the selected shell"),
    }
}

fn codex_cli_check(settings: Result<CodexCliDiagnosticConfig, String>) -> DoctorCheck {
    let settings = match settings {
        Ok(settings) => settings,
        Err(error) => {
            return DoctorCheck::skipped(
                "codex_cli",
                "Codex CLI discovery could not be classified because configuration is invalid",
            )
            .with_detail(error);
        }
    };
    if !settings.enabled {
        return DoctorCheck::skipped("codex_cli", "Codex CLI MCP enrichment is disabled");
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let path = process_path();
    let pathext = process_pathext();
    match resolve_executable(
        &settings.command,
        &cwd,
        path.as_deref(),
        pathext.as_deref(),
    ) {
        Ok(resolved) => DoctorCheck::pass("codex_cli", "Codex CLI MCP enrichment is available")
            .with_detail(resolved.display().to_string()),
        Err(error) if settings.required => DoctorCheck::failure(
            "codex_cli",
            "Required Codex CLI MCP discovery is unavailable",
        )
        .with_detail(error)
        .with_remediation("Install Codex or correct codexMcp.cliPath/CODEX_CLI_PATH"),
        Err(error) => DoctorCheck::warning(
            "codex_cli",
            "Codex CLI MCP enrichment is unavailable",
        )
        .with_detail(error)
        .with_remediation(
            "Install Codex or disable codexMcp.useCli if plugin-provided MCP servers are not needed",
        ),
    }
}

fn mcp_stdio_check(config: &AppConfig) -> DoctorCheck {
    let default_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut missing = Vec::new();
    let mut checked = 0usize;
    for (name, spec) in &config.mcp_servers {
        if spec.disabled {
            continue;
        }
        let Some(command) = spec.command.as_deref() else {
            continue;
        };
        checked += 1;
        let cwd = spec
            .cwd
            .as_deref()
            .filter(|cwd| !cwd.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| default_cwd.clone());
        if !cwd.is_dir() {
            missing.push(format!("{name}: cwd does not exist ({})", cwd.display()));
            continue;
        }
        let path = spec
            .env
            .get("PATH")
            .map(OsStr::new)
            .map(OsString::from)
            .or_else(process_path);
        let pathext = spec
            .env
            .get("PATHEXT")
            .map(OsStr::new)
            .map(OsString::from)
            .or_else(process_pathext);
        if let Err(error) = resolve_executable(
            OsStr::new(command),
            &cwd,
            path.as_deref(),
            pathext.as_deref(),
        ) {
            missing.push(format!("{name}: {error}"));
        }
    }
    if checked == 0 {
        return DoctorCheck::skipped("mcp_stdio", "No enabled stdio MCP servers are configured");
    }
    if missing.is_empty() {
        return DoctorCheck::pass("mcp_stdio", "Configured stdio MCP commands are resolvable")
            .with_detail(format!("checked {checked} command(s)"));
    }
    DoctorCheck::warning("mcp_stdio", "Some stdio MCP commands are not resolvable")
        .with_detail(missing.join("; "))
        .with_remediation("Install the missing MCP command or correct its command, cwd, or PATH")
}

fn update_check() -> DoctorCheck {
    match crate::self_update::inspect_update_lock() {
        Ok(None) => DoctorCheck::pass("self_update", "No self-update lock is present"),
        Ok(Some(lock)) => {
            let detail = lock
                .update_id
                .as_deref()
                .map(|id| format!("{} (update {id})", lock.path.display()))
                .unwrap_or_else(|| lock.path.display().to_string());
            DoctorCheck::warning("self_update", "A self-update lock is present")
                .with_detail(detail)
                .with_remediation(
                    "Inspect `codexify service logs -f`; remove the lock only if no update is running",
                )
        }
        Err(error) => {
            DoctorCheck::failure("self_update", "Self-update state could not be inspected")
                .with_detail(format!("{error:#}"))
        }
    }
}

fn latest_version_check_from_result(
    result: anyhow::Result<LatestVersionInspection>,
) -> DoctorCheck {
    match result {
        Ok(inspection) => {
            let detail = format!(
                "current={}; latest={}",
                inspection.current, inspection.latest
            );
            match inspection.status {
                LatestVersionStatus::UpdateAvailable => DoctorCheck::pass(
                    "updates",
                    format!("New Codexify version {} is available", inspection.latest),
                )
                .with_detail(detail),
                LatestVersionStatus::UpToDate => {
                    DoctorCheck::pass("updates", "Codexify is up to date").with_detail(detail)
                }
                LatestVersionStatus::AheadOfLatest => DoctorCheck::pass(
                    "updates",
                    "Running Codexify is newer than the latest published release",
                )
                .with_detail(detail),
            }
        }
        Err(error) => {
            DoctorCheck::warning("updates", "Latest Codexify release could not be checked")
                .with_detail(format!("{error:#}"))
                .with_remediation("Check network access to GitHub and rerun `codexify doctor`")
        }
    }
}

async fn latest_version_check() -> DoctorCheck {
    latest_version_check_from_result(crate::self_update::inspect_latest_version().await)
}

fn service_check() -> (DoctorCheck, Option<service::ServiceStatus>) {
    match service::status() {
        Ok(status) if !status.installed => (
            DoctorCheck::skipped("service", "Native background service is not installed")
                .with_detail(status.detail.clone()),
            Some(status),
        ),
        Ok(status) if status.running && status.enabled != Some(false) => (
            DoctorCheck::pass("service", "Native background service is running")
                .with_detail(status.detail.clone()),
            Some(status),
        ),
        Ok(status) => (
            DoctorCheck::failure("service", "Native background service is not healthy")
                .with_detail(status.detail.clone())
                .with_remediation(
                    "Run `codexify service enable`, then inspect `codexify service logs`",
                ),
            Some(status),
        ),
        Err(error) => (
            DoctorCheck::failure("service", "Native service state could not be queried")
                .with_detail(format!("{error:#}")),
            None,
        ),
    }
}

#[derive(Debug, Deserialize)]
struct HealthPayload {
    status: String,
    tools: Option<usize>,
}

async fn probe_local_health(port: u16) -> Result<String, String> {
    let client = crate::tls::client_builder()
        .no_proxy()
        .redirect(Policy::none())
        .timeout(HEALTH_PROBE_TIMEOUT)
        .build()
        .map_err(|error| format!("build local health client: {error}"))?;
    let url = format!("http://127.0.0.1:{port}/health");
    let mut response = client
        .get(&url)
        .send()
        .await
        .map_err(|error| format!("{url}: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("{url}: HTTP {}", response.status()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > HEALTH_MAX_BYTES as u64)
    {
        return Err(format!(
            "{url}: health response exceeds {HEALTH_MAX_BYTES} bytes"
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("{url}: read health response: {error}"))?
    {
        if body.len().saturating_add(chunk.len()) > HEALTH_MAX_BYTES {
            return Err(format!(
                "{url}: health response exceeds {HEALTH_MAX_BYTES} bytes"
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let payload: HealthPayload = serde_json::from_slice(&body)
        .map_err(|error| format!("{url}: invalid Codexify health JSON: {error}"))?;
    if payload.status != "ok" {
        return Err(format!("{url}: reported status {:?}", payload.status));
    }
    Ok(match payload.tools {
        Some(tools) => format!("{url}; status=ok; tools={tools}"),
        None => format!("{url}; status=ok"),
    })
}

async fn health_check(
    config: Option<&AppConfig>,
    service_status: Option<&service::ServiceStatus>,
) -> DoctorCheck {
    let Some(config) = config else {
        return DoctorCheck::skipped(
            "health",
            "Local health could not be checked because configuration is invalid",
        );
    };
    let Some(service_status) = service_status else {
        return DoctorCheck::skipped(
            "health",
            "Local health could not be checked because service state is unavailable",
        );
    };
    if !service_status.installed {
        return DoctorCheck::skipped("health", "Local health is skipped without a native service");
    }
    if !service_status.running {
        return DoctorCheck::skipped(
            "health",
            "Local health is skipped while the service is stopped",
        );
    }
    match probe_local_health(config.port).await {
        Ok(detail) => DoctorCheck::pass("health", "Codexify local health endpoint is ready")
            .with_detail(detail),
        Err(error) => DoctorCheck::failure("health", "Codexify local health endpoint is unhealthy")
            .with_detail(error)
            .with_remediation(
                "Inspect `codexify service logs` and verify doctor selected the service configuration",
            ),
    }
}

fn tunnel_credential_check(config: &AppConfig) -> DoctorCheck {
    let Some(settings) = config.openai_tunnel.as_ref() else {
        return DoctorCheck::skipped(
            "openai_tunnel_credential",
            "OpenAI tunnel mode is not configured",
        );
    };
    match openai_tunnel::validate_key_reference(&settings.api_key_ref) {
        Ok(()) => DoctorCheck::pass(
            "openai_tunnel_credential",
            "OpenAI tunnel credential reference is usable",
        )
        .with_detail(settings.api_key_ref.clone()),
        Err(error) => DoctorCheck::failure(
            "openai_tunnel_credential",
            "OpenAI tunnel credential reference is unusable",
        )
        .with_detail(format!("{error:#}"))
        .with_remediation("Correct openaiTunnel.apiKeyRef or provide its referenced secret"),
    }
}

async fn tunnel_runtime_check(config: &AppConfig) -> DoctorCheck {
    let Some(settings) = config.openai_tunnel.as_ref() else {
        return DoctorCheck::skipped(
            "openai_tunnel_runtime",
            "OpenAI tunnel mode is not configured",
        );
    };
    match openai_tunnel::inspect_runtime(settings).await {
        Ok(TunnelRuntimeInspection::Ready(path)) => DoctorCheck::pass(
            "openai_tunnel_runtime",
            "OpenAI tunnel runtime is compatible",
        )
        .with_detail(path.display().to_string()),
        Ok(TunnelRuntimeInspection::MissingManaged(path)) => DoctorCheck::warning(
            "openai_tunnel_runtime",
            "Managed OpenAI tunnel runtime is not installed yet",
        )
        .with_detail(path.display().to_string())
        .with_remediation("Start Codexify normally to install the pinned verified tunnel runtime"),
        Err(error) => DoctorCheck::failure(
            "openai_tunnel_runtime",
            "OpenAI tunnel runtime is incomplete or incompatible",
        )
        .with_detail(format!("{error:#}"))
        .with_remediation("Repair or remove the configured tunnel runtime, then restart Codexify"),
    }
}

fn effective_configuration_check(config: &AppConfig) -> DoctorCheck {
    let mode = if config.multi_project {
        "multi-project"
    } else {
        "single-project"
    };
    DoctorCheck::pass("configuration", "Effective configuration is valid").with_detail(format!(
        "workDir={}; port={}; mode={mode}; mcpServers={}",
        config.work_dir.display(),
        config.port,
        config.mcp_servers.len()
    ))
}

async fn finish_report(
    mut checks: Vec<DoctorCheck>,
    config: Option<&AppConfig>,
    codex_cli: DoctorCheck,
) -> DoctorReport {
    let git_dir = config
        .map(|config| config.work_dir.as_path())
        .unwrap_or_else(|| Path::new("."));
    checks.push(git_check(git_dir).await);
    checks.push(fixed_tool_check("rg", "ripgrep", "rg").await);
    checks.push(fixed_tool_check("gh", "GitHub CLI", "gh").await);
    checks.push(codex_cli);

    if let Some(config) = config {
        checks.push(shell_check(config));
        checks.push(mcp_stdio_check(config));
    } else {
        checks.push(DoctorCheck::skipped(
            "shell",
            "exec_command shell could not be checked because configuration is invalid",
        ));
        checks.push(DoctorCheck::skipped(
            "mcp_stdio",
            "MCP stdio commands could not be checked because configuration is invalid",
        ));
    }

    checks.push(update_check());
    checks.push(latest_version_check().await);
    let (service_check, service_status) = service_check();
    checks.push(service_check);
    checks.push(health_check(config, service_status.as_ref()).await);
    if let Some(config) = config {
        checks.push(tunnel_credential_check(config));
        checks.push(tunnel_runtime_check(config).await);
    } else {
        checks.push(DoctorCheck::skipped(
            "openai_tunnel_credential",
            "OpenAI tunnel credential could not be checked because configuration is invalid",
        ));
        checks.push(DoctorCheck::skipped(
            "openai_tunnel_runtime",
            "OpenAI tunnel runtime could not be checked because configuration is invalid",
        ));
    }
    DoctorReport::new(checks)
}

pub async fn run(cli: Cli) -> DoctorReport {
    let codex_cli_settings = codex_cli_diagnostic_config(&cli);
    let mut checks = vec![runtime_check()];

    match config_path_selection(&cli) {
        Ok(selection) => {
            checks.push(config_path_check(&selection));
        }
        Err(error) => {
            checks.push(
                DoctorCheck::failure("config_path", "Config path could not be resolved")
                    .with_detail(error),
            );
        }
    }

    let config = match load_config_quiet(cli) {
        Ok(config) => {
            checks.push(effective_configuration_check(&config));
            Some(config)
        }
        Err(error) => {
            checks.push(
                DoctorCheck::failure("configuration", "Effective configuration is invalid")
                    .with_detail(error)
                    .with_remediation("Correct the config or run codexify quickstart"),
            );
            None
        }
    };
    finish_report(checks, config.as_ref(), codex_cli_check(codex_cli_settings)).await
}

pub async fn run_for_config(config: &AppConfig) -> DoctorReport {
    let checks = vec![
        runtime_check(),
        DoctorCheck::pass("config_path", "Using the running server configuration"),
        effective_configuration_check(config),
    ];
    finish_report(
        checks,
        Some(config),
        DoctorCheck::skipped(
            "codex_cli",
            "Codex CLI discovery was already resolved when the server started",
        ),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_update::{LatestVersionInspection, LatestVersionSource, LatestVersionStatus};
    use semver::Version;
    use std::path::Path;

    #[test]
    fn report_counts_statuses_and_renders_each_check() {
        let report = DoctorReport::new(vec![
            DoctorCheck::pass("runtime", "Runtime is usable").with_detail("arm64 executable"),
            DoctorCheck::warning("config_path", "Config file is absent"),
            DoctorCheck::failure("configuration", "Configuration is invalid")
                .with_remediation("Run codexify quickstart"),
            DoctorCheck::skipped("service", "Service is not installed"),
        ]);

        assert!(!report.ok);
        assert_eq!(report.summary.passed, 1);
        assert_eq!(report.summary.warnings, 1);
        assert_eq!(report.summary.failures, 1);
        assert_eq!(report.summary.skipped, 1);

        let rendered = report.render_human();
        assert!(rendered.contains("PASS runtime: Runtime is usable"));
        assert!(rendered.contains("WARN config_path: Config file is absent"));
        assert!(rendered.contains("FAIL configuration: Configuration is invalid"));
        assert!(rendered.contains("SKIP service: Service is not installed"));
        assert!(rendered.contains("Remediation: Run codexify quickstart"));
        assert!(rendered.contains("Result: unhealthy"));
    }

    #[test]
    fn report_json_uses_stable_status_and_summary_fields() {
        let report = DoctorReport::new(vec![DoctorCheck::pass(
            "configuration",
            "Effective configuration is valid",
        )]);
        let value = serde_json::to_value(report).unwrap();

        assert_eq!(value["ok"], true);
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["platform"]["os"], std::env::consts::OS);
        assert_eq!(value["platform"]["arch"], std::env::consts::ARCH);
        assert_eq!(value["checks"][0]["status"], "pass");
        assert_eq!(value["checks"][0]["detail"], serde_json::Value::Null);
        assert_eq!(value["checks"][0]["remediation"], serde_json::Value::Null);
        assert_eq!(value["summary"]["passed"], 1);
        assert_eq!(value["summary"]["warnings"], 0);
        assert_eq!(value["summary"]["failures"], 0);
        assert_eq!(value["summary"]["skipped"], 0);
    }

    #[test]
    fn version_check_keeps_update_availability_informational_but_probe_failures_warn() {
        let available = latest_version_check_from_result(Ok(LatestVersionInspection {
            status: LatestVersionStatus::UpdateAvailable,
            current: Version::new(1, 1, 0),
            latest: Version::new(1, 2, 0),
            source: LatestVersionSource::GithubApi,
        }));
        assert_eq!(available.status, DoctorStatus::Pass);
        assert!(available.summary.contains("1.2.0"));

        let failed = latest_version_check_from_result(Err(anyhow::anyhow!("offline")));
        assert_eq!(failed.status, DoctorStatus::Warning);
        assert!(failed.detail.as_deref().unwrap().contains("offline"));
    }

    #[cfg(unix)]
    #[test]
    fn command_resolver_uses_the_effective_path_and_requires_executable_files() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let tool = bin.join("tool");
        std::fs::write(&tool, "tool").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            resolve_executable(OsStr::new("tool"), root.path(), Some(bin.as_os_str()), None)
                .unwrap(),
            tool
        );

        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            resolve_executable(OsStr::new("tool"), root.path(), Some(bin.as_os_str()), None)
                .is_err()
        );
    }

    #[test]
    fn git_checkout_detection_walks_parent_directories() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let nested = root.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();

        assert!(is_git_checkout(&nested));
        assert!(!is_git_checkout(Path::new(
            "/definitely/not/a/codexify/git/checkout"
        )));
    }

    #[tokio::test]
    async fn health_probe_accepts_only_codexify_health_json_without_redirects() {
        use axum::Json;
        use axum::Router;
        use axum::response::Redirect;
        use axum::routing::get;
        use serde_json::json;

        let healthy = Router::new().route(
            "/health",
            get(|| async { Json(json!({ "status": "ok", "tools": 17 })) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { axum::serve(listener, healthy).await.unwrap() });
        let detail = probe_local_health(port).await.unwrap();
        assert!(detail.contains("tools=17"));
        server.abort();

        let redirect = Router::new().route(
            "/health",
            get(|| async { Redirect::temporary("http://127.0.0.1:1/health") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { axum::serve(listener, redirect).await.unwrap() });
        assert!(probe_local_health(port).await.is_err());
        server.abort();

        let malformed = Router::new().route("/health", get(|| async { "not json" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { axum::serve(listener, malformed).await.unwrap() });
        assert!(probe_local_health(port).await.is_err());
        server.abort();
    }
}
