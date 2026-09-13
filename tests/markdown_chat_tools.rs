use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use codexify::config::default_config;
use codexify::exec_sessions::SessionState;
use codexify::markdown_chat::{MarkdownChatStore, NotificationState, NtfyConfig, WaitOutcome};
use codexify::project_bindings::ConversationIdentity;
use codexify::registry::load_tools_for_config;
use codexify::tool::ToolRequestContext;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn fixture() -> (
    tempfile::TempDir,
    codexify::types::AppConfig,
    SessionState,
    ToolRequestContext,
) {
    let root = tempfile::tempdir().unwrap();
    let mut config = default_config(root.path().to_path_buf());
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    config.markdown_chat.enabled = true;
    config.markdown_chat.max_wait_ms = 1000;
    let context = ToolRequestContext {
        conversation: ConversationIdentity::from_openai_session("tool-test"),
        connector_schema_version: None,
        conversation_schema_version: None,
        conversation_authorizations: Arc::new(
            codexify::conversation_auth::ConversationAuthorizationStore::new(),
        ),
        project_bindings: Arc::new(codexify::project_bindings::ProjectBindingStore::new(
            root.path().join("bindings"),
        )),
        diff_checkpoints: Arc::new(codexify::diff::DiffCheckpointManager::new()),
        artifact_egress: Arc::new(codexify::artifact_egress::ArtifactEgressStore::new_at(
            config.artifact_egress.clone(),
            root.path().join("artifacts"),
        )),
        markdown_chat: Arc::new(MarkdownChatStore::default()),
        cancellation: CancellationToken::new(),
    };
    (root, config, SessionState::new(), context)
}

fn append(path: &std::path::Path, text: &str) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(text.as_bytes()).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn tools_are_opt_in_and_read_wait_have_no_parameters() {
    let (_root, mut config, _session, _context) = fixture();
    let enabled = load_tools_for_config(&config);
    for name in ["chat_read", "chat_write", "chat_await"] {
        let tool = enabled.iter().find(|tool| tool.name() == name).unwrap();
        if name != "chat_write" {
            assert_eq!(tool.input_schema()["properties"], json!({}));
            assert!(!jsonschema::is_valid(
                &tool.input_schema(),
                &json!({"maxWaitMs":1})
            ));
        }
    }
    config.markdown_chat.enabled = false;
    assert!(
        load_tools_for_config(&config)
            .iter()
            .all(|tool| !tool.name().starts_with("chat_"))
    );
}

#[tokio::test]
async fn chat_tools_return_pending_input_without_the_normal_output_budget() {
    let (_root, mut config, session, context) = fixture();
    config.output.max_tool_output_tokens = Some(1);
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    let text = format!("{}END", "user text\n".repeat(12000));
    append(chat.path(), &text);
    let tools = load_tools_for_config(&config);
    let writer = tools
        .iter()
        .find(|tool| tool.name() == "chat_write")
        .unwrap();
    let result = writer
        .call_with_context(
            json!({"message":"I am waiting for clarification."}),
            &config,
            &session,
            &context,
        )
        .await;
    assert!(!result.is_error, "{}", result.joined_text());
    let pending = result.new_chat_message_from_user.unwrap();
    assert!(pending.contains("Before"));
    assert!(pending.ends_with(&text));
    assert_eq!(chat.read(false).await.unwrap().text, "");
    append(chat.path(), "Read this\n");
    let reader = tools
        .iter()
        .find(|tool| tool.name() == "chat_read")
        .unwrap();
    let result = reader
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(
        result
            .new_chat_message_from_user
            .unwrap()
            .ends_with("Read this\n")
    );
    let result = reader
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(result.new_chat_message_from_user.is_none());
    assert!(result.joined_text().contains("chat_await"));
}

#[tokio::test]
async fn brief_has_the_conversation_path_and_read_only_history_guidance() {
    let (_root, config, session, context) = fixture();
    let tools = load_tools_for_config(&config);
    let tool = tools
        .iter()
        .find(|tool| tool.name() == "get_agent_brief")
        .unwrap();
    let result = tool
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(!result.is_error);
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    let brief = result.joined_text();
    assert!(brief.contains(&chat.path().display().to_string()));
    assert!(brief.contains("read-only"));
    assert!(brief.contains("chat_read"));
    assert!(brief.contains("chat_write"));
    assert!(brief.contains("chat_await"));
    assert!(chat.path().is_file());
}

#[tokio::test]
async fn history_tools_read_only_the_current_conversation_without_acknowledging() {
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    append(chat.path(), "Historical search target\n");
    let other_identity = ConversationIdentity::from_openai_session("other-conversation").unwrap();
    let other = context
        .markdown_chat
        .chat(&config, Some(&other_identity), &session)
        .unwrap();
    other.ensure().await.unwrap();
    append(other.path(), "Unrelated private text\n");
    let tools = load_tools_for_config(&config);
    for name in ["read_file", "grep"] {
        let tool = tools.iter().find(|tool| tool.name() == name).unwrap();
        let args = if name == "grep" {
            json!({"path":chat.path(), "pattern":"Historical"})
        } else {
            json!({"path":chat.path()})
        };
        let result = tool
            .call_with_context(args, &config, &session, &context)
            .await;
        assert!(!result.is_error, "{name}: {}", result.joined_text());
        assert!(result.joined_text().contains("Historical search target"));
        assert_eq!(
            chat.read(false).await.unwrap().text,
            "Historical search target\n"
        );
        let args = if name == "grep" {
            json!({"path":other.path(), "pattern":"private"})
        } else {
            json!({"path":other.path()})
        };
        let denied = tool
            .call_with_context(args, &config, &session, &context)
            .await;
        assert!(denied.is_error);
        assert!(!denied.joined_text().contains("Unrelated private text"));
    }
}

