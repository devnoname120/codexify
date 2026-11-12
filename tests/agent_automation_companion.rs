use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use codexify::agent_automation::{
    ActionPrompt, BrowserCompanionClient, CompanionActionState, CompanionEventKind,
    CompanionUnreadMessage, ExecuteActionRequest, PROTOCOL_VERSION, RevealWindowRequest,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const BUILD: &str =
    "test-build-v1+protocol.7d1f04ca4cb844940820aeeb703a42c36363cee5e5f7209d46c5907f26c5ce05";

#[derive(Clone, Default)]
struct FakeState {
    posts: Arc<AtomicUsize>,
    actions: Arc<std::sync::Mutex<HashMap<String, Value>>>,
}

fn private_descriptor(root: &Path, endpoint: &str, token: &str, pid: u32, build: &str) -> PathBuf {
    let directory = root.join("browser");
    std::fs::create_dir_all(&directory).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = directory.join("descriptor.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version": PROTOCOL_VERSION,
            "kind": "codexify-browser-companion",
            "pid": pid,
            "endpoint": endpoint,
            "token": token,
            "build": build,
            "createdAt": "2026-10-09T12:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

async fn spawn_fake(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}"), task)
}

async fn spawn_stalled_event_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await.unwrap();
            if read == 0 {
                return;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let event = concat!(
            "{\"sequence\":3,\"actionId\":\"action_1\",\"atMs\":3,",
            "\"type\":\"queued\"}\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n{:X}\r\n{}\r\n",
            event.len(),
            event
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    (format!("http://{address}"), task)
}

async fn spawn_heartbeat_stalled_event_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await.unwrap();
            if read == 0 {
                return;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let event = concat!(
            "{\"sequence\":3,\"actionId\":\"action_1\",\"atMs\":3,",
            "\"type\":\"queued\"}\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n{:X}\r\n{}\r\n",
            event.len(),
            event
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if stream.write_all(b"1\r\n\n\r\n").await.is_err() {
                return;
            }
            if stream.flush().await.is_err() {
                return;
            }
        }
    });
    (format!("http://{address}"), task)
}

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        == Some(&format!("Bearer {TOKEN}"))
}

async fn health(headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    axum::Json(json!({
        "protocolVersion": PROTOCOL_VERSION,
        "kind": "codexify-browser-companion",
        "build": BUILD,
        "status": "idle",
        "authenticated": true,
        "accountFingerprint": "account_fingerprint_1",
        "activeActionId": null
    }))
    .into_response()
}

async fn capabilities(headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    axum::Json(json!({
        "protocolVersion": PROTOCOL_VERSION,
        "build": BUILD,
        "accountFingerprint": "account_fingerprint_1",
        "catalogRevision": "catalog_revision_1",
        "models": [{
            "id": "gpt-6-sol",
            "label": "GPT-6 Sol",
            "efforts": [
                {"id":"medium","label":"Medium","available":true,"reason":null},
                {"id":"high","label":"High","available":true,"reason":null}
            ]
        }]
    }))
    .into_response()
}

async fn login(headers: HeaderMap) -> Response {
    health(headers).await
}

async fn reveal(headers: HeaderMap, axum::Json(body): axum::Json<Value>) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    assert_eq!(body["url"], "https://chatgpt.com/c/example");
    axum::Json(json!({
        "protocolVersion":PROTOCOL_VERSION,
        "build":BUILD,
        "revealed":true,
        "activeActionId":null
    }))
    .into_response()
}

async fn cancel_action(headers: HeaderMap, axum::Json(body): axum::Json<Value>) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    assert_eq!(body["actionId"], "action_1");
    assert_eq!(body["idempotencyKey"], "cancel_1");
    axum::Json(json!({
        "protocolVersion":PROTOCOL_VERSION,
        "build":BUILD,
        "actionId":"action_1",
        "state":"idle",
        "cancelled":true
    }))
    .into_response()
}

