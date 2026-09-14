use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use codexify::config::default_config;
use codexify::exec_sessions::SessionState;
use codexify::markdown_chat::MarkdownChatStore;
use codexify::project_bindings::ConversationIdentity;
use serde_json::{Value, json};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[tokio::test]
async fn user_and_agent_times_are_persisted_and_retries_keep_the_original_time() {
    let root = tempfile::tempdir().unwrap();
    let mut config = default_config(root.path().into());
    config.markdown_chat.enabled = true;
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    let owner = ConversationIdentity::from_openai_session("timestamps").unwrap();
    let session = SessionState::new();
    let store = MarkdownChatStore::default();
    let chat = store.chat(&config, Some(&owner), &session).unwrap();
    let before = now_ms();
    let sent = serde_json::to_value(
        chat.append_user("timed-user".into(), "User body\n".into())
            .await
            .unwrap(),
    )
    .unwrap();
    let sent_at = sent["created_at_ms"]
        .as_u64()
        .expect("server timestamp on send receipt");
    assert!((before..=now_ms()).contains(&sent_at));
    chat.append("Agent body\n".into()).await.unwrap();
    let first = serde_json::to_value(chat.widget_page(None, None).await.unwrap()).unwrap();
    assert_eq!(first["messages"][0]["created_at_ms"], sent_at);
    assert_eq!(first["messages"][0]["markdown"], "User body\n");
    assert_eq!(first["messages"][1]["markdown"], "Agent body\n");
    let agent_at = first["messages"][1]["created_at_ms"]
        .as_u64()
        .expect("agent timestamp");
    assert!((before..=now_ms()).contains(&agent_at));
    let reopened = MarkdownChatStore::default()
        .chat(&config, Some(&owner), &SessionState::new())
        .unwrap();
    let repeated = serde_json::to_value(
        reopened
            .append_user("timed-user".into(), "User body\n".into())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(repeated, sent);
    let restored = serde_json::to_value(reopened.widget_page(None, None).await.unwrap()).unwrap();
    assert_eq!(restored["messages"], first["messages"]);
    assert!(reopened.read(false).await.unwrap().text.is_empty());
}

#[tokio::test]
async fn timed_markers_preserve_user_text_and_legacy_history_without_inventing_times() {
    let root = tempfile::tempdir().unwrap();
    let mut config = default_config(root.path().into());
    config.markdown_chat.enabled = true;
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    let chat = MarkdownChatStore::default()
        .chat(&config, None, &SessionState::new())
        .unwrap();
    chat.ensure().await.unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(chat.path())
        .unwrap();
    for (role, id, at, body) in [
        ("user", "old-user", "", "Old user body"),
        ("agent", "1789399800000000-7", "", "Old agent body"),
        (
            "user",
            "new-user",
            "\" created_at_ms=\"1789400100000",
            "  Complete user text\n",
        ),
        (
            "agent",
            "new-agent",
            "\" created_at_ms=\"1789400200000",
            "New agent body",
        ),
    ] {
        let heading = if role == "user" { "User" } else { "Agent" };
        writeln!(file, "\n\n<!-- codexify-{role}-message:v1:start id=\"{id}{at}\" -->\n\n## {heading}\n\n{body}\n\n<!-- codexify-{role}-message:v1:end id=\"{id}\" -->").unwrap();
    }
    file.write_all(b"Plain editor append\n").unwrap();
    let page: Value = serde_json::to_value(chat.widget_page(None, None).await.unwrap()).unwrap();
    let messages = page["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 5);
    assert!(messages[0]["created_at_ms"].is_null());
    assert_eq!(messages[1]["created_at_ms"], json!(1789399800000u64));
    assert_eq!(messages[2]["created_at_ms"], json!(1789400100000u64));
    assert_eq!(messages[2]["markdown"], "  Complete user text\n");
    assert_eq!(messages[3]["created_at_ms"], json!(1789400200000u64));
    assert!(messages[4]["created_at_ms"].is_null());
    let unread = chat.read(true).await.unwrap().text;
    assert_eq!(
        unread,
        "Old user body\n\n  Complete user text\n\n\nPlain editor append\n"
    );
    assert!(!unread.contains("created_at_ms"));
    assert!(!unread.contains("agent body"));
}
