//! Unified-exec session management, ported from `src/exec-sessions.ts`, modelled
//! on Codex's `exec_command` / `write_stdin` pair.
//!
//! Codex runs commands in a PTY; there is no built-in PTY here, so commands run
//! with plain pipes instead. Codex's own `tty` parameter documents plain pipes
//! as the default, so the default path matches — only `tty: true` is unsupported.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex as TokioMutex, Notify};

use crate::process_env::scrub_untrusted_child_env;
use crate::project_bindings::{
    ConversationIdentity, ProjectBindingScope, ProjectRootSelection, resolve_project_root,
};
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

// ─── Bounded output buffer ─────────────────────────────────────────────

/// Ceiling on bytes retained in RAM per session between yields. A resident
/// session drains stdout/stderr continuously, so without a cap a noisy command
/// (`yes`, a chatty build) could grow the pending buffer without bound and
/// exhaust memory long before the next `yield_output`. Output beyond this is
/// elided from the middle — head and tail are kept, matching `truncate_output`'s
/// "most informative at the start and end" heuristic. The taken output is
/// truncated again to the caller's token budget, so this is only a
/// memory-safety ceiling, set well above any useful single yield.
const MAX_PENDING_BYTES: usize = 1 << 20; // 1 MiB
const PENDING_HEAD_BYTES: usize = MAX_PENDING_BYTES / 2;
const PENDING_TAIL_BYTES: usize = MAX_PENDING_BYTES - PENDING_HEAD_BYTES;

/// Largest byte index `<= index` that lands on a UTF-8 char boundary.
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest byte index `>= index` that lands on a UTF-8 char boundary.
fn ceil_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Accumulates a process's output while bounding memory: the first
/// `PENDING_HEAD_BYTES` are frozen as the head, and the most recent
/// `PENDING_TAIL_BYTES` slide in the tail. Anything in between is dropped and
/// counted. Bytes always arrive in stream order, so head then tail reconstructs
/// the stream verbatim whenever nothing was elided.
#[derive(Default)]
struct PendingBuffer {
    head: String,
    tail: String,
    omitted: u64,
}

impl PendingBuffer {
    fn new() -> Self {
        Self::default()
    }

    fn push_str(&mut self, chunk: &str) {
        let mut chunk = chunk;
        if self.head.len() < PENDING_HEAD_BYTES {
            let space = PENDING_HEAD_BYTES - self.head.len();
            if chunk.len() <= space {
                self.head.push_str(chunk);
                return;
            }
            let split = floor_char_boundary(chunk, space);
            self.head.push_str(&chunk[..split]);
            chunk = &chunk[split..];
        }
        self.tail.push_str(chunk);
        if self.tail.len() > PENDING_TAIL_BYTES {
            let overflow = self.tail.len() - PENDING_TAIL_BYTES;
            let cut = ceil_char_boundary(&self.tail, overflow);
            self.omitted += cut as u64;
            self.tail.drain(..cut);
        }
    }

    /// Reconstructs the retained output and resets the buffer, mirroring the
    /// old `std::mem::take` semantics of "hand back everything so far, clear".
    fn take(&mut self) -> String {
        let head = std::mem::take(&mut self.head);
        let tail = std::mem::take(&mut self.tail);
        let omitted = std::mem::replace(&mut self.omitted, 0);
        if omitted == 0 {
            let mut out = head;
            out.push_str(&tail);
            out
        } else {
            format!(
                "{head}\n\n[... {omitted} bytes elided (session output buffer limit) ...]\n\n{tail}"
            )
        }
    }

    fn is_empty(&self) -> bool {
        self.head.is_empty() && self.tail.is_empty() && self.omitted == 0
    }
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
    pending: Arc<StdMutex<PendingBuffer>>,
    exit_code: Arc<StdMutex<Option<i32>>>,
    drain_done: Arc<Notify>,
    /// Last time a tool interacted with this session (output yield or stdin
    /// write). The idle reaper measures inactivity from here.
    last_activity: StdMutex<Instant>,
}

