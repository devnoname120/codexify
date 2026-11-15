use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::exec_sessions::{
    DEFAULT_MAX_OUTPUT_TOKENS, EXEC_DEFAULT_YIELD_MS, EXEC_MAX_YIELD_MS, EXEC_MIN_YIELD_MS,
    ExecSession, SessionState, ShellType, UnifiedExecOutput, clamp, generate_chunk_id,
    resolve_shell, shell_type_of, start_exec_session, truncate_output,
};
use crate::output_budget::resolve_requested_output_tokens;
use crate::safe_path::resolve_safe_path;
use crate::tool::{Tool, ToolBehavior, ToolRequestContext, parse_tool_args};
use crate::types::{AppConfig, ToolAuditMetadata, ToolResult};

/// The output schema shared by `exec_command` and `write_stdin`.
pub fn unified_exec_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "chunk_id": { "type": "string", "description": "Chunk identifier included when the response reports one." },
            "wall_time_seconds": { "type": "number", "description": "Elapsed wall time spent waiting for output in seconds." },
            "exit_code": { "type": "integer", "description": "Process exit code when the command finished during this call." },
            "session_id": { "type": "integer", "minimum": 1, "description": "Session identifier to pass to write_stdin when the process is still running." },
            "original_token_count": { "type": "integer", "minimum": 0, "description": "Approximate token count before output truncation." },
            "output": { "type": "string", "description": "Command output text, possibly truncated." }
        },
        "required": ["chunk_id", "wall_time_seconds", "original_token_count", "output"],
        "oneOf": [
            {
                "required": ["exit_code"],
                "not": { "required": ["session_id"] }
            },
            {
                "required": ["session_id"],
                "not": { "required": ["exit_code"] }
            }
        ],
        "additionalProperties": false
    })
}

pub fn render_unified_exec_output(result: &UnifiedExecOutput) -> String {
    serde_json::to_string_pretty(result).unwrap_or_else(|_| "{}".to_string())
}

const BASE_DESCRIPTION: &str = "Runs a command in a shell and returns its output. Use this for anything the structured tools do not cover: build pipelines, test runners, package managers, interactive REPLs.\n\nIf the command finishes within yield-time_ms the full result is returned with its exit_code. If it is still running, a session_id comes back instead — pass that to write_stdin to send input, poll for more output, or wait for it to finish. That makes long-running and interactive processes workable in a single session.\n\nCommands run through the platform shell, so pipes, redirection and && chains work. This bridge uses plain pipes rather than a PTY. The optional shell value follows Codex semantics: it selects a recognized shell type by basename, and is never executed as an arbitrary model-provided path.";

const WINDOWS_SHELL_GUIDANCE: &str = "Windows safety rules:\n- Do not compose destructive filesystem commands across shells. Do not enumerate paths in PowerShell and then pass them to `cmd /c`, batch builtins, or another shell for deletion or moving. Use one shell end-to-end, prefer native PowerShell cmdlets such as `Remove-Item` / `Move-Item` with `-LiteralPath`, and avoid string-built shell commands for file operations.\n- Before any recursive delete or move on Windows, verify the resolved absolute target paths stay within the intended workspace or explicitly named target directory. Never issue a recursive delete or move against a computed path if the final target has not been checked.\n- When using `Start-Process` to launch a background helper or service, pass `-WindowStyle Hidden` unless the user explicitly asked for a visible interactive window. Use visible windows only for interactive tools the user needs to see or control.";

pub fn exec_command_description() -> String {
    if cfg!(windows) {
        format!("{BASE_DESCRIPTION}\n\n{WINDOWS_SHELL_GUIDANCE}")
    } else {
        BASE_DESCRIPTION.to_string()
    }
}

fn syntax_hint(shell: ShellType) -> &'static str {
    match shell {
        ShellType::PowerShell => {
            "Commands are PowerShell, so `ls`, `cat` and `rm` are cmdlet aliases with different flags — prefer `Get-ChildItem`, `Get-Content`, `Remove-Item`, and `$env:FOO='bar'` for environment variables."
        }
        ShellType::Cmd => {
            "Commands are cmd.exe, so use `dir`, `%VAR%` and `copy` rather than POSIX equivalents."
        }
        ShellType::Posix => "Commands are POSIX shell, so the usual utilities and syntax apply.",
    }
}