#[tokio::test]
async fn await_returns_immediate_input_and_can_be_cancelled_without_consuming() {
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    append(chat.path(), "already waiting\n");
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        chat.wait(Duration::from_secs(2), cancelled).await.unwrap(),
        WaitOutcome::Cancelled
    ));
    assert_eq!(chat.read(false).await.unwrap().text, "already waiting\n");
    let result = chat
        .wait(Duration::from_secs(2), CancellationToken::new())
        .await
        .unwrap();
    match result {
        WaitOutcome::Message(snapshot) => assert_eq!(snapshot.text, "already waiting\n"),
        _ => panic!("pending input was not returned"),
    }
    let token = CancellationToken::new();
    let waiting = tokio::spawn({
        let chat = chat.clone();
        let token = token.clone();
        async move { chat.wait(Duration::from_secs(20), token).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    token.cancel();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        WaitOutcome::Cancelled
    ));
}

#[tokio::test]
async fn directory_watcher_handles_atomic_editor_replacement() {
    let (_root, config, session, context) = fixture();
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    chat.ensure().await.unwrap();
    let waiting = tokio::spawn({
        let chat = chat.clone();
        async move {
            chat.wait(Duration::from_secs(5), CancellationToken::new())
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut new_file = tempfile::NamedTempFile::new_in(chat.path().parent().unwrap()).unwrap();
    new_file
        .write_all(&std::fs::read(chat.path()).unwrap())
        .unwrap();
    new_file.write_all(b"saved by replacement\n").unwrap();
    new_file.persist(chat.path()).unwrap();
    match waiting.await.unwrap().unwrap() {
        WaitOutcome::Message(snapshot) => assert_eq!(snapshot.text, "saved by replacement\n"),
        _ => panic!("atomic editor append was missed"),
    }
}

#[tokio::test]
async fn timeout_instructs_another_await_without_claiming_a_read_receipt() {
    let (_root, config, session, context) = fixture();
    let tools = load_tools_for_config(&config);
    let tool = tools
        .iter()
        .find(|tool| tool.name() == "chat_await")
        .unwrap();
    let result = tool
        .call_with_context(json!({}), &config, &session, &context)
        .await;
    assert!(!result.is_error);
    assert_eq!(result.structured_content.unwrap()["status"], "timeout");
    assert!(result.new_chat_message_from_user.is_none());
    let text = result
        .content
        .iter()
        .filter_map(|part| match part {
            codexify::types::ToolContent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("chat_await again"));
    assert!(!text.contains("confirmed that the user has seen"));
}

#[tokio::test]
async fn ntfy_receives_exact_markdown_and_token_and_failure_does_not_undo_append() {
    use axum::{
        Router,
        body::Bytes,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    let (_root, mut config, session, context) = fixture();
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    let app = Router::new()
        .route(
            "/topic",
            post(move |headers: HeaderMap, body: Bytes| {
                let tx = tx.clone();
                async move {
                    tx.send((headers, body)).await.unwrap();
                    StatusCode::OK
                }
            }),
        )
        .route(
            "/failure",
            post(|| async { StatusCode::SERVICE_UNAVAILABLE }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    config.markdown_chat.ntfy = Some(NtfyConfig {
        url: format!("http://{address}/topic"),
        token: Some("test-ntfy-token".into()),
    });
    let tools = load_tools_for_config(&config);
    let writer = tools
        .iter()
        .find(|tool| tool.name() == "chat_write")
        .unwrap();
    let markdown = format!("## Question\n{}\n", "**full message**\n".repeat(1000));
    let result = writer
        .call_with_context(json!({"message":markdown}), &config, &session, &context)
        .await;
    assert!(!result.is_error, "{}", result.joined_text());
    assert_eq!(
        result.structured_content.unwrap()["notification"],
        "accepted"
    );
    let (headers, body) = rx.recv().await.unwrap();
    assert_eq!(headers["authorization"], "Bearer test-ntfy-token");
    assert_eq!(headers["markdown"], "yes");
    assert_eq!(body.as_ref(), markdown.as_bytes());
    config.markdown_chat.ntfy.as_mut().unwrap().url = format!("http://{address}/failure");
    let result = writer
        .call_with_context(
            json!({"message":"Written despite notification failure"}),
            &config,
            &session,
            &context,
        )
        .await;
    assert!(!result.is_error);
    assert_eq!(result.structured_content.unwrap()["notification"], "failed");
    let chat = context
        .markdown_chat
        .chat(&config, context.conversation.as_ref(), &session)
        .unwrap();
    assert!(
        std::fs::read_to_string(chat.path())
            .unwrap()
            .contains("Written despite notification failure")
    );
    assert_eq!(
        chat.read(false).await.unwrap().notification,
        NotificationState::Failed
    );
    server.abort();
}
