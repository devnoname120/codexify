use std::io::Write;
use std::sync::Arc;

use clap::Parser;
use codexify::config::{Cli, default_config, load_config_quiet};
use codexify::exec_sessions::SessionState;
use codexify::markdown_chat::{MarkdownChatConfig, MarkdownChatStore};
use codexify::project_bindings::ConversationIdentity;

fn fixture() -> (tempfile::TempDir, codexify::types::AppConfig) {
    let root = tempfile::tempdir().unwrap();
    let mut config = default_config(root.path().to_path_buf());
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    config.markdown_chat.enabled = true;
    (root, config)
}

fn user_append(path: &std::path::Path, text: &str) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(text.as_bytes()).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn markdown_chat_defaults_and_validation() {
    let config: MarkdownChatConfig = serde_json::from_str("{}").unwrap();
    assert!(!config.enabled);
    assert_eq!(config.max_wait_ms, 115_000);
    assert!(config.notifications.is_none());
    assert!(config.validate().is_ok());
    for wait in [0, 999, 300_001, u64::MAX] {
        let config = MarkdownChatConfig {
            max_wait_ms: wait,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
    for url in ["", "not a service URL", "ntfys://topic/\n", "https://["] {
        let config: MarkdownChatConfig =
            serde_json::from_value(serde_json::json!({"notifications":{"urls":[url]}})).unwrap();
        assert!(config.validate().is_err(), "{url}");
    }
    let config: MarkdownChatConfig = serde_json::from_value(
        serde_json::json!({"notifications":{"urls":["ntfys://private-test-token@ntfy.sh/topic?auth=token&image=no"]}}),
    )
    .unwrap();
    assert!(config.validate().is_ok());
    assert!(!format!("{config:?}").contains("private-test-token"));
}

#[test]
fn config_loader_reads_agent_chat_without_exposing_timeout_in_tools() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.json");
    std::fs::write(&path, serde_json::json!({
        "workDir": root.path(), "codexMcp": {"enabled": false},
        "agentChat": {"enabled": true, "maxWaitMs": 270000, "notifications": {"urls":["ntfys://test-token@ntfy.sh/topic?auth=token&image=no"]}}
    }).to_string()).unwrap();
    let cli = Cli::try_parse_from(["codexify", "--config", path.to_str().unwrap()]).unwrap();
    let config = load_config_quiet(cli).unwrap();
    assert!(config.markdown_chat.enabled);
    assert_eq!(config.markdown_chat.max_wait_ms, 270000);
    assert_eq!(
        config.markdown_chat.notifications.unwrap().urls,
        vec!["ntfys://test-token@ntfy.sh/topic?auth=token&image=no"]
    );
}

#[test]
fn legacy_markdown_chat_key_migrates_without_a_runtime_alias() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.json");
    let original = serde_json::json!({
        "workDir": root.path(),
        "codexMcp": {"enabled": false},
        "markdownChat": {"enabled": true}
    })
    .to_string();
    std::fs::write(&path, &original).unwrap();
    let cli = Cli::try_parse_from(["codexify", "--config", path.to_str().unwrap()]).unwrap();

    let config = load_config_quiet(cli).unwrap();

    assert!(config.markdown_chat.enabled);
    let migrated: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(migrated["schemaVersion"], 1);
    assert_eq!(migrated["agentChat"]["enabled"], true);
    assert!(migrated.get("markdownChat").is_none());
    assert_eq!(
        std::fs::read(path.with_file_name("config.json.before-schema-v1.bak")).unwrap(),
        original.as_bytes()
    );
}

#[tokio::test]
async fn channels_are_per_conversation_even_without_worktrees_and_memory() {
    let (_root, mut config) = fixture();
    config.memory.enabled = Some(false);
    let store = MarkdownChatStore::default();
    let session = SessionState::new();
    let a = ConversationIdentity::from_openai_session("a").unwrap();
    let b = ConversationIdentity::from_openai_session("b").unwrap();
    let first = store.chat(&config, Some(&a), &session).unwrap();
    let other = store.chat(&config, Some(&b), &session).unwrap();
    assert_ne!(first.path(), other.path());
    assert_eq!(first.path().file_name().unwrap(), "CHAT.md");
    first.ensure().await.unwrap();
    other.ensure().await.unwrap();
    user_append(first.path(), "User A only\n");
    assert_eq!(first.read(false).await.unwrap().text, "User A only\n");
    assert_eq!(other.read(false).await.unwrap().text, "");
    assert!(Arc::ptr_eq(
        &first,
        &store.chat(&config, Some(&a), &session).unwrap()
    ));
}

