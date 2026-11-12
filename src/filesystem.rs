//! Bounded filesystem work in killable child processes, shared across chats.
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::types::{
    AppConfig, FilesystemConfig, IgnoreConfig, OutputConfig, ToolResult, TreeConfig,
};

pub const WORKER_ARGUMENT: &str = "--internal-filesystem-worker";
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const MAX_TIMEOUT_MS: u64 = 120_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Operation {
    Grep,
    Glob,
    Tree,
    ReadFile,
    ListDirectory,
}

#[derive(Debug)]
pub struct FilesystemWorker {
    slots: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    executable: Option<PathBuf>,
    worker_limit: usize,
    queue_limit: usize,
}

impl FilesystemWorker {
    pub fn new(config: &FilesystemConfig, executable: Option<PathBuf>) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(config.max_concurrent_workers)),
            pending: Arc::new(Semaphore::new(
                config.max_concurrent_workers + config.max_pending_requests,
            )),
            executable,
            worker_limit: config.max_concurrent_workers,
            queue_limit: config.max_pending_requests,
        }
    }

    pub fn snapshot(&self) -> Value {
        let active = self
            .worker_limit
            .saturating_sub(self.slots.available_permits());
        let admitted =
            (self.worker_limit + self.queue_limit).saturating_sub(self.pending.available_permits());
        serde_json::json!({"occupiedWorkerSlots":active,"queuedRequests":admitted.saturating_sub(active),
            "workerLimit":self.worker_limit,"queueLimit":self.queue_limit})
    }

    pub async fn run(
        &self,
        operation: Operation,
        args: Value,
        config: &AppConfig,
        cancellation: &CancellationToken,
        executable: Option<&Path>,
    ) -> ToolResult {
        let timeout_ms = if args.get("timeoutMs").is_some() {
            match crate::tool::arg_u64(&args, "timeoutMs") {
                Some(value) => value,
                None => return ToolResult::error("timeoutMs must be an integer."),
            }
        } else {
            DEFAULT_TIMEOUT_MS
        };
        if !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
            return ToolResult::error(format!("timeoutMs must be between 1 and {MAX_TIMEOUT_MS}"));
        }
        if cancellation.is_cancelled() {
            return cancelled_result(operation);
        }
        let Ok(_pending) = self.pending.clone().try_acquire_owned() else {
            return ToolResult::error(
                "Filesystem queue is full. Wait for an existing operation to finish.",
            );
        };
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let _slot = crate::audit::stage("filesystem_queue", async {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(cancelled_result(operation)),
                _ = tokio::time::sleep_until(deadline) => Err(timeout_result(operation, timeout_ms)),
                slot = self.slots.clone().acquire_owned() => Ok(slot.expect("filesystem semaphore stays open")),
            }
        }).await;
        let _slot = match _slot {
            Ok(slot) => slot,
            Err(result) => return result,
        };
        let executable = executable
            .map(Path::to_path_buf)
            .or_else(|| self.executable.clone())
            .map(Ok)
            .unwrap_or_else(std::env::current_exe);
        let executable = match executable {
            Ok(path) => path,
            Err(error) => return ToolResult::error(format!("Locate filesystem worker: {error}")),
        };
        let request = WorkerRequest {
            operation,
            args,
            work_dir: config.work_dir.clone(),
            ignore: config.ignore.clone(),
            tree: config.tree.clone(),
            output: config.output.clone(),
            filesystem: config.filesystem.clone(),
        };
        let payload = match serde_json::to_vec(&request) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(format!("Prepare filesystem request: {error}")),
        };
        let mut command = tokio::process::Command::new(executable);
        command
            .arg(WORKER_ARGUMENT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        crate::process_env::scrub_untrusted_child_env(&mut command, config);
        #[cfg(windows)]
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        let record_worker = crate::audit::worker_observer("filesystem");
        let mut spawning = tokio::task::spawn_blocking(move || {
            let child = command.spawn()?;
            record_worker(child.id());
            Ok::<_, std::io::Error>(WorkerLease {
                child: Some(child),
                permits: Some((_slot, _pending)),
            })
        });
        let lease = crate::audit::stage("filesystem_spawn", async {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(cancelled_result(operation)),
                _ = tokio::time::sleep_until(deadline) => Err(timeout_result(operation, timeout_ms)),
                result = &mut spawning => match result {
                    Ok(Ok(lease)) => Ok(lease),
                    Ok(Err(error)) => Err(ToolResult::error(format!("Start filesystem worker: {error}"))),
                    Err(error) => Err(ToolResult::error(format!("Filesystem spawn failed: {error}"))),
                },
            }
        }).await;
        let mut lease = match lease {
            Ok(lease) => lease,
            Err(result) => return result,
        };
        let child = lease.child.as_mut().unwrap();
        let pid = child.id();
        let mut input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        // A text response may need six bytes of JSON escaping per UTF-8 byte.
        let response_limit = crate::output_budget::approx_bytes_for_tokens(
            crate::output_budget::tool_output_token_budget(config),
        )
        .saturating_mul(6)
        .saturating_add(8192) as u64;
        let result = crate::audit::stage("filesystem_execution", async {
            let work = async {
                input.write_all(&payload).await?;
                drop(input);
                let mut bytes = Vec::new();
                output.take(response_limit.saturating_add(1)).read_to_end(&mut bytes).await?;
                if bytes.len() as u64 > response_limit {
                    return Ok(ToolResult::error("Filesystem worker exceeded its response budget."));
                }
                let exit = child.wait().await?;
                if !exit.success() { return Ok(ToolResult::error(format!("Filesystem worker exited with {exit}"))); }
                let response: WorkerResponse = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
                let mut result = if response.is_error { ToolResult::error(response.text) } else { ToolResult::text(response.text) };
                result.audit.truncated = response.truncated;
                result.audit.original_output_tokens = response.original_output_tokens;
                Ok::<_, std::io::Error>(result)
            };
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => cancelled_result(operation),
                _ = tokio::time::sleep_until(deadline) => timeout_result(operation, timeout_ms),
                result = work => result.unwrap_or_else(|error| ToolResult::error(format!("Read filesystem response: {error}"))),
            }
        }).await;
        if child.id().is_some() {
            let cleanup = crate::audit::stage(
                "filesystem_cleanup",
                tokio::time::timeout(Duration::from_secs(1), child.kill()),
            )
            .await;
            if !matches!(cleanup, Ok(Ok(()))) {
                return ToolResult::error(format!(
                    "{} Worker termination requested; exit is not yet confirmed.",
                    result.joined_text()
                ));
            }
        }
        let mut result = result;
        result.audit.process_id = pid;
        result
    }
}

