//! Unified-exec session management, ported from `src/exec-sessions.ts`, modelled
//! on Codex's `exec_command` / `write_stdin` pair.
//!
//! Codex runs commands in a PTY; there is no built-in PTY here, so commands run
//! with plain pipes instead. Codex's own `tty` parameter documents plain pipes
//! as the default, so the default path matches — only `tty: true` is unsupported.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex as TokioMutex, Notify};

use crate::process_env::scrub_untrusted_child_env;
use crate::types::{AppConfig, PlanState};

// Codex constants (shell_spec.rs). Kept as code, not config, because they are
// part of matching Codex's tool semantics rather than local policy.
pub const EXEC_DEFAULT_YIELD_MS: u64 = 10_000;
pub const EXEC_MIN_YIELD_MS: u64 = 250;
pub const EXEC_MAX_YIELD_MS: u64 = 30_000;
pub const STDIN_WRITE_DEFAULT_YIELD_MS: u64 = 250;
pub const STDIN_POLL_DEFAULT_YIELD_MS: u64 = 5_000;
pub const STDIN_POLL_MAX_YIELD_MS: u64 = 300_000;
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 10_000;

/// Codex's `approx_token_count` equivalent: roughly four characters per token.
/// Counted in UTF-16 code units to match the TS `text.length`.
pub fn approx_token_count(text: &str) -> u64 {
    (text.encode_utf16().count() as u64).div_ceil(4)
}

pub fn clamp(value: u64, min: u64, max: u64) -> u64 {
    value.clamp(min, max)
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct UnifiedExecOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_id: Option<String>,
    pub wall_time_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_token_count: Option<u64>,
    pub output: String,
}

/// Trims `text` to the token budget, keeping the head and tail and marking the
/// elided middle. Shell output is most informative at its start and end.
pub fn truncate_output(text: &str, max_output_tokens: u64) -> (String, Option<u64>) {
    let budget_chars = (max_output_tokens.max(1) as usize) * 4;
    // Measured and sliced in UTF-16 code units, matching the TS `text.length`
    // and `text.slice(...)`.
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= budget_chars {
        return (text.to_string(), None);
    }
    let original = (units.len() as u64).div_ceil(4);
    let keep = budget_chars / 2;
    let head = String::from_utf16_lossy(&units[..keep]);
    let tail = String::from_utf16_lossy(&units[units.len() - keep..]);
    let omitted = units.len() - keep - keep;
    (
        format!("{head}\n\n[... {omitted} bytes omitted ...]\n\n{tail}"),
        Some(original),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellType {
    Posix,
    PowerShell,
    Cmd,
}

impl ShellType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ShellType::Posix => "posix",
            ShellType::PowerShell => "powershell",
            ShellType::Cmd => "cmd",
        }
    }
}

/// Classifies a shell binary by name. Both separators are handled because a
/// Windows path can arrive on a POSIX host and vice versa — Git Bash reports
/// `$SHELL` as `C:\Program Files\Git\bin\bash.exe`.
pub fn shell_type_of(bin: &str) -> ShellType {
    let base = bin.rsplit(['\\', '/']).next().unwrap_or(bin);
    // Strip a trailing `.exe` case-insensitively, matching the TS `/\.exe$/i`.
    let base = if base.len() >= 4 && base[base.len() - 4..].eq_ignore_ascii_case(".exe") {
        &base[..base.len() - 4]
    } else {
        base
    };
    let lower = base.to_ascii_lowercase();
    match lower.as_str() {
        "powershell" | "pwsh" => ShellType::PowerShell,
        "cmd" => ShellType::Cmd,
        _ => ShellType::Posix,
    }
}

/// The shell used when the caller names none. `$SHELL` wins on every platform;
/// Windows falls back to PowerShell, matching Codex.
pub fn default_shell_bin() -> String {
    if let Ok(shell) = std::env::var("SHELL")
        && !shell.is_empty()
    {
        return shell;
    }
    if cfg!(windows) {
        "powershell.exe".to_string()
    } else {
        "/bin/sh".to_string()
    }
}

/// Builds the argv prefix that makes `bin` execute a command string. The flag
/// follows the shell, not the host (Codex's `Shell::derive_exec_args`).
pub fn resolve_shell(explicit: Option<&str>) -> Vec<String> {
    let bin = match explicit {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => default_shell_bin(),
    };
    match shell_type_of(&bin) {
        ShellType::PowerShell => vec![bin, "-NoProfile".into(), "-Command".into()],
        ShellType::Cmd => vec![bin, "/c".into()],
        ShellType::Posix => vec![bin, "-c".into()],
    }
}