impl ExecSession {
    /// The exit code, or `None` while the process is still running.
    pub fn exit_code(&self) -> Option<i32> {
        *self.exit_code.lock().unwrap()
    }

    /// Mark the session as just used, resetting its idle clock.
    fn touch(&self) {
        *self.last_activity.lock().unwrap() = Instant::now();
    }

    /// How long the session has sat idle since the last tool interaction.
    fn idle_for(&self) -> Duration {
        self.last_activity.lock().unwrap().elapsed()
    }

    /// Take everything buffered so far, clearing the buffer.
    fn take_pending(&self) -> String {
        self.pending.lock().unwrap().take()
    }

    /// Write `chars` to the process's stdin and flush.
    pub async fn write_stdin(&self, chars: &str) -> std::io::Result<()> {
        self.touch();
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
        self.touch();
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

/// The resident-process portion of a session. Keeping this behind an `Arc`
/// separates process lifetime from the lightweight per-call [`SessionState`]
/// view used by tools. The last owner kills any still-running process trees.
struct ExecSessionState {
    sessions: StdMutex<HashMap<u64, Arc<ExecSession>>>,
    next_id: AtomicU64,
}

impl ExecSessionState {
    fn new() -> Self {
        Self {
            sessions: StdMutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    fn session(&self, id: u64) -> Option<Arc<ExecSession>> {
        self.sessions.lock().unwrap().get(&id).cloned()
    }

    fn session_ids(&self) -> Vec<u64> {
        let mut ids = self
            .sessions
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    fn remove(&self, id: u64) -> Option<Arc<ExecSession>> {
        self.sessions.lock().unwrap().remove(&id)
    }

    fn len(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    fn is_empty(&self) -> bool {
        self.sessions.lock().unwrap().is_empty()
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    fn insert(&self, session: Arc<ExecSession>) {
        self.sessions.lock().unwrap().insert(session.id, session);
    }

    fn reap(&self, idle_timeout: Duration) -> Vec<Arc<ExecSession>> {
        reap_sessions(&mut self.sessions.lock().unwrap(), idle_timeout)
    }

    /// Starts a transport-owned reaper. It holds only a weak reference, so it
    /// exits when the transport's last [`SessionState`] view is dropped.
    fn spawn_idle_reaper(self: &Arc<Self>, idle_timeout: Duration) {
        if idle_timeout.is_zero() || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let weak = Arc::downgrade(self);
        let interval = reaper_interval(idle_timeout);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(state) = weak.upgrade() else {
                    break;
                };
                for session in state.reap(idle_timeout) {
                    kill_exec_session(&session);
                }
            }
        });
    }
}

impl Drop for ExecSessionState {
    fn drop(&mut self) {
        if let Ok(sessions) = self.sessions.lock() {
            for session in sessions.values() {
                if session.exit_code().is_none() {
                    kill_pid(session.pid);
                }
            }
        }
    }
}

type ConversationExecStates = HashMap<ConversationIdentity, Arc<ExecSessionState>>;

/// Server-process ownership for ChatGPT resident commands. ChatGPT can replace
/// its MCP transport between adjacent tool calls while retaining the same
/// `_meta["openai/session"]`; this store makes that stable identity, rather than
/// the transient transport, own `exec_command` sessions.
#[derive(Default)]
pub struct ConversationExecSessionStore {
    states: Arc<StdMutex<ConversationExecStates>>,
}

impl ConversationExecSessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a tool-call view that shares the transport's non-exec state but
    /// substitutes the resident-process state owned by this conversation.
    pub fn session_for(
        &self,
        identity: &ConversationIdentity,
        transport_session: &SessionState,
    ) -> SessionState {
        let exec = self
            .states
            .lock()
            .unwrap()
            .entry(identity.clone())
            .or_insert_with(|| Arc::new(ExecSessionState::new()))
            .clone();
        transport_session.with_exec_state(exec)
    }

    /// Reap inactive conversation-owned processes and discard empty ownership
    /// records. Unlike transport state, this task persists across MCP reconnects
    /// and stops only when the server drops the store.
    pub fn spawn_idle_reaper(&self, idle_timeout: Duration) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let weak = Arc::downgrade(&self.states);
        let interval = if idle_timeout.is_zero() {
            Duration::from_secs(30)
        } else {
            reaper_interval(idle_timeout)
        };
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(states) = weak.upgrade() else {
                    break;
                };
                let killed = reap_conversation_states(&states, idle_timeout);
                for session in killed {
                    kill_exec_session(&session);
                }
            }
        });
    }
}