async fn execute(
    State(state): State<FakeState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    state.posts.fetch_add(1, Ordering::SeqCst);
    let action_id = body["actionId"].as_str().unwrap().to_string();
    state.actions.lock().unwrap().insert(
        action_id.clone(),
        json!({
            "protocolVersion":PROTOCOL_VERSION,
            "build":BUILD,
            "actionId":action_id,
            "state":"queued",
            "sendAccepted":false,
            "canonicalUrl":null,
            "selected":null,
            "lastEventSequence":1,
            "attention":null
        }),
    );
    (StatusCode::OK, "accepted but receipt lost").into_response()
}

async fn retryable_execute(
    State(state): State<FakeState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let attempt = state.posts.fetch_add(1, Ordering::SeqCst) + 1;
    if attempt == 1 {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let action_id = body["actionId"].as_str().unwrap().to_string();
    let snapshot = json!({
        "protocolVersion":PROTOCOL_VERSION,
        "build":BUILD,
        "actionId":action_id,
        "state":"queued",
        "sendAccepted":false,
        "canonicalUrl":null,
        "selected":null,
        "lastEventSequence":1,
        "attention":null
    });
    state
        .actions
        .lock()
        .unwrap()
        .insert(action_id, snapshot.clone());
    axum::Json(snapshot).into_response()
}

async fn action_state(
    State(state): State<FakeState>,
    headers: HeaderMap,
    AxumPath(action_id): AxumPath<String>,
) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.actions.lock().unwrap().get(&action_id).cloned() {
        Some(value) => axum::Json(value).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn ordered_events(headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let body = [
        json!({"sequence":3,"actionId":"action_1","atMs":3,"type":"queued"}),
        json!({"sequence":4,"actionId":"action_1","atMs":4,"type":"answer_delta","markdown":"visible answer","continuation":false}),
        json!({"sequence":5,"actionId":"action_1","atMs":5,"type":"generation_complete","finalMarkdown":"visible answer","canonicalUrl":"https://chatgpt.com/c/example"}),
    ]
    .into_iter()
    .map(|value| serde_json::to_string(&value).unwrap())
    .collect::<Vec<_>>()
    .join("\n");
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        format!("{body}\n"),
    )
        .into_response()
}

async fn out_of_order_events(headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        concat!(
            "{\"sequence\":4,\"actionId\":\"action_1\",\"atMs\":4,\"type\":\"queued\"}\n",
            "{\"sequence\":4,\"actionId\":\"action_1\",\"atMs\":5,\"type\":\"generation_complete\",\"finalMarkdown\":\"x\",\"canonicalUrl\":null}\n"
        ),
    )
        .into_response()
}

async fn gapped_events(headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        concat!(
            "{\"sequence\":3,\"actionId\":\"action_1\",\"atMs\":3,\"type\":\"queued\"}\n",
            "{\"sequence\":5,\"actionId\":\"action_1\",\"atMs\":5,\"type\":\"generation_complete\",\"finalMarkdown\":\"x\",\"canonicalUrl\":null}\n"
        ),
    )
        .into_response()
}

async fn unknown_events(headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        "{\"sequence\":3,\"actionId\":\"action_1\",\"atMs\":3,\"type\":\"private_unknown_event\",\"secret\":\"private-event-payload\"}\n",
    )
        .into_response()
}

async fn wrong_action_events(headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        "{\"sequence\":3,\"actionId\":\"other_action\",\"atMs\":3,\"type\":\"queued\"}\n",
    )
        .into_response()
}

async fn oversized_events(headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let value = json!({
        "sequence":3,
        "actionId":"action_1",
        "atMs":3,
        "type":"answer_delta",
        "markdown":"private-event-payload".repeat(10_000),
        "continuation":false
    });
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        format!("{}\n", serde_json::to_string(&value).unwrap()),
    )
        .into_response()
}