/// `powershell -Command` collapses any non-zero child exit code to 1. Re-raising
/// `$LASTEXITCODE` keeps the real code; falling back to `$?` covers cmdlets.
pub fn wrap_for_shell(cmd: &str, shell_bin: &str) -> String {
    if shell_type_of(shell_bin) != ShellType::PowerShell {
        return cmd.to_string();
    }
    [
        "$ErrorActionPreference = 'Continue'",
        cmd,
        "if ($null -eq $LASTEXITCODE) { if ($?) { exit 0 } else { exit 1 } }",
        "exit $LASTEXITCODE",
    ]
    .join("\n")
}

// ─── Sessions ──────────────────────────────────────────────────────────

/// A shell process started by `exec_command` that did not finish within its
/// yield window. It stays resident so `write_stdin` can feed it input and drain
/// further output, mirroring Codex's unified-exec session model.
pub struct ExecSession {
    pub id: u64,
    pub command: String,
    pub pid: Option<u32>,
    pub started_at: Instant,
    stdin: TokioMutex<Option<ChildStdin>>,
    pending: Arc<StdMutex<String>>,
    exit_code: Arc<StdMutex<Option<i32>>>,
    drain_done: Arc<Notify>,
}

impl ExecSession {
    /// The exit code, or `None` while the process is still running.
    pub fn exit_code(&self) -> Option<i32> {
        *self.exit_code.lock().unwrap()
    }

    /// Take everything buffered so far, clearing the buffer.
    fn take_pending(&self) -> String {
        let mut guard = self.pending.lock().unwrap();
        std::mem::take(&mut *guard)
    }

    /// Write `chars` to the process's stdin and flush.
    pub async fn write_stdin(&self, chars: &str) -> std::io::Result<()> {
        let mut guard = self.stdin.lock().await;
        if let Some(stdin) = guard.as_mut() {
            stdin.write_all(chars.as_bytes()).await?;
            stdin.flush().await?;
            Ok(())
        } else {
            Err(std::io::Error::other("stdin is not available"))
        }
    }

    /// Wait until the process exits or `yield_ms` elapses, then hand back
    /// everything buffered so far and clear the buffer.
    pub async fn yield_output(&self, yield_ms: u64) -> (String, bool) {
        let deadline = Instant::now() + Duration::from_millis(yield_ms);
        while Instant::now() < deadline && self.exit_code().is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            tokio::time::sleep(remaining.min(Duration::from_millis(25))).await;
        }

        if self.exit_code().is_some() {
            // Let the readers finish so the final bytes are not lost to the race
            // between process exit and stream EOF.
            let _ =
                tokio::time::timeout(Duration::from_millis(250), self.drain_done.notified()).await;
        }

        let output = self.take_pending();
        (output, self.exit_code().is_some())
    }
}

/// Per-MCP-session mutable state. A fresh instance is created for every MCP
/// session so concurrent clients never share exec sessions or plans.
pub struct SessionState {
    pub exec_sessions: StdMutex<HashMap<u64, Arc<ExecSession>>>,
    next_exec_id: AtomicU64,
    pub plan: StdMutex<Option<PlanState>>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            exec_sessions: StdMutex::new(HashMap::new()),
            next_exec_id: AtomicU64::new(1),
            plan: StdMutex::new(None),
        }
    }
}