fn reaper_interval(idle_timeout: Duration) -> Duration {
    idle_timeout
        .min(Duration::from_secs(30))
        .max(Duration::from_secs(1))
}

fn reap_conversation_states(
    states: &StdMutex<ConversationExecStates>,
    idle_timeout: Duration,
) -> Vec<Arc<ExecSession>> {
    let mut killed = Vec::new();
    let mut states = states.lock().unwrap();
    states.retain(|_, state| {
        killed.extend(state.reap(idle_timeout));
        // A concurrent tool call owns another Arc. Keep the map entry until that
        // call finishes so it cannot publish a session into a detached state.
        !(state.is_empty() && Arc::strong_count(state) == 1)
    });
    killed
}

/// Per-MCP-transport mutable state. The fallback project root and current plan
/// remain transport-scoped. Resident commands use this transport-owned exec
/// state for generic MCP clients, while ChatGPT calls receive a temporary view
/// backed by [`ConversationExecSessionStore`].
pub struct SessionState {
    exec: Arc<ExecSessionState>,
    pub plan: Arc<StdMutex<Option<PlanState>>>,
    project_root: Arc<StdMutex<Option<PathBuf>>>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            exec: Arc::new(ExecSessionState::new()),
            plan: Arc::new(StdMutex::new(None)),
            project_root: Arc::new(StdMutex::new(None)),
        }
    }
}

impl SessionState {
    pub fn new() -> Self {
        Self::default()
    }

    fn with_exec_state(&self, exec: Arc<ExecSessionState>) -> Self {
        Self {
            exec,
            plan: self.plan.clone(),
            project_root: self.project_root.clone(),
        }
    }

    /// Starts cleanup for generic-client, transport-owned resident commands.
    pub fn spawn_idle_reaper(&self, idle_timeout: Duration) {
        self.exec.spawn_idle_reaper(idle_timeout);
    }

    pub fn exec_session(&self, id: u64) -> Option<Arc<ExecSession>> {
        self.exec.session(id)
    }

    pub fn exec_session_ids(&self) -> Vec<u64> {
        self.exec.session_ids()
    }

    pub fn remove_exec_session(&self, id: u64) -> Option<Arc<ExecSession>> {
        self.exec.remove(id)
    }

    fn exec_session_count(&self) -> usize {
        self.exec.len()
    }

    fn reap_exec_sessions(&self, idle_timeout: Duration) -> Vec<Arc<ExecSession>> {
        self.exec.reap(idle_timeout)
    }

    pub fn effective_config(&self, config: &AppConfig) -> Result<AppConfig, String> {
        if !config.multi_project {
            return Ok(config.clone());
        }

        let Some(project_root) = self.project_root.lock().unwrap().clone() else {
            return Err(format!(
                "No project root is selected for this MCP transport session. If the exact path is unknown, call `list_projects` first. Then call `set_project_root` with a directory relative to the access root `{}`, followed by `get_agent_brief` before using project tools.",
                config.work_dir.display()
            ));
        };

        let mut effective = config.clone();
        effective.work_dir = project_root;
        Ok(effective)
    }