/// Keep admission permits until the child is reaped, including when a caller
/// abandons its future or kernel I/O delays termination. Cleanup never pins HTTP.
struct WorkerLease {
    child: Option<tokio::process::Child>,
    permits: Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)>,
}
impl Drop for WorkerLease {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if child.id().is_none() {
            return;
        }
        let _ = child.start_kill();
        let permits = self.permits.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _permits = permits;
                let _ = child.wait().await;
            });
        }
    }
}

fn cancelled_result(operation: Operation) -> ToolResult {
    ToolResult::error(if matches!(operation, Operation::Grep) {
        "Search cancelled."
    } else {
        "Filesystem operation cancelled."
    })
}

fn timeout_result(operation: Operation, timeout_ms: u64) -> ToolResult {
    let name = if matches!(operation, Operation::Grep) {
        "Search"
    } else {
        "Filesystem operation"
    };
    ToolResult::error(format!(
        "{name} timed out after {timeout_ms} ms. Narrow the request or increase timeoutMs."
    ))
}

#[derive(Serialize, Deserialize)]
struct WorkerRequest {
    operation: Operation,
    args: Value,
    work_dir: PathBuf,
    ignore: IgnoreConfig,
    tree: TreeConfig,
    output: OutputConfig,
    filesystem: FilesystemConfig,
}
#[derive(Serialize, Deserialize)]
struct WorkerResponse {
    text: String,
    is_error: bool,
    truncated: Option<bool>,
    original_output_tokens: Option<u64>,
}

pub fn run_worker() -> Result<(), String> {
    let request: WorkerRequest =
        serde_json::from_reader(std::io::stdin().lock()).map_err(|error| error.to_string())?;
    let mut config = crate::config::default_config(request.work_dir);
    config.ignore = request.ignore;
    config.tree = request.tree;
    config.output = request.output;
    config.filesystem = request.filesystem;
    SCAN.with(|state| *state.borrow_mut() = ScanState::new(&config.filesystem));
    let mut result = match request.operation {
        Operation::Grep => crate::tools::grep::Grep::search(request.args, &config),
        Operation::Glob => crate::tools::glob::Glob::run_inner(request.args, &config),
        Operation::Tree => crate::tools::tree::Tree::run_inner(request.args, &config),
        Operation::ReadFile => crate::tools::read_file::ReadFile::run_inner(request.args, &config),
        Operation::ListDirectory => {
            crate::tools::list_directory::ListDirectory::run_inner(request.args, &config)
        }
    };
    let notice = SCAN.with(|state| state.borrow().notice());
    if !notice.is_empty() && !result.is_error {
        result = ToolResult::text(
            crate::output_budget::truncate_text(
                &format!("{}\n\nIncomplete scan: {notice}", result.joined_text()),
                crate::output_budget::tool_output_token_budget(&config),
            )
            .text,
        )
        .with_truncation(true);
    }
    crate::output_budget::enforce_tool_result_budget(
        &mut result,
        crate::output_budget::tool_output_token_budget(&config),
    );
    let response = WorkerResponse {
        text: result.joined_text(),
        is_error: result.is_error,
        truncated: result.audit.truncated,
        original_output_tokens: result.audit.original_output_tokens,
    };
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &response).map_err(|error| error.to_string())?;
    out.flush().map_err(|error| error.to_string())
}

