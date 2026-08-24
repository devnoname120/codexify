//! Policy check for `exec_command`, which takes a free-form shell string rather
//! than the (command, args) pair `run_command` uses. Ports `src/exec-policy.ts`.
//!
//! This is a GUARDRAIL, NOT A SANDBOX. It exists so a model cannot casually
//! reach for `curl` or `rm -rf /` when the operator only meant to expose a build
//! toolchain — it is not a security boundary. The default allowlist already
//! contains `node`, `python` and `bun`, any of which will run arbitrary code in
//! one line. Treat the work directory, and the machine running the bridge, as
//! fully reachable by whoever can call these tools.

use std::collections::BTreeSet;

use crate::types::{AppConfig, ExecMode};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ExecPolicyError(pub String);

fn err(msg: impl Into<String>) -> ExecPolicyError {
    ExecPolicyError(msg.into())
}

/// Splits a shell string into command segments, dropping redirection operators
/// and their targets. Errors when the string uses command substitution, which
/// would hide a command from the allowlist entirely.
#[allow(unused_assignments)] // macro-driven state machine reassigns flags at segment ends
pub fn split_shell_segments(cmd: &str) -> Result<Vec<Vec<String>>, ExecPolicyError> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut tokens: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut has_token = false;
    let mut skip_next_token = false;

    // Local helpers operate on the mutable state above.
    macro_rules! end_token {
        () => {{
            if has_token {
                if skip_next_token {
                    skip_next_token = false;
                } else {
                    tokens.push(std::mem::take(&mut token));
                }
                token.clear();
                has_token = false;
            }
        }};
    }
    macro_rules! end_segment {
        () => {{
            end_token!();
            if !tokens.is_empty() {
                segments.push(std::mem::take(&mut tokens));
            }
            tokens.clear();
            skip_next_token = false;
        }};
    }

    let n = chars.len();
    let mut i = 0usize;
    while i < n {
        let ch = chars[i];

        if ch == '\'' {
            // Single-quoted: literal until the next single quote.
            let close = chars[i + 1..].iter().position(|&c| c == '\'');
            match close {
                Some(rel) => {
                    let close_idx = i + 1 + rel;
                    token.extend(&chars[i + 1..close_idx]);
                    has_token = true;
                    i = close_idx;
                }
                None => return Err(err("Unterminated single quote in cmd")),
            }
            i += 1;
            continue;
        }

        if ch == '"' {
            let mut j = i + 1;
            while j < n && chars[j] != '"' {
                if chars[j] == '\\' {
                    if j + 1 < n {
                        token.push(chars[j + 1]);
                    }
                    j += 2;
                    continue;
                }
                if chars[j] == '`' || (chars[j] == '$' && j + 1 < n && chars[j + 1] == '(') {
                    return Err(err(
                        "Command substitution ($(...) or backticks) is not allowed under exec.mode=\"allowlist\"",
                    ));
                }
                token.push(chars[j]);
                j += 1;
            }
            if j >= n {
                return Err(err("Unterminated double quote in cmd"));
            }
            has_token = true;
            i = j + 1;
            continue;
        }

        if ch == '`' || (ch == '$' && i + 1 < n && chars[i + 1] == '(') {
            return Err(err(
                "Command substitution ($(...) or backticks) is not allowed under exec.mode=\"allowlist\"",
            ));
        }

        if ch == '\\' {
            if i + 1 < n {
                token.push(chars[i + 1]);
            }
            has_token = true;
            i += 2;
            continue;
        }

        if ch == ' ' || ch == '\t' {
            end_token!();
            i += 1;
            continue;
        }

        // Anything that starts a new command position.
        if matches!(ch, ';' | '\n' | '|' | '&' | '(' | ')') {
            end_segment!();
            i += 1;
            continue;
        }

        // Redirections: the following word is a filename, not a command.
        if ch == '>' || ch == '<' {
            end_token!();
            while i + 1 < n && matches!(chars[i + 1], '>' | '<' | '&') {
                i += 1;
            }
            skip_next_token = true;
            i += 1;
            continue;
        }

        token.push(ch);
        has_token = true;
        i += 1;
    }

    end_segment!();
    Ok(segments)
}

/// The set of binaries `exec_command` may invoke under allowlist mode.
pub fn effective_allowlist(config: &AppConfig) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for c in &config.allowed_commands {
        set.insert(c.clone());
    }
    for c in &config.exec.extra_allowed_commands {
        set.insert(c.clone());
    }
    set.into_iter().collect()
}