#[tokio::test]
async fn peeks_do_not_consume_and_cursors_survive_restart_and_transport_replacement() {
    let (_root, config) = fixture();
    let identity = ConversationIdentity::from_openai_session("persistent").unwrap();
    let store = MarkdownChatStore::default();
    let channel = store
        .chat(&config, Some(&identity), &SessionState::new())
        .unwrap();
    channel.ensure().await.unwrap();
    let message = "\n## User\nPlease review this without changing it.\n\n";
    user_append(channel.path(), message);
    for _ in 0..3 {
        assert_eq!(channel.read(false).await.unwrap().text, message);
    }
    assert_eq!(channel.read(true).await.unwrap().text, message);
    let restarted = MarkdownChatStore::default()
        .chat(&config, Some(&identity), &SessionState::new())
        .unwrap();
    assert_eq!(restarted.path(), channel.path());
    assert_eq!(restarted.read(false).await.unwrap().text, "");
    user_append(channel.path(), "Next\n");
    assert_eq!(restarted.read(true).await.unwrap().text, "Next\n");
}

#[tokio::test]
async fn write_returns_unread_user_text_and_does_not_echo_agent_messages() {
    let (_root, config) = fixture();
    let channel = MarkdownChatStore::default()
        .chat(&config, None, &SessionState::new())
        .unwrap();
    channel.ensure().await.unwrap();
    user_append(channel.path(), "Before writing: don't push.\n");
    let receipt = channel
        .append("The tests passed. I will not push.".into())
        .await
        .unwrap();
    assert_eq!(receipt.user_text, "Before writing: don't push.\n");
    assert_eq!(channel.read(false).await.unwrap().text, "");
    user_append(channel.path(), "Now commit.\n");
    assert_eq!(channel.read(true).await.unwrap().text, "Now commit.\n");
    let full = std::fs::read_to_string(channel.path()).unwrap();
    assert!(full.contains("Before writing: don't push.\n"));
    assert!(full.contains("The tests passed. I will not push."));
}

#[tokio::test]
async fn atomic_editor_saves_preserve_unicode_and_large_messages() {
    let (_root, config) = fixture();
    let channel = MarkdownChatStore::default()
        .chat(&config, None, &SessionState::new())
        .unwrap();
    channel.ensure().await.unwrap();
    let message = format!("{}\nTHE END\n", "日本語 é \n".repeat(12000));
    let mut replacement =
        tempfile::NamedTempFile::new_in(channel.path().parent().unwrap()).unwrap();
    replacement
        .write_all(&std::fs::read(channel.path()).unwrap())
        .unwrap();
    replacement.write_all(message.as_bytes()).unwrap();
    replacement.persist(channel.path()).unwrap();
    assert_eq!(channel.read(true).await.unwrap().text, message);
    assert_eq!(channel.read(false).await.unwrap().text, "");
}

#[tokio::test]
async fn truncation_is_an_error_and_never_silently_advances_the_cursor() {
    let (_root, config) = fixture();
    let channel = MarkdownChatStore::default()
        .chat(&config, None, &SessionState::new())
        .unwrap();
    channel.ensure().await.unwrap();
    user_append(channel.path(), "first message\n");
    channel.read(true).await.unwrap();
    let full = std::fs::read(channel.path()).unwrap();
    std::fs::write(channel.path(), "truncated").unwrap();
    assert!(channel.read(true).await.is_err());
    std::fs::write(channel.path(), &full).unwrap();
    user_append(channel.path(), "restored\n");
    assert_eq!(channel.read(true).await.unwrap().text, "restored\n");
}

#[tokio::test]
async fn disabled_feature_creates_no_channel() {
    let (root, mut config) = fixture();
    config.markdown_chat.enabled = false;
    assert!(
        MarkdownChatStore::default()
            .chat(&config, None, &SessionState::new())
            .is_err()
    );
    assert!(!root.path().join("metadata").exists());
}

#[tokio::test]
async fn generic_transports_get_independent_channels() {
    let (_root, config) = fixture();
    let store = MarkdownChatStore::default();
    let first = store.chat(&config, None, &SessionState::new()).unwrap();
    let second = store.chat(&config, None, &SessionState::new()).unwrap();
    assert_ne!(first.path(), second.path());
    first.ensure().await.unwrap();
    assert!(!first.path().with_file_name("cursor.json").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn transcript_is_private_and_symlinks_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (root, config) = fixture();
    let channel = MarkdownChatStore::default()
        .chat(&config, None, &SessionState::new())
        .unwrap();
    channel.ensure().await.unwrap();
    assert_eq!(
        std::fs::metadata(channel.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(channel.path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    std::fs::remove_file(channel.path()).unwrap();
    let unrelated = root.path().join("unrelated.md");
    std::fs::write(&unrelated, "unrelated").unwrap();
    symlink(&unrelated, channel.path()).unwrap();
    assert!(channel.read(true).await.is_err());
    assert!(channel.append("no".into()).await.is_err());
    assert_eq!(std::fs::read_to_string(unrelated).unwrap(), "unrelated");
}