async fn protocol_bounded_failure_event(headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let value = json!({
        "sequence":3,
        "actionId":"action_1",
        "atMs":3,
        "type":"failed",
        "code":"selector_drift",
        "message":"x".repeat(8 * 1024)
    });
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        format!("{}\n", serde_json::to_string(&value).unwrap()),
    )
        .into_response()
}

#[test]
fn descriptor_validation_is_private_loopback_only_and_secret_safe() {
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(
        root.path(),
        "http://127.0.0.1:43123",
        TOKEN,
        std::process::id(),
        BUILD,
    );
    let client = BrowserCompanionClient::from_descriptor_path(&path).unwrap();
    let debug = format!("{client:?}");
    assert!(!debug.contains(TOKEN));
    assert!(debug.contains(BUILD));

    for endpoint in [
        "https://127.0.0.1:43123",
        "http://localhost:43123",
        "http://0.0.0.0:43123",
        "http://127.0.0.1:43123/path",
        "http://user:pass@127.0.0.1:43123",
    ] {
        let path = private_descriptor(root.path(), endpoint, TOKEN, std::process::id(), BUILD);
        assert!(BrowserCompanionClient::from_descriptor_path(&path).is_err());
    }

    let secret = URL_SAFE_NO_PAD.encode([7u8; 32]);
    let path = private_descriptor(
        root.path(),
        "http://127.0.0.1:43123",
        &secret,
        std::process::id(),
        BUILD,
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = BrowserCompanionClient::from_descriptor_path(&path)
            .err()
            .unwrap();
        assert!(!error.contains(&secret));
    }
}

