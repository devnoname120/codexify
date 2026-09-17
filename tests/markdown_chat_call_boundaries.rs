use codexify::config::default_config;
use codexify::exec_sessions::SessionState;
use codexify::markdown_chat::MarkdownChatStore;
use codexify::project_bindings::ConversationIdentity;
use std::io::Write;

#[tokio::test]
async fn user_messages_preserve_call_boundaries_through_retries_and_restart() {
    let root = tempfile::tempdir().unwrap();
    let mut config = default_config(root.path().into());
    config.markdown_chat.enabled = true;
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    let owner = ConversationIdentity::from_openai_session("call-boundaries").unwrap();
    let session = SessionState::new();
    let chat = MarkdownChatStore::default()
        .chat(&config, Some(&owner), &session)
        .unwrap();
    chat.record_agent_call(1000).await.unwrap();
    chat.append("Working on it".into()).await.unwrap();
    chat.record_agent_call(2000).await.unwrap();
    chat.record_agent_call(3000).await.unwrap();
    let before = chat.widget_page(None, None).await.unwrap();
    let sent = serde_json::to_value(
        chat.append_user("question".into(), "And now?".into())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(sent["tool_call_count"], 3);
    let page = chat.widget_page(None, None).await.unwrap();
    assert_eq!(page.total_tool_calls, 3);
    assert_eq!(page.read_through, before.read_through);
    assert_eq!(page.delivered_through, before.delivered_through);
    assert_eq!(page.messages[1].tool_call_count, Some(3));
    assert!(chat.read(false).await.unwrap().text.contains("And now?"));
    chat.record_agent_call(4000).await.unwrap();
    let reopened = MarkdownChatStore::default()
        .chat(&config, Some(&owner), &SessionState::new())
        .unwrap();
    let repeated = serde_json::to_value(
        reopened
            .append_user("question".into(), "And now?".into())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(repeated, sent);
    reopened
        .append_user("second".into(), "Still here".into())
        .await
        .unwrap();
    reopened.record_agent_call(5000).await.unwrap();
    reopened.append("Finished".into()).await.unwrap();
    let page = reopened.widget_page(None, None).await.unwrap();
    assert_eq!(page.total_tool_calls, 5);
    assert_eq!(
        page.messages
            .iter()
            .map(|message| message.tool_call_count)
            .collect::<Vec<_>>(),
        vec![Some(1), Some(3), Some(4), Some(5)]
    );
    let older = reopened
        .widget_page(Some(page.messages[2].start), None)
        .await
        .unwrap();
    assert_eq!(older.messages[1].tool_call_count, Some(3));
}

#[tokio::test]
async fn legacy_user_retry_does_not_invent_a_call_boundary() {
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
    writeln!(file, "\n\n<!-- codexify-user-message:v1:start id=\"old\" created_at_ms=\"1000\" -->\n\n## User\n\nOld message\n\n<!-- codexify-user-message:v1:end id=\"old\" -->").unwrap();
    chat.record_agent_call(2000).await.unwrap();
    let receipt = serde_json::to_value(
        chat.append_user("old".into(), "Old message".into())
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(receipt["tool_call_count"].is_null());
    let page = chat.widget_page(None, None).await.unwrap();
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].tool_call_count, None);
}
