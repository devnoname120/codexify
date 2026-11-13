use codexify::config::default_config;
use codexify::diff::{DiffCheckpointManager, DiffOwner};
use codexify::exec_sessions::SessionState;
use codexify::tool::{Tool, ToolRequestContext};
use codexify::tools::{
    exec_command::ExecCommand, glob::Glob, grep::Grep, read_file::ReadFile, write_stdin::WriteStdin,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn config(root: &std::path::Path) -> codexify::types::AppConfig {
    let mut config = default_config(root.to_path_buf());
    config.set_filesystem_worker_executable(env!("CARGO_BIN_EXE_codexify").into());
    config
}

#[tokio::test]
async fn a_small_window_does_not_read_a_gigabyte_file() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("large.log");
    std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    let mut config = config(root.path());
    config.filesystem.max_scan_bytes = 32;
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        ReadFile.call(
            json!({"path":"large.log","limit":2}),
            &config,
            &SessionState::new(),
        ),
    )
    .await
    .unwrap();
    assert!(!result.is_error, "{}", result.joined_text());
    assert!(result.joined_text().contains("1\tfirst\n2\tsecond"));
    assert!(!result.joined_text().contains("input byte budget"));
    assert!(result.joined_text().contains("offset=2"));
}

#[tokio::test]
async fn input_byte_and_directory_limits_are_reported() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.txt"), "before\nneedle\n").unwrap();
    let mut config = config(root.path());
    config.filesystem.max_scan_bytes = 7;
    let result = Grep::default()
        .call(json!({"pattern":"needle"}), &config, &SessionState::new())
        .await;
    assert!(!result.is_error, "{}", result.joined_text());
    assert!(result.joined_text().contains("input byte budget reached"));
    assert_eq!(result.audit.truncated, Some(true));
    for index in 0..10 {
        std::fs::write(root.path().join(format!("{index}.txt")), "x").unwrap();
    }
    config.filesystem.max_scan_entries = 3;
    let result = Glob
        .call(json!({"pattern":"*.txt"}), &config, &SessionState::new())
        .await;
    assert!(
        result
            .joined_text()
            .contains("directory-entry budget reached")
    );
    assert_eq!(result.audit.truncated, Some(true));
}

#[tokio::test]
async fn streamed_grep_preserves_overlapping_context_and_group_separators() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("file.txt"),
        "one\nhit\nhit\nfour\nfive\nsix\nseven\nhit\nnine",
    )
    .unwrap();
    let config = config(root.path());
    let result = Grep::default()
        .call(
            json!({"pattern":"hit","context":1}),
            &config,
            &SessionState::new(),
        )
        .await;
    assert!(!result.is_error, "{}", result.joined_text());
    assert_eq!(
        result.joined_text(),
        "file.txt:1-one\nfile.txt:2:hit\nfile.txt:3:hit\nfile.txt:4-four\n--\nfile.txt:7-seven\nfile.txt:8:hit\nfile.txt:9-nine"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_checkpoint_prevents_sending_stdin() {
    let root = tempfile::tempdir().unwrap();
    let output = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    std::fs::write(root.path().join(".git/config"), "[invalid").unwrap();
    let config = config(root.path());
    let session = SessionState::new();
    let started = ExecCommand.call(json!({"cmd":"read value; printf '%s' \"$value\" > changed.txt","shell":"/bin/sh","yield_time_ms":50}), &config, &session).await;
    assert!(!started.is_error, "{}", started.joined_text());
    let id = started.structured_content.as_ref().unwrap()["session_id"]
        .as_u64()
        .unwrap();
    let context = ToolRequestContext {
        conversation: None,
        task_conversation: None,
        task_owner_generation: None,
        conversation_retired: false,
        connector_schema_version: None,
        conversation_schema_version: None,
        conversation_authorizations: Arc::new(
            codexify::conversation_auth::ConversationAuthorizationStore::new(),
        ),
        project_bindings: Arc::new(codexify::project_bindings::ProjectBindingStore::new(
            root.path().join("bindings"),
        )),
        markdown_chat: Arc::new(codexify::markdown_chat::MarkdownChatStore::default()),
        diff_checkpoints: Arc::new(DiffCheckpointManager::new()),
        artifact_egress: Arc::new(codexify::artifact_egress::ArtifactEgressStore::new_at(
            config.artifact_egress.clone(),
            root.path().join("artifacts"),
        )),
        agent_automation: None,
        agent_runtime: None,
        cancellation: CancellationToken::new(),
    };
    let result = WriteStdin
        .call_with_context(
            json!({"session_id":id,"chars":"value\n"}),
            &config,
            &session,
            &context,
        )
        .await;
    let process = session.exec_session(id).unwrap();
    process.interrupt();
    process.yield_output_with_metadata(1000).await;
    assert!(result.is_error, "{}", result.joined_text());
    assert!(result.joined_text().contains("Refusing to send input"));
    assert!(!root.path().join("changed.txt").exists());
}

#[tokio::test]
async fn read_checkpoint_checks_do_not_queue_behind_a_mutation() {
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path());
    let manager = DiffCheckpointManager::new();
    let session = SessionState::new();
    let owner = DiffOwner::transport(session.diff_state());
    let (_, _guard) = manager
        .begin_mutation(&config, owner.clone())
        .await
        .unwrap();
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        manager.ensure_initialized_for_read(&config, owner),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        result,
        codexify::diff::DiffAvailability::Unavailable(_)
    ));
}