/// Names the shell that will actually run, saving the model a `get_environment`
/// call and a wrong first guess.
pub fn describe_exec_command(config: &AppConfig) -> String {
    let parts = resolve_shell(config.exec.default_shell.as_deref());
    let bin = &parts[0];
    let shell = shell_type_of(bin);
    format!(
        "{}\n\nThis server runs commands with: {} ({}). {}",
        exec_command_description(),
        bin,
        shell.as_str(),
        syntax_hint(shell)
    )
}

pub struct ExecCommand;

pub(super) fn command_chat(
    config: &AppConfig,
    session: &SessionState,
    context: Option<&ToolRequestContext>,
) -> Option<Arc<crate::markdown_chat::ChatFile>> {
    if !config.markdown_chat.enabled {
        return None;
    }
    let context = context?;
    match context
        .markdown_chat
        .chat(config, context.task_conversation.as_ref(), session)
    {
        Ok(chat) => Some(chat),
        Err(error) => {
            tracing::debug!(%error, "owner message observation unavailable during command output wait");
            None
        }
    }
}

pub(super) async fn wait_for_command_output(
    exec_session: &ExecSession,
    yield_ms: u64,
    chat: Option<&Arc<crate::markdown_chat::ChatFile>>,
    cancellation: &CancellationToken,
) -> Result<(String, bool, bool), String> {
    let output = crate::audit::stage(
        "command_output_wait",
        exec_session.yield_output_with_metadata(yield_ms),
    );
    tokio::pin!(output);
    if let Some(chat) = chat {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err("Output wait cancelled; the command session remains available.".into()),
            message = chat.wait_for_unread(cancellation.clone()) => match message {
                // End only the output wait. Draining does not signal the child,
                // resend stdin, consume owner text, or change the session ID.
                Ok(()) => return Ok(exec_session.yield_output_with_metadata(0).await),
                Err(error) => tracing::debug!(%error, "owner message observation failed; retaining command output wait"),
            },
            result = &mut output => return Ok(result),
        }
    }
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err("Output wait cancelled; the command session remains available.".into()),
        result = &mut output => Ok(result),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecCommandArgs {
    cmd: String,
    workdir: Option<String>,
    tty: Option<bool>,
    yield_time_ms: Option<u64>,
    max_output_tokens: Option<u64>,
    shell: Option<String>,
}

#[async_trait]
impl Tool for ExecCommand {
    fn name(&self) -> &'static str {
        "exec_command"
    }

    fn title(&self) -> String {
        "Execute shell command".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            true,
            false,
            true,
            "A shell command can modify or delete local state and interact with external systems.",
        )
    }

    fn description(&self) -> String {
        exec_command_description()
    }

    fn describe(&self, config: &AppConfig) -> String {
        describe_exec_command(config)
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string", "minLength": 1, "description": "Shell command to execute." },
                "workdir": { "type": "string", "description": "Working directory for the command, relative to the project root. Defaults to the project root." },
                "tty": { "type": "boolean", "const": false, "description": "Commands use plain pipes; only false is supported." },
                "yield_time_ms": { "type": "integer", "minimum": 0, "description": format!("Wait before yielding output. Defaults to {EXEC_DEFAULT_YIELD_MS} ms and is clamped to the effective range {EXEC_MIN_YIELD_MS}-{EXEC_MAX_YIELD_MS} ms (at least {EXEC_DEFAULT_YIELD_MS} ms on Windows).") },
                "max_output_tokens": { "type": "integer", "minimum": 0, "description": format!("Output token budget. Zero or omission uses the {DEFAULT_MAX_OUTPUT_TOKENS}-token default; larger requests are capped by the server output policy and the middle of longer output is elided.") },
                "shell": { "type": "string", "minLength": 1, "description": "Recognized shell type selector such as bash, zsh, sh, powershell, pwsh, or cmd. A path is used only for its basename; the supplied path is never executed directly. Unrecognized or unavailable shell types use Codex's platform fallback (/bin/sh on POSIX, cmd.exe on Windows)." }
            },
            "required": ["cmd"],
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(unified_exec_output_schema())
    }

    fn uses_exec_session_state(&self) -> bool {
        true
    }

    fn manages_model_output_budget(&self) -> bool {
        true
    }

    fn may_modify_project(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, config: &AppConfig, session: &SessionState) -> ToolResult {
        self.run(args, config, session, None).await
    }

    async fn call_with_context(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        self.run(args, config, session, Some(context)).await
    }
}