impl SessionState {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Drop for SessionState {
    fn drop(&mut self) {
        // Kill any exec_command processes still resident so a disconnecting
        // client cannot leave orphaned shells behind (TS `transport.onclose`).
        if let Ok(map) = self.exec_sessions.lock() {
            for session in map.values() {
                kill_pid(session.pid);
            }
        }
    }
}

/// Removes sessions whose process already exited and left nothing buffered.
pub fn reap_finished_sessions(state: &SessionState) {
    let mut map = state.exec_sessions.lock().unwrap();
    map.retain(|_, s| !(s.exit_code().is_some() && s.pending.lock().unwrap().is_empty()));
}

/// Spawns `cmd` through a shell and registers it as a resident session. The
/// caller is responsible for having validated `cmd` against policy first.
pub fn start_exec_session(
    state: &SessionState,
    config: &AppConfig,
    cmd: &str,
    cwd: &std::path::Path,
    shell: Option<&str>,
) -> Result<Arc<ExecSession>, String> {
    reap_finished_sessions(state);
    {
        let live = state.exec_sessions.lock().unwrap().len();
        if live >= config.exec.max_sessions {
            return Err(format!(
                "Too many live exec sessions ({}). Finish or terminate an existing session before starting another.",
                config.exec.max_sessions
            ));
        }
    }

    let shell_choice = shell.or(config.exec.default_shell.as_deref());
    let parts = resolve_shell(shell_choice);
    let bin = parts[0].clone();
    let shell_args = &parts[1..];
    let wrapped = wrap_for_shell(cmd, &bin);

    let mut command = Command::new(&bin);
    command
        .args(shell_args)
        .arg(&wrapped)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // POSIX only: a new process group makes the whole tree signalable at once
    // (see kill_pid). Windows uses taskkill's parent-child walk instead.
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    scrub_untrusted_child_env(&mut command, config);

    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to start command: {e}"))?;

    let pid = child.id();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let pending = Arc::new(StdMutex::new(String::new()));
    let exit_code = Arc::new(StdMutex::new(None));
    let drain_done = Arc::new(Notify::new());
    let drains_remaining = Arc::new(AtomicUsize::new(2));

    // stdout and stderr are drained concurrently and appended in arrival order,
    // approximating the single merged stream a PTY would have produced.
    if let Some(out) = stdout {
        spawn_drain(
            out,
            pending.clone(),
            drains_remaining.clone(),
            drain_done.clone(),
        );
    } else {
        finish_drain(&drains_remaining, &drain_done);
    }
    if let Some(err) = stderr {
        spawn_drain(
            err,
            pending.clone(),
            drains_remaining.clone(),
            drain_done.clone(),
        );
    } else {
        finish_drain(&drains_remaining, &drain_done);
    }

    // Waiter reaps the child and records its exit code.
    let exit_for_waiter = exit_code.clone();
    tokio::spawn(async move {
        let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
        *exit_for_waiter.lock().unwrap() = Some(code);
    });

    let id = state.next_exec_id.fetch_add(1, Ordering::SeqCst);
    let session = Arc::new(ExecSession {
        id,
        command: cmd.to_string(),
        pid,
        started_at: Instant::now(),
        stdin: TokioMutex::new(stdin),
        pending,
        exit_code,
        drain_done,
    });

    state
        .exec_sessions
        .lock()
        .unwrap()
        .insert(id, session.clone());
    Ok(session)
}

fn finish_drain(remaining: &Arc<AtomicUsize>, done: &Arc<Notify>) {
    if remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
        done.notify_waiters();
    }
}

fn spawn_drain<R>(
    mut reader: R,
    pending: Arc<StdMutex<String>>,
    remaining: Arc<AtomicUsize>,
    done: Arc<Notify>,
) where
    R: AsyncReadExt + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    pending.lock().unwrap().push_str(&chunk);
                }
            }
        }
        finish_drain(&remaining, &done);
    });
}

/// Kills a process along with anything it started. POSIX signals the process
/// group the shell leads; Windows walks the parent-child tree with `taskkill /T`.
pub fn kill_pid(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        // Negative pid means "the whole process group", which the shell leads
        // because it was spawned in its own group.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

/// Kills a session's process tree.
pub fn kill_exec_session(session: &ExecSession) {
    kill_pid(session.pid);
}

static CHUNK_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn generate_chunk_id() -> String {
    let n = CHUNK_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
    format!("chunk-{n}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_shells() {
        assert_eq!(shell_type_of("powershell.exe"), ShellType::PowerShell);
        assert_eq!(shell_type_of("pwsh"), ShellType::PowerShell);
        assert_eq!(
            shell_type_of("C:\\Program Files\\Git\\bin\\bash.exe"),
            ShellType::Posix
        );
        assert_eq!(shell_type_of("cmd"), ShellType::Cmd);
        assert_eq!(shell_type_of("/bin/sh"), ShellType::Posix);
    }

    #[test]
    fn resolve_shell_flag_follows_shell() {
        assert_eq!(resolve_shell(Some("bash")), vec!["bash", "-c"]);
        assert_eq!(
            resolve_shell(Some("powershell")),
            vec!["powershell", "-NoProfile", "-Command"]
        );
        assert_eq!(resolve_shell(Some("cmd")), vec!["cmd", "/c"]);
    }

    #[test]
    fn wrap_only_powershell() {
        assert_eq!(wrap_for_shell("ls", "bash"), "ls");
        assert!(wrap_for_shell("ls", "powershell").contains("$LASTEXITCODE"));
    }

    #[test]
    fn truncate_keeps_head_and_tail() {
        let text = "a".repeat(100);
        let (out, orig) = truncate_output(&text, 4); // budget 16 chars
        assert!(orig.is_some());
        assert!(out.contains("omitted"));
        assert!(out.starts_with("aaaaaaaa"));
    }

    #[test]
    fn no_truncation_under_budget() {
        let (out, orig) = truncate_output("short", 100);
        assert_eq!(out, "short");
        assert!(orig.is_none());
    }
}