    pub fn select_project_root(
        &self,
        config: &AppConfig,
        input: &str,
    ) -> Result<ProjectRootSelection, String> {
        if !config.multi_project {
            return Err(
                "Project-root selection is disabled. Start codexify with `--multi-project` or set `multiProject` to true."
                    .to_string(),
            );
        }

        let (access_root, project_root) = resolve_project_root(config, input)?;

        let mut selected = self.project_root.lock().unwrap();
        match selected.as_ref() {
            Some(current) if current == &project_root => Ok(ProjectRootSelection {
                access_root,
                project_root,
                newly_selected: false,
                scope: ProjectBindingScope::McpTransportSession,
            }),
            Some(current) => Err(format!(
                "This MCP transport session is already bound to project root `{}` and cannot switch to `{}`. Open a new session for another project.",
                current.display(),
                project_root.display()
            )),
            None => {
                *selected = Some(project_root.clone());
                Ok(ProjectRootSelection {
                    access_root,
                    project_root,
                    newly_selected: true,
                    scope: ProjectBindingScope::McpTransportSession,
                })
            }
        }
    }

    pub fn selected_project_root(&self) -> Option<PathBuf> {
        self.project_root.lock().unwrap().clone()
    }
}

/// Removes finished-and-empty sessions and, when `idle_timeout` is non-zero,
/// removes every session idle beyond it. Running idle sessions are returned so
/// the caller can kill them without holding the map lock; completed sessions
/// whose final output was never polled are simply expired.
fn reap_sessions(
    map: &mut HashMap<u64, Arc<ExecSession>>,
    idle_timeout: Duration,
) -> Vec<Arc<ExecSession>> {
    let mut killed = Vec::new();
    map.retain(|_, s| {
        if s.exit_code().is_some() && s.pending.lock().unwrap().is_empty() {
            return false;
        }
        if !idle_timeout.is_zero() && s.idle_for() >= idle_timeout {
            if s.exit_code().is_none() {
                killed.push(s.clone());
            }
            return false;
        }
        true
    });
    killed
}

/// Removes sessions whose process already exited and left nothing buffered.
/// Idle enforcement is the background reaper's job (see `spawn_idle_reaper`),
/// so this opportunistic pass never kills a running session.
pub fn reap_finished_sessions(state: &SessionState) {
    let _ = state.reap_exec_sessions(Duration::ZERO);
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
        let live = state.exec_session_count();
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

    let pending = Arc::new(StdMutex::new(PendingBuffer::new()));
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

    let id = state.exec.next_id();
    let session = Arc::new(ExecSession {
        id,
        command: cmd.to_string(),
        pid,
        started_at: Instant::now(),
        stdin: TokioMutex::new(stdin),
        pending,
        exit_code,
        drain_done,
        last_activity: StdMutex::new(Instant::now()),
    });

    state.exec.insert(session.clone());
    Ok(session)
}

fn finish_drain(remaining: &Arc<AtomicUsize>, done: &Arc<Notify>) {
    if remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
        done.notify_waiters();
    }
}

fn spawn_drain<R>(
    mut reader: R,
    pending: Arc<StdMutex<PendingBuffer>>,
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
    if session.exit_code().is_none() {
        kill_pid(session.pid);
    }
}