impl ExecCommand {
    async fn run(
        &self,
        args: Value,
        config: &AppConfig,
        session: &SessionState,
        context: Option<&ToolRequestContext>,
    ) -> ToolResult {
        if context.is_some_and(|context| context.cancellation.is_cancelled()) {
            return ToolResult::error("Call cancelled before the command was started.");
        }
        let ExecCommandArgs {
            cmd,
            workdir,
            tty,
            yield_time_ms,
            max_output_tokens,
            shell,
        } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        if cmd.trim().is_empty() {
            return ToolResult::error("cmd must be a non-empty string");
        }
        if tty == Some(true) {
            return ToolResult::error(
                "tty is not supported by this bridge; commands run with plain pipes. Omit tty or pass false.",
            );
        }

        let yield_ms = clamp(
            yield_time_ms.unwrap_or(EXEC_DEFAULT_YIELD_MS),
            EXEC_MIN_YIELD_MS,
            EXEC_MAX_YIELD_MS,
        );
        // Codex floors the initial exec_command yield at 10s on Windows, where
        // process startup and pipe buffering make a short first window routinely
        // return before any output has surfaced.
        #[cfg(windows)]
        let yield_ms = yield_ms.max(EXEC_DEFAULT_YIELD_MS);
        let max_output_tokens = resolve_requested_output_tokens(config, max_output_tokens);

        let cwd = match resolve_safe_path(workdir.as_deref().unwrap_or(""), &config.work_dir, true)
        {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };

        let started = std::time::Instant::now();
        let exec_session = match crate::audit::stage("exec_spawn", async {
            start_exec_session(session, config, &cmd, &cwd, shell.as_deref())
        })
        .await
        {
            Ok(s) => s,
            Err(e) => return ToolResult::error(e),
        };

        let chat = command_chat(config, session, context);
        let cancellation = context
            .map(|context| context.cancellation.clone())
            .unwrap_or_default();
        let (output, exited, buffer_truncated) =
            match wait_for_command_output(&exec_session, yield_ms, chat.as_ref(), &cancellation)
                .await
            {
                Ok(output) => output,
                Err(error) => {
                    return ToolResult::error(format!("{error} Session id: {}", exec_session.id));
                }
            };
        let (text, original_token_count, truncated) = truncate_output(&output, max_output_tokens);

        let mut result = UnifiedExecOutput {
            chunk_id: Some(generate_chunk_id()),
            wall_time_seconds: started.elapsed().as_secs_f64(),
            output: text,
            original_token_count: Some(original_token_count),
            ..Default::default()
        };

        let is_error = if exited {
            let code = exec_session.exit_code().unwrap_or(-1);
            result.exit_code = Some(code);
            session.remove_exec_session(exec_session.id);
            code != 0
        } else {
            result.session_id = Some(exec_session.id);
            false
        };

        let structured = serde_json::to_value(&result).unwrap_or(Value::Null);
        ToolResult {
            content: vec![crate::types::ToolContent::Text(render_unified_exec_output(
                &result,
            ))],
            is_error,
            structured_content: Some(structured),
            meta: None,
            new_chat_message_from_user: None,
            chat_delivery_end: None,
            audit: ToolAuditMetadata {
                truncated: Some(buffer_truncated || truncated),
                original_output_tokens: truncated.then_some(original_token_count),
                exec_session_id: Some(exec_session.id),
                process_id: exec_session.pid,
                resident: Some(!exited),
                sensitive_output: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_config;
    use crate::output_budget::approx_token_count;
    use crate::tools::write_stdin::WriteStdin;
    use serde_json::json;
    use std::time::Duration;

    struct OwnerWakeFixture {
        root: tempfile::TempDir,
        config: AppConfig,
        session: Arc<SessionState>,
        context: ToolRequestContext,
        chat: Arc<crate::markdown_chat::ChatFile>,
    }

    async fn owner_wake_fixture() -> OwnerWakeFixture {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().canonicalize().unwrap());
        config.memory.dir = Some(root.path().join("metadata").display().to_string());
        config.markdown_chat.enabled = true;
        config.exec.default_shell = Some(if cfg!(windows) { "powershell" } else { "sh" }.into());
        let identity = crate::project_bindings::ConversationIdentity::from_openai_session(
            "command-owner-wake",
        );
        let context = ToolRequestContext {
            conversation: identity.clone(),
            task_conversation: identity,
            task_owner_generation: Some(0),
            conversation_retired: false,
            connector_schema_version: None,
            conversation_schema_version: None,
            conversation_authorizations: Arc::new(
                crate::conversation_auth::ConversationAuthorizationStore::new(),
            ),
            project_bindings: Arc::new(crate::project_bindings::ProjectBindingStore::new(
                root.path().join("bindings"),
            )),
            diff_checkpoints: Arc::new(crate::diff::DiffCheckpointManager::new()),
            artifact_egress: Arc::new(crate::artifact_egress::ArtifactEgressStore::new_at(
                config.artifact_egress.clone(),
                root.path().join("artifacts"),
            )),
            markdown_chat: Arc::default(),
            agent_automation: None,
            cancellation: CancellationToken::new(),
        };
        let session = Arc::new(SessionState::new());
        let chat = context
            .markdown_chat
            .chat(&config, context.task_conversation.as_ref(), &session)
            .unwrap();
        chat.ensure().await.unwrap();
        OwnerWakeFixture {
            root,
            config,
            session,
            context,
            chat,
        }
    }

    fn finish_on_input_command() -> &'static str {
        if cfg!(windows) {
            "$line=[Console]::ReadLine(); [Console]::WriteLine('finished:'+ $line)"
        } else {
            "IFS= read -r line; printf 'finished:%s\\n' \"$line\""
        }
    }

    async fn wait_for_test_file(path: &std::path::Path) {
        tokio::time::timeout(Duration::from_secs(8), async {
            while !path.is_file() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    fn spawn_poll(
        fixture: &OwnerWakeFixture,
        id: u64,
        chars: &str,
    ) -> tokio::task::JoinHandle<ToolResult> {
        let config = fixture.config.clone();
        let session = fixture.session.clone();
        let context = fixture.context.clone();
        let chars = chars.to_string();
        tokio::spawn(async move {
            WriteStdin
                .call_with_context(
                    json!({"session_id":id,"chars":chars,"yield_time_ms":300_000}),
                    &config,
                    &session,
                    &context,
                )
                .await
        })
    }

    #[tokio::test]
    async fn owner_message_wake_long_poll_preserves_process_cursor_and_task_isolation() {
        let fixture = owner_wake_fixture().await;
        fixture
            .chat
            .append_user("already-read".into(), "Old acknowledged input".into())
            .await
            .unwrap();
        fixture.chat.read(true).await.unwrap();
        let before = fixture.chat.unread_snapshot().await.unwrap();
        let exec = start_exec_session(
            &fixture.session,
            &fixture.config,
            finish_on_input_command(),
            fixture.root.path(),
            None,
        )
        .unwrap();
        let mut polling = spawn_poll(&fixture, exec.id, "");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut polling)
                .await
                .is_err(),
            "already-read text must not end the wait"
        );
        let other = crate::project_bindings::ConversationIdentity::from_openai_session(
            "unrelated-command-owner",
        )
        .unwrap();
        let other_chat = fixture
            .context
            .markdown_chat
            .chat(&fixture.config, Some(&other), &fixture.session)
            .unwrap();
        other_chat
            .append_user("other-owner".into(), "Do not wake a different task".into())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut polling)
                .await
                .is_err(),
            "another task's message must not wake this command"
        );
        fixture
            .chat
            .append_user("new-owner".into(), "Read this instruction now".into())
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(4), polling)
            .await
            .unwrap()
            .unwrap();
        assert!(!result.is_error, "{}", result.joined_text());
        assert_eq!(
            result.structured_content.as_ref().unwrap()["session_id"],
            exec.id
        );
        assert!(exec.exit_code().is_none());
        assert!(fixture.session.exec_session(exec.id).is_some());
        let pending = fixture.chat.unread_snapshot().await.unwrap();
        assert_eq!(pending.read_through, before.read_through);
        assert_eq!(pending.messages.len(), 1);
        assert_eq!(
            other_chat.unread_snapshot().await.unwrap().messages.len(),
            1
        );
        fixture.chat.read(true).await.unwrap();
        let finished = WriteStdin
            .call_with_context(
                json!({"session_id":exec.id,"chars":"finish\n","yield_time_ms":1500}),
                &fixture.config,
                &fixture.session,
                &fixture.context,
            )
            .await;
        assert_eq!(
            finished.structured_content.as_ref().unwrap()["exit_code"],
            0
        );
        assert!(finished.joined_text().contains("finished:finish"));
    }

    #[tokio::test]
    async fn owner_message_wake_after_stdin_never_resends_input_or_loses_late_output() {
        let fixture = owner_wake_fixture().await;
        let command = if cfg!(windows) {
            "$first=[Console]::ReadLine(); [IO.File]::AppendAllText('stdin-events.txt',$first+\"`n\"); [Console]::WriteLine('first:'+ $first); $second=[Console]::ReadLine(); [IO.File]::AppendAllText('stdin-events.txt',$second+\"`n\"); [Console]::WriteLine('second:'+ $second)"
        } else {
            "IFS= read -r first; printf '%s\\n' \"$first\" >> stdin-events.txt; printf 'first:%s\\n' \"$first\"; IFS= read -r second; printf '%s\\n' \"$second\" >> stdin-events.txt; printf 'second:%s\\n' \"$second\""
        };
        let exec = start_exec_session(
            &fixture.session,
            &fixture.config,
            command,
            fixture.root.path(),
            None,
        )
        .unwrap();
        let input = spawn_poll(&fixture, exec.id, "only-once\n");
        let events = fixture.root.path().join("stdin-events.txt");
        wait_for_test_file(&events).await;
        fixture
            .chat
            .append_user(
                "stdin-owner".into(),
                "New work while input waits for output".into(),
            )
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(4), input)
            .await
            .unwrap()
            .unwrap();
        assert!(!result.is_error, "{}", result.joined_text());
        assert_eq!(
            result.structured_content.as_ref().unwrap()["session_id"],
            exec.id
        );
        assert_eq!(
            std::fs::read_to_string(&events)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["only-once"]
        );
        assert!(exec.exit_code().is_none());
        fixture.chat.read(true).await.unwrap();
        let finished = WriteStdin
            .call_with_context(
                json!({"session_id":exec.id,"chars":"finish\n","yield_time_ms":1500}),
                &fixture.config,
                &fixture.session,
                &fixture.context,
            )
            .await;
        assert_eq!(
            finished.structured_content.as_ref().unwrap()["exit_code"],
            0
        );
        assert!(finished.joined_text().contains("second:finish"));
        let combined = format!("{}{}", result.joined_text(), finished.joined_text());
        assert_eq!(
            combined.matches("first:only-once").count(),
            1,
            "early and late output must be delivered exactly once"
        );
        assert_eq!(
            std::fs::read_to_string(&events)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["only-once", "finish"]
        );
    }

    #[tokio::test]
    async fn owner_message_wake_initial_exec_returns_its_registered_session() {
        let fixture = owner_wake_fixture().await;
        let config = fixture.config.clone();
        let session = fixture.session.clone();
        let context = fixture.context.clone();
        let running = tokio::spawn(async move {
            ExecCommand
                .call_with_context(
                    json!({"cmd":finish_on_input_command(),"yield_time_ms":30_000}),
                    &config,
                    &session,
                    &context,
                )
                .await
        });
        let id = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let Some(id) = fixture.session.exec_session_ids().first() {
                    break *id;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        fixture
            .chat
            .append_user(
                "initial-owner".into(),
                "Handle this before waiting further".into(),
            )
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(4), running)
            .await
            .unwrap()
            .unwrap();
        assert!(!result.is_error, "{}", result.joined_text());
        assert_eq!(
            result.structured_content.as_ref().unwrap()["session_id"],
            id
        );
        assert!(
            fixture
                .session
                .exec_session(id)
                .unwrap()
                .exit_code()
                .is_none()
        );
        assert_eq!(
            fixture.chat.unread_snapshot().await.unwrap().messages.len(),
            1
        );
        fixture.chat.read(true).await.unwrap();
        let finished = WriteStdin
            .call_with_context(
                json!({"session_id":id,"chars":"finish\n","yield_time_ms":5000}),
                &fixture.config,
                &fixture.session,
                &fixture.context,
            )
            .await;
        assert_eq!(
            finished.structured_content.as_ref().unwrap()["exit_code"],
            0
        );
    }

    #[tokio::test]
    async fn owner_message_wake_real_cancellation_keeps_session_and_buffer() {
        let fixture = owner_wake_fixture().await;
        let command = if cfg!(windows) {
            "[Console]::WriteLine('buffered-before-cancel'); [IO.File]::WriteAllText('cancellation-ready','ready'); $line=[Console]::ReadLine(); [Console]::WriteLine('after-cancel')"
        } else {
            "printf 'buffered-before-cancel\\n'; printf ready > cancellation-ready; IFS= read -r line; printf 'after-cancel\\n'"
        };
        let exec = start_exec_session(
            &fixture.session,
            &fixture.config,
            command,
            fixture.root.path(),
            None,
        )
        .unwrap();
        wait_for_test_file(&fixture.root.path().join("cancellation-ready")).await;
        let polling = spawn_poll(&fixture, exec.id, "");
        tokio::time::sleep(Duration::from_millis(50)).await;
        fixture.context.cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), polling)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_error);
        assert!(result.joined_text().contains("Output wait cancelled"));
        assert!(exec.exit_code().is_none());
        assert!(fixture.session.exec_session(exec.id).is_some());
        let (buffer, exited, _) = exec.yield_output_with_metadata(0).await;
        assert!(!exited);
        assert!(buffer.contains("buffered-before-cancel"));
        exec.write_stdin("finish\n").await.unwrap();
        let (late, exited, _) = exec.yield_output_with_metadata(1500).await;
        assert!(exited);
        assert!(late.contains("after-cancel"));
    }

    fn large_output_command() -> &'static str {
        if cfg!(windows) {
            "[Console]::Out.Write('x' * 4000)"
        } else {
            "head -c 4000 /dev/zero | tr '\\0' x"
        }
    }

    #[tokio::test]
    async fn completed_command_reports_process_audit_metadata() {
        let root = tempfile::tempdir().unwrap();
        let config = default_config(root.path().to_path_buf());
        let session = SessionState::new();

        let result = ExecCommand
            .call(
                json!({ "cmd": "echo audit", "yield_time_ms": 1000 }),
                &config,
                &session,
            )
            .await;

        assert!(!result.is_error, "{}", result.joined_text());
        assert_eq!(result.audit.truncated, Some(false));
        assert_eq!(result.audit.resident, Some(false));
        assert!(result.audit.exec_session_id.is_some());
        assert!(result.audit.process_id.is_some());
    }

    #[tokio::test]
    async fn requested_output_budget_cannot_exceed_server_policy() {
        let root = tempfile::tempdir().unwrap();
        let mut config = default_config(root.path().to_path_buf());
        config.output.max_tool_output_tokens = Some(20);
        let session = SessionState::new();

        let result = ExecCommand
            .call(
                json!({
                    "cmd": large_output_command(),
                    "yield_time_ms": 3000,
                    "max_output_tokens": 70000
                }),
                &config,
                &session,
            )
            .await;

        assert!(!result.is_error, "{}", result.joined_text());
        let output = result
            .structured_content
            .as_ref()
            .and_then(|value| value.get("output"))
            .and_then(Value::as_str)
            .unwrap();
        assert!(approx_token_count(output) <= 20, "{output}");
        assert!(output.contains("output truncated"), "{output}");
        assert_eq!(result.audit.truncated, Some(true));
        assert!(result.audit.original_output_tokens.unwrap() > 20);
    }
}