#[cfg(unix)]
#[test]
fn descriptor_rejects_symlinks_permissive_parents_and_dead_processes() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(
        root.path(),
        "http://127.0.0.1:43123",
        TOKEN,
        std::process::id(),
        BUILD,
    );
    let link = root.path().join("descriptor-link.json");
    symlink(&path, &link).unwrap();
    assert!(BrowserCompanionClient::from_descriptor_path(&link).is_err());

    std::fs::set_permissions(
        path.parent().unwrap(),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(BrowserCompanionClient::from_descriptor_path(&path).is_err());
    std::fs::set_permissions(
        path.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();

    let path = private_descriptor(
        root.path(),
        "http://127.0.0.1:43123",
        TOKEN,
        u32::MAX,
        BUILD,
    );
    assert!(BrowserCompanionClient::from_descriptor_path(&path).is_err());
}

#[tokio::test]
async fn health_and_capabilities_validate_auth_protocol_build_and_catalog() {
    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/session/capabilities", post(capabilities));
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path(&path).unwrap();

    let health = client.health(&CancellationToken::new()).await.unwrap();
    assert!(health.authenticated);
    assert_eq!(
        health.account_fingerprint.as_deref(),
        Some("account_fingerprint_1")
    );
    let catalog = client
        .capabilities(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(catalog.catalog_revision, "catalog_revision_1");
    assert_eq!(catalog.models[0].id, "gpt-6-sol");
    assert_eq!(catalog.models[0].efforts.len(), 2);

    task.abort();
}

#[tokio::test]
async fn login_reveal_cancel_and_action_status_use_the_authenticated_contract() {
    let state = FakeState::default();
    state.actions.lock().unwrap().insert(
        "action_1".into(),
        json!({
            "protocolVersion":PROTOCOL_VERSION,
            "build":BUILD,
            "actionId":"action_1",
            "state":"generating",
            "sendAccepted":true,
            "canonicalUrl":"https://chatgpt.com/c/example",
            "selected":{"model":"gpt-6-sol","effort":"high"},
            "lastEventSequence":7,
            "attention":null
        }),
    );
    let app = Router::new()
        .route("/v1/session/login", post(login))
        .route("/v1/window/reveal", post(reveal))
        .route("/v1/actions/cancel", post(cancel_action))
        .route("/v1/actions/{action_id}", get(action_state))
        .with_state(state);
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path(&path).unwrap();

    assert!(
        client
            .login(&CancellationToken::new())
            .await
            .unwrap()
            .authenticated
    );
    assert!(
        client
            .reveal(
                &RevealWindowRequest {
                    url: Some("https://chatgpt.com/c/example".into()),
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap()
            .revealed
    );
    let snapshot = client
        .action("action_1", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(snapshot.state, CompanionActionState::Generating);
    assert!(snapshot.send_accepted);
    assert_eq!(snapshot.selected.unwrap().effort, "high");
    assert!(
        client
            .cancel("action_1", "cancel_1", &CancellationToken::new())
            .await
            .unwrap()
            .cancelled
    );

    task.abort();
}

#[tokio::test]
async fn protocol_build_and_redirect_mismatches_fail_closed() {
    async fn wrong_build(headers: HeaderMap) -> Response {
        if !authorized(&headers) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        axum::Json(json!({
            "protocolVersion":PROTOCOL_VERSION,
            "kind":"codexify-browser-companion",
            "build":"other-build",
            "status":"idle",
            "authenticated":false,
            "accountFingerprint":null,
            "activeActionId":null
        }))
        .into_response()
    }

    let app = Router::new().route("/v1/health", get(wrong_build));
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path(&path).unwrap();
    assert!(
        client
            .health(&CancellationToken::new())
            .await
            .unwrap_err()
            .contains("build")
    );
    task.abort();

    let followed = Arc::new(AtomicUsize::new(0));
    let capture = followed.clone();
    let app = Router::new()
        .route(
            "/v1/health",
            get(|| async {
                Response::builder()
                    .status(StatusCode::TEMPORARY_REDIRECT)
                    .header(header::LOCATION, "/capture")
                    .body(Body::empty())
                    .unwrap()
            }),
        )
        .route(
            "/capture",
            get(move || {
                let capture = capture.clone();
                async move {
                    capture.fetch_add(1, Ordering::SeqCst);
                    StatusCode::NO_CONTENT
                }
            }),
        );
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path(&path).unwrap();
    assert!(client.health(&CancellationToken::new()).await.is_err());
    assert_eq!(followed.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
async fn invalid_capability_catalogs_fail_closed() {
    async fn invalid_catalog(headers: HeaderMap) -> Response {
        if !authorized(&headers) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        axum::Json(json!({
            "protocolVersion":PROTOCOL_VERSION,
            "build":BUILD,
            "accountFingerprint":"account_fingerprint_1",
            "catalogRevision":"catalog_revision_1",
            "models":[{
                "id":"gpt-6-sol",
                "label":"GPT-6 Sol",
                "efforts":[
                    {"id":"high","label":"High","available":false,"reason":null}
                ]
            }]
        }))
        .into_response()
    }

    let app = Router::new().route("/v1/session/capabilities", post(invalid_catalog));
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path(&path).unwrap();

    assert!(
        client
            .capabilities(&CancellationToken::new())
            .await
            .unwrap_err()
            .contains("explanation")
    );
    task.abort();
}

#[tokio::test]
async fn capability_text_bounds_match_the_typescript_protocol() {
    async fn wide_catalog(headers: HeaderMap) -> Response {
        if !authorized(&headers) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        axum::Json(json!({
            "protocolVersion":PROTOCOL_VERSION,
            "build":BUILD,
            "accountFingerprint":"account_fingerprint_1",
            "catalogRevision":"catalog_revision_1",
            "models":[{
                "id":"gpt-6-sol",
                "label":"M".repeat(200),
                "efforts":[
                    {
                        "id":"xhigh",
                        "label":"E".repeat(200),
                        "available":false,
                        "reason":"R".repeat(500)
                    }
                ]
            }]
        }))
        .into_response()
    }

    let app = Router::new().route("/v1/session/capabilities", post(wide_catalog));
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path(&path).unwrap();

    let catalog = client
        .capabilities(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(catalog.models[0].label.len(), 200);
    assert_eq!(catalog.models[0].efforts[0].label.len(), 200);
    assert_eq!(
        catalog.models[0].efforts[0].reason.as_ref().unwrap().len(),
        500
    );
    task.abort();
}

#[tokio::test]
async fn action_attention_uses_the_protocol_delta_bound() {
    async fn action_with_attention(headers: HeaderMap) -> Response {
        if !authorized(&headers) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let detail = "detail".repeat(900);
        axum::Json(json!({
            "protocolVersion":PROTOCOL_VERSION,
            "build":BUILD,
            "actionId":"action_1",
            "state":"approval_required",
            "sendAccepted":true,
            "canonicalUrl":"https://chatgpt.com/c/example",
            "selected":{"model":"gpt-6-sol","effort":"high"},
            "lastEventSequence":7,
            "attention":{"code":"approval_required","message":detail}
        }))
        .into_response()
    }

    let app = Router::new().route("/v1/actions/action_1", get(action_with_attention));
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path(&path).unwrap();

    let snapshot = client
        .action("action_1", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(snapshot.attention.unwrap().message.unwrap().len(), 5400);
    task.abort();
}

#[tokio::test]
async fn cancellation_and_response_bounds_are_enforced_without_echoing_payloads() {
    async fn slow_health() -> Response {
        tokio::time::sleep(Duration::from_secs(60)).await;
        axum::Json(json!({})).into_response()
    }
    async fn oversized_health() -> Response {
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("private-payload".repeat(100_000)))
            .unwrap()
    }

    let app = Router::new().route("/v1/health", get(oversized_health));
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client =
        BrowserCompanionClient::from_descriptor_path_with_timeout(&path, Duration::from_secs(5))
            .unwrap();
    let error = client
        .health(&CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert!(error.contains("too large"));
    assert!(!error.contains("private-payload"));
    task.abort();

    let app = Router::new().route("/v1/health", get(slow_health));
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client =
        BrowserCompanionClient::from_descriptor_path_with_timeout(&path, Duration::from_secs(5))
            .unwrap();
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let worker = tokio::spawn(async move { client.health(&worker_cancellation).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(1), worker)
        .await
        .unwrap()
        .unwrap()
        .err()
        .unwrap();
    assert!(error.contains("cancelled"));
    task.abort();
}

async fn exercise_event_stream(app: Router, valid: bool) {
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path(&path).unwrap();
    let mut kinds = Vec::new();
    let result = client
        .stream_action_events("action_1", 2, &CancellationToken::new(), |event| {
            kinds.push(event.kind.clone());
            Ok(())
        })
        .await;
    if valid {
        result.unwrap();
        assert!(matches!(kinds[0], CompanionEventKind::Queued));
        assert!(matches!(kinds[1], CompanionEventKind::AnswerDelta { .. }));
        assert!(matches!(
            kinds[2],
            CompanionEventKind::GenerationComplete { .. }
        ));
    } else {
        assert!(result.unwrap_err().contains("sequence"));
    }
    task.abort();
}

async fn event_stream_error(app: Router) -> String {
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path(&path).unwrap();
    let error = client
        .stream_action_events("action_1", 2, &CancellationToken::new(), |_| Ok(()))
        .await
        .unwrap_err();
    task.abort();
    error
}

fn action_request() -> ExecuteActionRequest {
    ExecuteActionRequest {
        action_id: "action_1".into(),
        idempotency_key: "idempotency_1".into(),
        url: "https://chatgpt.com/c/example".into(),
        pairing_generation: 1,
        connector_name: "Codexify".into(),
        expected_account_fingerprint: "account-1".into(),
        model: Some("gpt-6-sol".into()),
        effort: Some("high".into()),
        prompt: ActionPrompt::OfflineGoal { goal_revision: 3 },
        expected_url: "https://chatgpt.com/c/example".into(),
        auto_approve_tool_calls: false,
        bypass_conversation_cooldown: false,
    }
}

#[test]
fn action_prompt_wire_fields_are_camel_case_like_the_protocol_envelope() {
    let value = serde_json::to_value(ExecuteActionRequest {
        prompt: ActionPrompt::OfflineCombined {
            batch_id: "batch_1".into(),
            messages: vec![CompanionUnreadMessage {
                id: "message_1".into(),
                markdown: "Continue.".into(),
            }],
            goal_revision: 3,
        },
        ..action_request()
    })
    .unwrap();

    assert_eq!(value["prompt"]["batchId"], "batch_1");
    assert_eq!(value["prompt"]["goalRevision"], 3);
    assert!(value["prompt"].get("batch_id").is_none());
    assert!(value["prompt"].get("goal_revision").is_none());
}

#[tokio::test]
async fn malformed_execute_receipt_reconciles_status_without_replaying_send() {
    let state = FakeState::default();
    let app = Router::new()
        .route("/v1/actions/execute", post(execute))
        .route("/v1/actions/{action_id}", get(action_state))
        .with_state(state.clone());
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path(&path).unwrap();

    let receipt = client
        .execute(&action_request(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(receipt.action_id, "action_1");
    assert_eq!(receipt.state, CompanionActionState::Queued);
    assert_eq!(state.posts.load(Ordering::SeqCst), 1);

    task.abort();
}

#[tokio::test]
async fn retryable_pre_acceptance_failure_reuses_the_same_idempotent_action_once() {
    let state = FakeState::default();
    let app = Router::new()
        .route("/v1/actions/execute", post(retryable_execute))
        .route("/v1/actions/{action_id}", get(action_state))
        .with_state(state.clone());
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path(&path).unwrap();

    let receipt = client
        .execute(&action_request(), &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(receipt.state, CompanionActionState::Queued);
    assert_eq!(state.posts.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn invalid_action_data_is_rejected_before_network_access() {
    let state = FakeState::default();
    let app = Router::new()
        .route("/v1/actions/execute", post(execute))
        .with_state(state.clone());
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path(&path).unwrap();

    let mut invalid = Vec::new();
    let mut request = action_request();
    request.url = "http://chatgpt.com/c/example".into();
    invalid.push(request);
    let mut request = action_request();
    request.pairing_generation = 0;
    invalid.push(request);
    let mut request = action_request();
    request.model = Some("bad model".into());
    invalid.push(request);
    let mut request = action_request();
    request.connector_name = "😀".repeat(40);
    invalid.push(request);
    let mut request = action_request();
    request.prompt = ActionPrompt::OfflineGoal { goal_revision: 0 };
    invalid.push(request);
    let mut request = action_request();
    request.prompt = ActionPrompt::PairVerify {
        reference: "short".into(),
    };
    invalid.push(request);
    let mut request = action_request();
    request.prompt = ActionPrompt::OfflineChat {
        batch_id: "batch_1".into(),
        messages: vec![CompanionUnreadMessage {
            id: "message_1".into(),
            markdown: "x".repeat(300 * 1024),
        }],
    };
    invalid.push(request);
    let mut request = action_request();
    request.prompt = ActionPrompt::OfflineChat {
        batch_id: "batch_1".into(),
        messages: vec![CompanionUnreadMessage {
            id: "message_1".into(),
            markdown: "x\0y".into(),
        }],
    };
    invalid.push(request);
    let mut request = action_request();
    request.prompt = ActionPrompt::OfflineChat {
        batch_id: "batch_1".into(),
        messages: vec![CompanionUnreadMessage {
            id: "message_1".into(),
            markdown: "&".repeat(60 * 1024),
        }],
    };
    invalid.push(request);

    for request in invalid {
        assert!(
            client
                .execute(&request, &CancellationToken::new())
                .await
                .is_err()
        );
    }
    assert_eq!(state.posts.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
async fn protocol_valid_prompt_can_use_the_full_json_transport_bound() {
    let state = FakeState::default();
    let app = Router::new()
        .route("/v1/actions/execute", post(execute))
        .route("/v1/actions/{action_id}", get(action_state))
        .with_state(state.clone());
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path(&path).unwrap();
    let mut request = action_request();
    request.prompt = ActionPrompt::OfflineChat {
        batch_id: "batch_1".into(),
        messages: vec![CompanionUnreadMessage {
            id: "message_1".into(),
            markdown: "\\\"".repeat(100_000),
        }],
    };

    let snapshot = client
        .execute(&request, &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(snapshot.state, CompanionActionState::Queued);
    assert_eq!(state.posts.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn event_stream_is_bounded_strictly_ordered_and_typed() {
    exercise_event_stream(
        Router::new().route("/v1/actions/action_1/events", get(ordered_events)),
        true,
    )
    .await;
    exercise_event_stream(
        Router::new().route("/v1/actions/action_1/events", get(out_of_order_events)),
        false,
    )
    .await;
    exercise_event_stream(
        Router::new().route("/v1/actions/action_1/events", get(gapped_events)),
        false,
    )
    .await;
}

#[tokio::test]
async fn event_stream_rejects_unknown_wrong_action_and_oversized_events_without_echoing() {
    for (app, expected) in [
        (
            Router::new().route("/v1/actions/action_1/events", get(unknown_events)),
            "invalid",
        ),
        (
            Router::new().route("/v1/actions/action_1/events", get(wrong_action_events)),
            "wrong action",
        ),
        (
            Router::new().route("/v1/actions/action_1/events", get(oversized_events)),
            "too large",
        ),
    ] {
        let error = event_stream_error(app).await;
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains("private-event-payload"));
    }
}

#[tokio::test]
async fn event_stream_accepts_protocol_bounded_terminal_messages() {
    let app = Router::new().route(
        "/v1/actions/action_1/events",
        get(protocol_bounded_failure_event),
    );
    let (endpoint, task) = spawn_fake(app).await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path(&path).unwrap();
    let mut message_bytes = None;

    client
        .stream_action_events("action_1", 2, &CancellationToken::new(), |event| {
            if let CompanionEventKind::Failed { message, .. } = event.kind {
                message_bytes = Some(message.len());
            }
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(message_bytes, Some(8 * 1024));
    task.abort();
}

#[tokio::test]
async fn event_stream_idle_timeout_is_bounded_per_client() {
    let (endpoint, task) = spawn_stalled_event_server().await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path_with_timeouts(
        &path,
        Duration::from_secs(5),
        Duration::from_millis(50),
    )
    .unwrap();

    let started = std::time::Instant::now();
    let error = client
        .stream_action_events("action_1", 2, &CancellationToken::new(), |_| Ok(()))
        .await
        .unwrap_err();

    assert!(error.contains("stalled"));
    assert!(started.elapsed() < Duration::from_secs(1));
    task.abort();
}

#[tokio::test]
async fn event_stream_heartbeats_do_not_mask_semantic_idle_timeout() {
    let (endpoint, task) = spawn_heartbeat_stalled_event_server().await;
    let root = tempfile::tempdir().unwrap();
    let path = private_descriptor(root.path(), &endpoint, TOKEN, std::process::id(), BUILD);
    let client = BrowserCompanionClient::from_descriptor_path_with_timeouts(
        &path,
        Duration::from_secs(5),
        Duration::from_millis(60),
    )
    .unwrap();

    let result = tokio::time::timeout(
        Duration::from_millis(500),
        client.stream_action_events("action_1", 2, &CancellationToken::new(), |_| Ok(())),
    )
    .await;

    let error = result
        .expect("heartbeats must not keep a semantically idle event stream alive")
        .unwrap_err();
    assert!(error.contains("stalled"), "{error}");
    task.abort();
}
