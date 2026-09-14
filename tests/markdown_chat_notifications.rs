use codexify::markdown_chat::MarkdownChatConfig;
use codexify::markdown_chat::{NotificationState, notification};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[test]
fn only_notifications_is_serialized_as_the_provider_configuration() {
    let config = serde_json::to_value(MarkdownChatConfig::default()).unwrap();
    assert!(config.get("notifications").is_some());
    assert!(
        config.get("ntfy").is_none(),
        "native ntfy must not remain in the config"
    );
}

#[test]
fn only_notifications_is_accepted_even_for_ntfy_destinations() {
    for ntfy in [
        json!(null),
        json!({"url":"https://ntfy.example/private-topic", "token":"private-token"}),
    ] {
        let result = serde_json::from_value::<MarkdownChatConfig>(json!({"ntfy":ntfy}));
        assert!(
            result.is_err(),
            "the removed ntfy block must not be accepted or ignored"
        );
        let message = result.unwrap_err().to_string();
        assert!(message.contains("ntfy"));
        assert!(!message.contains("private-topic"));
        assert!(!message.contains("private-token"));
    }
    let config: MarkdownChatConfig = serde_json::from_value(json!({
        "notifications":{"urls":["ntfys://ntfy.example/topic?image=no","pover://user@app"]}
    }))
    .unwrap();
    assert!(config.validate().is_ok());
}

#[test]
fn apprise_configuration_is_opt_in_validated_and_redacted() {
    let default: MarkdownChatConfig = serde_json::from_value(json!({})).unwrap();
    assert!(default.validate().is_ok());
    let configured = serde_json::from_value::<MarkdownChatConfig>(json!({
        "notifications":{"urls":["ntfys://private-topic","pover://private-user@private-token"]}
    }))
    .expect("provider-agnostic notification configuration");
    assert!(configured.validate().is_ok());
    let debug = format!("{configured:?}");
    for secret in ["private-topic", "private-user", "private-token"] {
        assert!(!debug.contains(secret));
    }
    for value in [
        json!({"notifications":{"urls":[]}}),
        json!({"notifications":{"urls":[""]}}),
        json!({"notifications":{"urls":["not a service URL"]}}),
        json!({"notifications":{"urls":["ntfys://topic"],"pythonPath":""}}),
        json!({"notifications":{"urls":["ntfys://topic"],"timeoutMs":0}}),
        json!({"notifications":{"urls":["ntfys://topic"],"timeoutMs":60001}}),
        json!({"notifications":{"urls":["ntfys://topic"]},"ntfy":{"url":"https://ntfy.sh/legacy"}}),
    ] {
        let config = serde_json::from_value::<MarkdownChatConfig>(value.clone());
        assert!(
            config.is_err() || config.unwrap().validate().is_err(),
            "accepted {value}"
        );
    }
}

#[tokio::test]
async fn missing_runtime_and_pre_cancelled_sends_have_explicit_outcomes() {
    let root = tempfile::tempdir().unwrap();
    let config: MarkdownChatConfig = serde_json::from_value(json!({
        "notifications":{"urls":["ntfys://test"],"pythonPath":root.path().join("missing-python")}
    }))
    .unwrap();
    let token = CancellationToken::new();
    assert_eq!(
        notification::publish_config(&config, "test", "body".into(), &token).await,
        NotificationState::Failed
    );
    token.cancel();
    assert_eq!(
        notification::publish_config(&config, "test", "body".into(), &token).await,
        NotificationState::Cancelled
    );
    assert_eq!(
        notification::publish_config(
            &MarkdownChatConfig::default(),
            "test",
            "body".into(),
            &CancellationToken::new()
        )
        .await,
        NotificationState::NotConfigured
    );
}

fn apprise_config(address: std::net::SocketAddr, timeout: u64) -> MarkdownChatConfig {
    let python = std::env::var("CODEXIFY_TEST_APPRISE_PYTHON")
        .expect("CI must provide an interpreter with Apprise installed");
    serde_json::from_value(json!({"notifications":{
        "urls":[format!("ntfy://{address}/test-topic?image=no")],
        "pythonPath":python,"timeoutMs":timeout
    }}))
    .unwrap()
}

#[tokio::test]
#[ignore = "requires CODEXIFY_TEST_APPRISE_PYTHON; exercised explicitly by CI on every platform"]
async fn apprise_subprocess_delivers_markdown_to_a_local_ntfy_server() {
    use axum::{Router, http::HeaderMap, routing::post};
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let app = Router::new().route(
        "/",
        post(move |headers: HeaderMap, body: String| {
            let captured = captured.clone();
            async move {
                captured.lock().unwrap().push((headers, body));
                axum::Json(json!({"id":"test","event":"message"}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = apprise_config(listener.local_addr().unwrap(), 15000);
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let markdown = "## Exact body\n\n**Bold** and Unicode: é 日本語\n";
    let result =
        notification::publish_config(&config, "test", markdown.into(), &CancellationToken::new())
            .await;
    server.abort();
    assert_eq!(result, NotificationState::Accepted);
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0["x-markdown"], "yes");
    let payload: serde_json::Value = serde_json::from_str(&requests[0].1).unwrap();
    // Apprise normalizes boundary whitespace; CHAT.md retains the exact source.
    assert_eq!(payload["message"], markdown.trim());
    assert_eq!(payload["topic"], "test-topic");
    assert_eq!(payload["title"], "Codexify - test");
}

#[tokio::test]
#[ignore = "requires CODEXIFY_TEST_APPRISE_PYTHON; exercised explicitly by CI on every platform"]
async fn apprise_waits_are_bounded_and_cancellable_after_the_request_starts() {
    use axum::{Router, routing::post};
    for cancel in [true, false] {
        let seen = Arc::new(tokio::sync::Notify::new());
        let received = seen.clone();
        let app = Router::new().route(
            "/",
            post(move || {
                let received = received.clone();
                async move {
                    received.notify_one();
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    axum::Json(json!({"event":"message"}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = apprise_config(listener.local_addr().unwrap(), 5000);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let token = CancellationToken::new();
        let cancellation = token.clone();
        let work = tokio::spawn(async move {
            notification::publish_config(&config, "test", "body".into(), &cancellation).await
        });
        tokio::time::timeout(Duration::from_secs(10), seen.notified())
            .await
            .unwrap();
        if cancel {
            token.cancel();
        }
        let outcome = tokio::time::timeout(Duration::from_secs(7), work)
            .await
            .unwrap()
            .unwrap();
        server.abort();
        assert_eq!(
            outcome,
            if cancel {
                NotificationState::Cancelled
            } else {
                NotificationState::Failed
            }
        );
    }
}