static CHUNK_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn generate_chunk_id() -> String {
    let n = CHUNK_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
    format!("chunk-{n}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool;
    use crate::tools::exec_command::ExecCommand;
    use crate::tools::write_stdin::WriteStdin;
    use crate::types::ExecMode;
    use serde_json::json;

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

    #[test]
    fn pending_buffer_passes_small_output_through_verbatim() {
        let mut buf = PendingBuffer::new();
        buf.push_str("hello ");
        buf.push_str("world");
        assert_eq!(buf.take(), "hello world");
        assert!(buf.is_empty());
    }

    #[test]
    fn pending_buffer_reconstructs_stream_when_under_cap() {
        let mut buf = PendingBuffer::new();
        // Just over the head cap but under the total cap: nothing is elided,
        // and head+tail must equal the original stream in order.
        let text = "x".repeat(PENDING_HEAD_BYTES + 1024);
        buf.push_str(&text);
        let out = buf.take();
        assert_eq!(out, text);
        assert!(!out.contains("elided"));
    }

    #[test]
    fn pending_buffer_elides_middle_past_cap_and_bounds_memory() {
        let mut buf = PendingBuffer::new();
        // Feed far more than the cap in many chunks, as the drains would.
        for _ in 0..64 {
            buf.push_str(&"a".repeat(100_000)); // 6.4 MiB total
        }
        // Retained bytes stay bounded regardless of how much was written.
        assert!(buf.head.len() <= PENDING_HEAD_BYTES);
        assert!(buf.tail.len() <= PENDING_TAIL_BYTES);
        assert!(buf.omitted > 0);
        let out = buf.take();
        assert!(out.contains("elided"));
        assert!(out.starts_with("aaaa"));
        assert!(out.ends_with("aaaa"));
        assert!(buf.is_empty());
    }

    /// A command that keeps the shell resident for a while, picked to match the
    /// platform's default shell so the test works on POSIX and Windows alike.
    fn resident_sleep_command() -> String {
        match shell_type_of(&default_shell_bin()) {
            ShellType::PowerShell => "Start-Sleep -Seconds 30".to_string(),
            ShellType::Cmd => "ping 127.0.0.1 -n 30 > nul".to_string(),
            ShellType::Posix => "sleep 30".to_string(),
        }
    }

    #[tokio::test]
    async fn conversation_owned_exec_session_survives_replacement_transport() {
        let dir = std::env::temp_dir();
        let mut config = crate::config::default_config(dir.clone());
        config.exec.mode = ExecMode::Unrestricted;
        let store = ConversationExecSessionStore::new();
        let identity =
            ConversationIdentity::from_openai_session("conversation-resident-process").unwrap();

        let first_transport = SessionState::new();
        let first_call = store.session_for(&identity, &first_transport);
        let started = ExecCommand
            .call(
                json!({
                    "cmd": resident_sleep_command(),
                    "yield_time_ms": EXEC_MIN_YIELD_MS
                }),
                &config,
                &first_call,
            )
            .await;
        assert!(!started.is_error, "{}", started.joined_text());
        let session_id = started
            .structured_content
            .as_ref()
            .and_then(|value| value.get("session_id"))
            .and_then(serde_json::Value::as_u64)
            .expect("resident command should return a session id");
        let resident = first_call.exec_session(session_id).unwrap();

        drop(first_call);
        drop(first_transport);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(resident.exit_code().is_none());

        let replacement_transport = SessionState::new();
        let replacement_call = store.session_for(&identity, &replacement_transport);
        let resumed = WriteStdin
            .call(
                json!({
                    "session_id": session_id,
                    "chars": "x",
                    "yield_time_ms": 1
                }),
                &config,
                &replacement_call,
            )
            .await;
        assert!(!resumed.is_error, "{}", resumed.joined_text());
        assert_eq!(
            resumed
                .structured_content
                .as_ref()
                .and_then(|value| value.get("session_id"))
                .and_then(serde_json::Value::as_u64),
            Some(session_id)
        );

        replacement_call.remove_exec_session(session_id);
        kill_exec_session(&resident);
    }

    #[test]
    fn conversation_exec_states_are_isolated_and_empty_states_are_reclaimed() {
        let store = ConversationExecSessionStore::new();
        let transport = SessionState::new();
        let first = ConversationIdentity::from_openai_session("first-conversation").unwrap();
        let second = ConversationIdentity::from_openai_session("second-conversation").unwrap();

        let first_call = store.session_for(&first, &transport);
        let second_call = store.session_for(&second, &transport);
        assert!(!Arc::ptr_eq(&first_call.exec, &second_call.exec));
        assert_eq!(store.states.lock().unwrap().len(), 2);

        assert!(reap_conversation_states(&store.states, Duration::ZERO).is_empty());
        assert_eq!(store.states.lock().unwrap().len(), 2);

        drop(first_call);
        drop(second_call);
        assert!(reap_conversation_states(&store.states, Duration::ZERO).is_empty());
        assert!(store.states.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reap_sessions_only_kills_running_sessions_past_the_idle_timeout() {
        let dir = std::env::temp_dir();
        let config = crate::config::default_config(dir.clone());
        let state = SessionState::new();
        let session =
            start_exec_session(&state, &config, &resident_sleep_command(), &dir, None).unwrap();
        assert!(session.exit_code().is_none());
        assert_eq!(state.exec_session_count(), 1);

        // Disabled (0) never touches a running session.
        assert!(state.reap_exec_sessions(Duration::ZERO).is_empty());
        assert_eq!(state.exec_session_count(), 1);
        // Still well within a generous timeout.
        assert!(
            state
                .reap_exec_sessions(Duration::from_secs(3600))
                .is_empty()
        );
        assert_eq!(state.exec_session_count(), 1);

        // Once idle past a tiny timeout, it is selected, killed and removed.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let killed = state.reap_exec_sessions(Duration::from_millis(50));
        assert_eq!(killed.len(), 1);
        for session in &killed {
            kill_exec_session(session);
        }
        assert_eq!(state.exec_session_count(), 0);
    }

    #[tokio::test]
    async fn touch_resets_the_idle_clock() {
        let dir = std::env::temp_dir();
        let config = crate::config::default_config(dir.clone());
        let state = SessionState::new();
        let session =
            start_exec_session(&state, &config, &resident_sleep_command(), &dir, None).unwrap();

        tokio::time::sleep(Duration::from_millis(80)).await;
        session.touch(); // a fresh write_stdin/yield would do this
        assert!(
            state
                .reap_exec_sessions(Duration::from_millis(50))
                .is_empty()
        );
        kill_exec_session(&session);
    }

    #[tokio::test]
    async fn background_idle_reaper_removes_an_idle_session_then_stops_on_drop() {
        let dir = std::env::temp_dir();
        let mut config = crate::config::default_config(dir.clone());
        config.exec.idle_timeout_ms = 50;
        let state = SessionState::new();
        state.spawn_idle_reaper(Duration::from_millis(config.exec.idle_timeout_ms));
        let session =
            start_exec_session(&state, &config, &resident_sleep_command(), &dir, None).unwrap();
        let pid = session.pid;
        drop(session);

        // The reaper's check interval floors at 1s; give it a few ticks.
        let mut reaped = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if state.exec_session_count() == 0 {
                reaped = true;
                break;
            }
        }
        assert!(
            reaped,
            "background reaper should have removed the idle session"
        );
        // Best-effort: make sure the process is gone even if the assert path changes.
        kill_pid(pid);
    }

    #[test]
    fn pending_buffer_keeps_utf8_boundaries_intact() {
        let mut buf = PendingBuffer::new();
        // A leading single byte pushes every following 2-byte char onto an odd
        // offset, so the even head/tail cut points land mid-codepoint — the
        // floor/ceil boundary logic must back off or take() panics.
        buf.push_str("x");
        for _ in 0..800_000 {
            buf.push_str("é"); // 2 bytes each → ~1.6 MiB, well over the cap
        }
        assert!(buf.head.len() <= PENDING_HEAD_BYTES);
        assert!(buf.tail.len() <= PENDING_TAIL_BYTES);
        assert!(buf.omitted > 0);
        let out = buf.take(); // must not panic on a char boundary
        assert!(out.starts_with("xé"));
        assert!(out.ends_with('é'));
    }
}
