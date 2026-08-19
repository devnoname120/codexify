//! CLI parsing and config loading. Ports `src/config.ts`.
//!
//! An existing `codex.config.json` keeps working: every field is read with its
//! original camelCase name, absent sections fall back to the same defaults the
//! TypeScript used, and a missing config file is tolerated.

use std::path::{Path, PathBuf};

use clap::Parser;
use serde::Deserialize;

use std::collections::HashMap;

use crate::types::{
    AppConfig, CommandConfig, ExecConfig, ExecMode, IgnoreConfig, McpServerSpec, MemoryConfig,
    OutputConfig, ProjectDocConfig, SkillsConfig, TreeConfig,
};

#[derive(Parser, Debug)]
#[command(
    name = "codexify",
    about = "Codexify MCP bridge (Rust): expose Codex-style agent tools over Streamable HTTP."
)]
pub struct Cli {
    /// Project directory the tools operate on (required).
    #[arg(long = "work-dir")]
    pub work_dir: String,

    /// Server port. Default: 3000 (or the config file's value).
    #[arg(long)]
    pub port: Option<u16>,

    /// Bearer token for auth. When set, every request except /health must carry it.
    #[arg(long = "api-key")]
    pub api_key: Option<String>,

    /// Config file path. Default: ./codex.config.json (tolerated if missing).
    #[arg(long)]
    pub config: Option<String>,
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

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileConfig {
    api_key: Option<String>,
    port: Option<u16>,
    allowed_commands: Option<Vec<String>>,
    tree: Option<PartialTree>,
    command: Option<PartialCommand>,
    exec: Option<PartialExec>,
    project_doc: Option<ProjectDocConfig>,
    output: Option<OutputConfig>,
    memory: Option<MemoryConfig>,
    skills: Option<SkillsConfig>,
    ignore: Option<IgnoreConfig>,
    allowed_hosts: Option<Vec<String>>,
    mcp_servers: Option<HashMap<String, McpServerSpec>>,
}

/// A fully-defaulted config for a given work directory, matching what
/// `load_config` produces from an empty config file. Handy for tests and for
/// embedding the server without a config file.
pub fn default_config(work_dir: std::path::PathBuf) -> AppConfig {
    AppConfig {
        work_dir,
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
    let file: FileConfig = match std::fs::read_to_string(&config_path) {
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
    if let Some(t) = file.tree {
        if let Some(d) = t.default_depth {
            tree.default_depth = d;
        }
        if let Some(ig) = t.ignore {
            tree.ignore = ig;
        }
    }

    let mut command = default_command();
    if let Some(c) = file.command {
        if let Some(d) = c.default_timeout {
            command.default_timeout = d;
        }
        if let Some(m) = c.max_timeout {
            command.max_timeout = m;
        }
    }

    let mut exec = default_exec();
    if let Some(e) = file.exec {
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

    Ok(AppConfig {
        work_dir,
        api_key: cli.api_key.or(file.api_key),
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
        mcp_servers: file.mcp_servers.unwrap_or_default(),
        generated_skills_dir: None,
    })
}
