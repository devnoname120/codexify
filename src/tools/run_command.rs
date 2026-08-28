use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;

use crate::exec_sessions::SessionState;
use crate::output_budget::{
    BoundedTextBuffer, approx_token_count, tool_output_token_budget, truncate_text,
};
use crate::process_env::scrub_untrusted_child_env;
use crate::tool::{Tool, ToolBehavior, parse_tool_args, schema_for, text_output_schema};
use crate::types::{AppConfig, ToolAuditMetadata, ToolContent, ToolResult};

pub struct RunCommand;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RunCommandArgs {
    /// Allowlisted executable name.
    #[schemars(length(min = 1))]
    command: String,
    /// Arguments passed directly to the executable without shell parsing.
    #[serde(default)]
    args: Vec<String>,
    /// Timeout in milliseconds. Defaults to the configured command timeout.
    #[schemars(range(min = 1))]
    timeout: Option<u64>,
}

const RUN_COMMAND_STREAM_MAX_BYTES: usize = 512 * 1024;
const DRAIN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

#[async_trait]
impl Tool for RunCommand {
    fn name(&self) -> &'static str {
        "run_command"
    }

    fn title(&self) -> String {
        "Run command".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            true,
            false,
            true,
            "The selected executable can modify or delete local state and can contact external systems.",
        )
    }

    fn description(&self) -> String {
        "Execute an allowlisted binary with an argument array in the project directory. Returns bounded stdout, stderr, and exit code; timed-out commands return bounded partial output. Use exec_command instead when shell syntax such as pipes or redirects is required. The timeout defaults to the server's configured command timeout.".into()
    }

    fn input_schema(&self) -> Value {
        schema_for::<RunCommandArgs>()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    fn may_modify_project(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let RunCommandArgs {
            command,
            args: cmd_args,
            timeout,
        } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        let timeout = timeout
            .unwrap_or(config.command.default_timeout)
            .min(config.command.max_timeout);

        if !config.allowed_commands.iter().any(|c| c == &command) {
            return ToolResult::error(format!(
                "Command not allowed: \"{}\". Allowed: {}",
                command,
                config.allowed_commands.join(", ")
            ));
        }

        let mut cmd = Command::new(&command);
        cmd.args(&cmd_args)
            .current_dir(&config.work_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        scrub_untrusted_child_env(&mut cmd, config);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolResult::error(e.to_string()),
        };

        let stdout = Arc::new(StdMutex::new(BoundedTextBuffer::<
            RUN_COMMAND_STREAM_MAX_BYTES,
        >::new()));
        let stderr = Arc::new(StdMutex::new(BoundedTextBuffer::<
            RUN_COMMAND_STREAM_MAX_BYTES,
        >::new()));
        let stdout_drain = child
            .stdout
            .take()
            .map(|stream| spawn_bounded_drain(stream, stdout.clone()));
        let stderr_drain = child
            .stderr
            .take()
            .map(|stream| spawn_bounded_drain(stream, stderr.clone()));

        let waited = tokio::time::timeout(Duration::from_millis(timeout), child.wait()).await;
        let (timed_out, status) = match waited {
            Ok(Ok(status)) => (false, Some(status)),
            Ok(Err(error)) => {
                let _ = child.kill().await;
                let _ = settle_drain(stdout_drain).await;
                let _ = settle_drain(stderr_drain).await;
                return ToolResult::error(error.to_string());
            }
            Err(_) => {
                let _ = child.kill().await;
                let status = child.wait().await.ok();
                (true, status)
            }
        };

        let stdout_settled = settle_drain(stdout_drain).await;
        let stderr_settled = settle_drain(stderr_drain).await;

        let stdout = stdout
            .lock()
            .unwrap()
            .take("run_command stdout capture limit");
        let stderr = stderr
            .lock()
            .unwrap()
            .take("run_command stderr capture limit");
        let exit_code = status.and_then(|status| status.code()).unwrap_or(-1);

        let mut out = String::new();
        if !stdout.text.is_empty() {
            out.push_str(&stdout.text);
        }
        if !stderr.text.is_empty() {
            if !out.is_empty() {
                out.push_str("\n--- stderr ---\n");
            }
            out.push_str(&stderr.text);
        }
        if out.is_empty() {
            out.push_str("(no output)");
        }
        if timed_out {
            out.push_str(&format!(
                "\n\ncommand timed out after {}s",
                timeout as f64 / 1000.0
            ));
        } else {
            out.push_str(&format!("\n\nexit code: {exit_code}"));
        }
        if !stdout_settled || !stderr_settled {
            out.push_str("\n\n[... output pipe remained open; later bytes omitted ...]");
        }

        let bounded = truncate_text(&out, tool_output_token_budget(config));
        let capture_truncated =
            stdout.truncated || stderr.truncated || !stdout_settled || !stderr_settled;
        let original_output_tokens = bounded.original_token_count.saturating_add(
            stdout
                .omitted_bytes
                .saturating_add(stderr.omitted_bytes)
                .div_ceil(4),
        );

        ToolResult {
            content: vec![ToolContent::Text(bounded.text)],
            is_error: timed_out || exit_code != 0,
            structured_content: None,
            meta: None,
            audit: ToolAuditMetadata {
                truncated: Some(capture_truncated || bounded.truncated),
                original_output_tokens: (capture_truncated || bounded.truncated)
                    .then_some(original_output_tokens.max(approx_token_count(&out))),
                ..Default::default()
            },
        }
    }
}

fn spawn_bounded_drain<R>(
    mut reader: R,
    buffer: Arc<StdMutex<BoundedTextBuffer<RUN_COMMAND_STREAM_MAX_BYTES>>>,
) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    buffer.lock().unwrap().push_bytes(&chunk[..read]);
                }
            }
        }
    })
}

async fn settle_drain(handle: Option<JoinHandle<()>>) -> bool {
    let Some(mut handle) = handle else {
        return true;
    };
    if tokio::time::timeout(DRAIN_SHUTDOWN_TIMEOUT, &mut handle)
        .await
        .is_err()
    {
        handle.abort();
        let _ = handle.await;
        false
    } else {
        true
    }
}