struct ScanState {
    bytes_left: u64,
    entries_left: usize,
    max_line: usize,
    bytes_exhausted: bool,
    entries_exhausted: bool,
    long_lines: bool,
}
impl ScanState {
    fn new(config: &FilesystemConfig) -> Self {
        Self {
            bytes_left: config.max_scan_bytes,
            entries_left: config.max_scan_entries,
            max_line: config.max_line_bytes,
            bytes_exhausted: false,
            entries_exhausted: false,
            long_lines: false,
        }
    }
    fn notice(&self) -> String {
        let mut reasons = Vec::new();
        if self.bytes_exhausted {
            reasons.push("input byte budget reached");
        }
        if self.entries_exhausted {
            reasons.push("directory-entry budget reached");
        }
        if self.long_lines {
            reasons.push("overlong input lines were elided");
        }
        reasons.join("; ")
    }
}
thread_local! { static SCAN: RefCell<ScanState> = RefCell::new(ScanState::new(&FilesystemConfig::default())); }

pub(crate) fn visit_entry() -> bool {
    SCAN.with(|state| {
        let mut state = state.borrow_mut();
        if state.entries_left == 0 {
            state.entries_exhausted = true;
            false
        } else {
            state.entries_left -= 1;
            true
        }
    })
}

/// Retain at most one bounded line. Discard excess bytes without allocating,
/// and count skipped lines and skipped line tails against the input budget.
struct BudgetReader(std::fs::File);
impl Read for BudgetReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let left = SCAN.with(|state| state.borrow().bytes_left);
        if left == 0 {
            SCAN.with(|state| state.borrow_mut().bytes_exhausted = true);
            return Ok(0);
        }
        let length = output
            .len()
            .min(usize::try_from(left).unwrap_or(usize::MAX));
        let count = self.0.read(&mut output[..length])?;
        SCAN.with(|state| state.borrow_mut().bytes_left -= count as u64);
        Ok(count)
    }
}

pub(crate) struct Lines {
    reader: BufReader<BudgetReader>,
    trailing_empty: bool,
    finished: bool,
}
impl Lines {
    pub(crate) fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            reader: BufReader::new(BudgetReader(std::fs::File::open(path)?)),
            trailing_empty: true,
            finished: false,
        })
    }
    pub(crate) fn next(&mut self) -> std::io::Result<Option<String>> {
        if self.finished {
            return Ok(None);
        }
        let max_line = SCAN.with(|state| state.borrow().max_line);
        let mut line = Vec::new();
        loop {
            let chunk = self.reader.fill_buf()?;
            if chunk.is_empty() {
                self.finished = true;
                return Ok(if !line.is_empty() || self.trailing_empty {
                    Some(String::from_utf8_lossy(&line).into_owned())
                } else {
                    None
                });
            }
            let available = chunk.len();
            let newline = chunk[..available].iter().position(|b| *b == b'\n');
            let consumed = newline.map_or(available, |i| i + 1);
            let content = newline.unwrap_or(consumed);
            let retained = content.min(max_line.saturating_sub(line.len()));
            line.extend_from_slice(&chunk[..retained]);
            SCAN.with(|state| {
                let mut state = state.borrow_mut();
                state.long_lines |= retained < content;
            });
            self.reader.consume(consumed);
            self.trailing_empty = newline.is_some();
            if newline.is_some() {
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn queued_filesystem_requests_expire_and_cancel_before_spawning() {
        let config = crate::config::default_config(PathBuf::from("/unused"));
        let worker = FilesystemWorker::new(&config.filesystem, Some("/does-not-exist".into()));
        let _slots = worker
            .slots
            .acquire_many(config.filesystem.max_concurrent_workers as u32)
            .await
            .unwrap();
        let result = worker
            .run(
                Operation::Grep,
                json!({"pattern":"x","timeoutMs":10}),
                &config,
                &CancellationToken::new(),
                None,
            )
            .await;
        assert!(result.joined_text().contains("timed out after 10 ms"));
        let cancellation = CancellationToken::new();
        let work = worker.run(
            Operation::ReadFile,
            json!({"path":"x"}),
            &config,
            &cancellation,
            None,
        );
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(work, cancel);
        assert!(result.joined_text().contains("cancelled"));
    }

    #[tokio::test]
    async fn filesystem_pending_queue_and_deadlines_are_bounded() {
        let config = crate::config::default_config(PathBuf::from("/unused"));
        let worker = FilesystemWorker::new(&config.filesystem, Some("/does-not-exist".into()));
        let _pending = worker
            .pending
            .acquire_many(
                (config.filesystem.max_concurrent_workers + config.filesystem.max_pending_requests)
                    as u32,
            )
            .await
            .unwrap();
        let result = worker
            .run(
                Operation::Grep,
                json!({"pattern":"x"}),
                &config,
                &CancellationToken::new(),
                None,
            )
            .await;
        assert!(result.joined_text().contains("queue is full"));
        for value in [json!(0), json!(120001), json!("5")] {
            let result = worker
                .run(
                    Operation::Grep,
                    json!({"pattern":"x","timeoutMs":value}),
                    &config,
                    &CancellationToken::new(),
                    None,
                )
                .await;
            assert!(result.joined_text().contains("timeoutMs"));
        }
    }
}
