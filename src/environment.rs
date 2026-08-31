//! Environment description, ported from the describe/render helpers in
//! `src/tools/get-environment.ts`.
//!
//! Codex hands its model an `<environment_context>` block before the first turn.
//! MCP has no equivalent channel, so the same facts are offered as a tool call
//! and repeated in the server's `instructions`.

use serde::Serialize;

use crate::exec_policy::effective_allowlist;
use crate::exec_sessions::{resolve_shell, shell_type_of};
use crate::types::AppConfig;

/// Node-style platform identifier, matching what the TypeScript reported.
pub fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        "linux" => "linux",
        other => other,
    }
}

/// Node-style architecture identifier.
pub fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Friendly OS name, since the platform id says "win32" and "darwin".
pub fn os_name(platform: &str) -> String {
    match platform {
        "win32" => "Windows".into(),
        "darwin" => "macOS".into(),
        "linux" => "Linux".into(),
        other => other.into(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ShellInfo {
    pub bin: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub argv_prefix: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecInfo {
    pub mode: String,
    pub max_sessions: usize,
    pub allowed_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentInfo {
    pub os: String,
    pub platform: String,
    pub arch: String,
    pub cwd: String,
    pub path_separator: String,
    pub shell: ShellInfo,
    pub exec: ExecInfo,
}

pub fn describe_environment(config: &AppConfig) -> EnvironmentInfo {
    let parts = resolve_shell(config.exec.default_shell.as_deref());
    let bin = parts[0].clone();
    let args = parts[1..].to_vec();
    let platform = node_platform().to_string();

    let mode = match config.exec.mode {
        crate::types::ExecMode::Allowlist => "allowlist",
        crate::types::ExecMode::Unrestricted => "unrestricted",
    };

    EnvironmentInfo {
        os: os_name(&platform),
        platform,
        arch: node_arch().to_string(),
        cwd: config.work_dir.to_string_lossy().into_owned(),
        path_separator: std::path::MAIN_SEPARATOR.to_string(),
        shell: ShellInfo {
            bin: bin.clone(),
            type_: shell_type_of(&bin).as_str().to_string(),
            argv_prefix: args,
        },
        exec: ExecInfo {
            mode: mode.to_string(),
            max_sessions: config.exec.max_sessions,
            allowed_commands: effective_allowlist(config),
        },
    }
}

/// Prose rather than raw JSON, because the shell type is the one fact that
/// changes what a caller should write, and it is easy to miss in a nested object.
pub fn render_environment(info: &EnvironmentInfo) -> String {
    let shell_advice = match info.shell.type_.as_str() {
        "powershell" => {
            "Write PowerShell, not POSIX sh: `Get-ChildItem` not `ls -la`, `Select-String` not `grep`, `$env:FOO='bar'` not `FOO=bar`. Names like ls/cat/rm are aliases for cmdlets and take different flags."
        }
        "cmd" => {
            "Write cmd.exe syntax: `dir` not `ls`, `%VAR%` not `$VAR`, `&&` chains but no pipelines of POSIX tools unless they are installed."
        }
        _ => "Write POSIX sh syntax. Standard utilities (ls, grep, sed, awk) behave as usual.",
    };

    let exec_policy = if info.exec.mode == "allowlist" {
        format!(", allowing: {}", info.exec.allowed_commands.join(", "))
    } else {
        " (any command runs)".to_string()
    };

    let path_sep_json = serde_json::to_string(&info.path_separator).unwrap_or_default();

    [
        format!("OS: {} ({}/{})", info.os, info.platform, info.arch),
        format!("Working directory: {}", info.cwd),
        format!("Path separator: {path_sep_json}"),
        format!(
            "Shell for exec_command: {} ({})",
            info.shell.bin, info.shell.type_
        ),
        String::new(),
        shell_advice.to_string(),
        String::new(),
        format!("exec_command policy: {}{}", info.exec.mode, exec_policy),
        format!("Concurrent exec sessions: up to {}", info.exec.max_sessions),
    ]
    .join("\n")
}