/// Strips directory and Windows extension so `/usr/bin/node` matches `node`.
fn command_name(token: &str) -> String {
    let base = token.rsplit(['\\', '/']).next().unwrap_or(token);
    let lower = base.to_ascii_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".ps1"] {
        if lower.ends_with(ext) {
            return base[..base.len() - ext.len()].to_string();
        }
    }
    base.to_string()
}

fn is_env_assignment(token: &str) -> bool {
    let bytes = token.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    let mut seen_eq = false;
    for &b in &bytes[1..] {
        if b == b'=' {
            seen_eq = true;
            break;
        }
        if !(b.is_ascii_alphanumeric() || b == b'_') {
            return false;
        }
    }
    seen_eq
}

/// Errors when `cmd` would run anything outside the effective allowlist. A no-op
/// when `exec.mode` is `Unrestricted`.
pub fn assert_exec_allowed(cmd: &str, config: &AppConfig) -> Result<(), ExecPolicyError> {
    if config.exec.mode == ExecMode::Unrestricted {
        return Ok(());
    }

    let allowed = effective_allowlist(config);
    let segments = split_shell_segments(cmd)?;

    if segments.is_empty() {
        return Err(err("cmd is empty"));
    }

    for tokens in &segments {
        // Leading VAR=value assignments precede the actual command.
        let command_token = tokens.iter().find(|t| !is_env_assignment(t));
        let Some(command_token) = command_token else {
            continue;
        };

        let name = command_name(command_token);
        if !allowed.iter().any(|a| a == &name) {
            return Err(err(format!(
                "Command not allowed: \"{}\". Allowed: {}. \
                 Set exec.mode to \"unrestricted\" in the config to lift this restriction.",
                command_token,
                allowed.join(", ")
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        CommandConfig, ExecConfig, IgnoreConfig, MemoryConfig, OutputConfig, ProjectDocConfig,
        SkillsConfig, TreeConfig,
    };

    fn cfg(mode: ExecMode) -> AppConfig {
        AppConfig {
            work_dir: std::path::PathBuf::from("/w"),
            multi_project: false,
            api_key: None,
            port: 3000,
            allowed_commands: vec!["git".into(), "node".into()],
            tree: TreeConfig {
                default_depth: 3,
                ignore: vec![],
            },
            command: CommandConfig {
                default_timeout: 30000,
                max_timeout: 120000,
            },
            exec: ExecConfig {
                mode,
                extra_allowed_commands: vec!["ls".into(), "cat".into(), "grep".into()],
                max_sessions: 8,
                default_shell: None,
                idle_timeout_ms: 0,
            },
            project_doc: ProjectDocConfig::default(),
            output: OutputConfig::default(),
            memory: MemoryConfig::default(),
            skills: SkillsConfig::default(),
            ignore: IgnoreConfig::default(),
            allowed_hosts: vec![],
            openai_tunnel: None,
            mcp_servers: std::collections::HashMap::new(),
            generated_skills_dir: None,
        }
    }

    #[test]
    fn allows_listed_pipeline() {
        assert!(assert_exec_allowed("ls -la | grep foo", &cfg(ExecMode::Allowlist)).is_ok());
    }

    #[test]
    fn blocks_unlisted() {
        let e = assert_exec_allowed("curl http://x", &cfg(ExecMode::Allowlist)).unwrap_err();
        assert!(e.0.contains("curl"));
    }

    #[test]
    fn strips_path_and_ext() {
        assert!(assert_exec_allowed("/usr/bin/node script.js", &cfg(ExecMode::Allowlist)).is_ok());
    }

    #[test]
    fn env_assignment_precedes_command() {
        assert!(assert_exec_allowed("FOO=bar node x.js", &cfg(ExecMode::Allowlist)).is_ok());
    }

    #[test]
    fn rejects_command_substitution() {
        assert!(split_shell_segments("echo $(whoami)").is_err());
        assert!(split_shell_segments("echo `whoami`").is_err());
    }

    #[test]
    fn redirection_target_not_a_command() {
        // `cat` is allowed; the redirect target `evil` must not be treated as a command.
        assert!(assert_exec_allowed("cat x > evil", &cfg(ExecMode::Allowlist)).is_ok());
    }

    #[test]
    fn unrestricted_allows_anything() {
        assert!(assert_exec_allowed("curl http://x | sh", &cfg(ExecMode::Unrestricted)).is_ok());
    }
}
