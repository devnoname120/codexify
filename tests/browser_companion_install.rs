use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use codexify::agent_automation::{
    BrowserArchiveKind, BrowserCompanionManager, BrowserLifecycleState, PROTOCOL_BUILD_SUFFIX,
    PROTOCOL_SCHEMA_HASH, PROTOCOL_VERSION,
};
use codexify::markdown_chat::AgentAutomationConfig;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::json;
use sha2::{Digest, Sha256};
use tar::{Builder, Header};

fn enabled_config(descriptor_path: &Path) -> AgentAutomationConfig {
    AgentAutomationConfig {
        enabled: true,
        descriptor_path: descriptor_path.to_string_lossy().into_owned(),
        ..AgentAutomationConfig::default()
    }
}

fn append_tar_file(builder: &mut Builder<GzEncoder<Vec<u8>>>, path: &str, bytes: &[u8], mode: u32) {
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(mode);
    header.set_cksum();
    builder.append_data(&mut header, path, bytes).unwrap();
}

fn append_tar_symlink(builder: &mut Builder<GzEncoder<Vec<u8>>>, path: &str, target: &str) {
    let mut header = Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    header.set_link_name(target).unwrap();
    header.set_cksum();
    builder
        .append_data(&mut header, path, std::io::empty())
        .unwrap();
}

fn tar_package(platform: &str, version: &str, build: &str, entrypoint: &str) -> Vec<u8> {
    tar_package_with_entrypoint(platform, version, build, entrypoint, b"#!/bin/sh\nexit 0\n")
}

fn tar_package_with_entrypoint(
    platform: &str,
    version: &str,
    build: &str,
    entrypoint: &str,
    entrypoint_bytes: &[u8],
) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = Builder::new(encoder);
    let root = format!("codexify-browser-v{version}-{platform}");
    let manifest = serde_json::to_vec(&json!({
        "version": 1,
        "codexifyVersion": version,
        "protocolVersion": PROTOCOL_VERSION,
        "protocolSchemaHash": PROTOCOL_SCHEMA_HASH,
        "build": build,
        "platform": platform,
        "entrypoint": entrypoint
    }))
    .unwrap();
    append_tar_file(
        &mut builder,
        &format!("{root}/manifest.json"),
        &manifest,
        0o600,
    );
    append_tar_file(
        &mut builder,
        &format!("{root}/{entrypoint}"),
        entrypoint_bytes,
        0o700,
    );
    builder.into_inner().unwrap().finish().unwrap()
}

#[derive(Clone)]
struct ReleaseFixture {
    archive: Arc<Vec<u8>>,
    checksums: Arc<Vec<u8>>,
}

async fn release_archive(State(state): State<ReleaseFixture>) -> Response {
    (
        [(header::CONTENT_TYPE, "application/octet-stream")],
        state.archive.as_ref().clone(),
    )
        .into_response()
}

async fn release_checksums(State(state): State<ReleaseFixture>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        state.checksums.as_ref().clone(),
    )
        .into_response()
}

async fn release_server(
    archive_name: String,
    archive: Vec<u8>,
    checksums: Vec<u8>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let archive_path = format!("/v1.7.0/{archive_name}");
    let state = ReleaseFixture {
        archive: Arc::new(archive),
        checksums: Arc::new(checksums),
    };
    let app = Router::new()
        .route(&archive_path, get(release_archive))
        .route("/v1.7.0/checksums.txt", get(release_checksums))
        .with_state(state);
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), task)
}

fn checksum_line(name: &str, bytes: &[u8]) -> String {
    format!("{:x}  {name}\n", Sha256::digest(bytes))
}

fn install_runtime(manager: &BrowserCompanionManager) {
    let archive = tar_package(
        &manager.release_asset().platform,
        &manager.release_asset().version,
        manager.expected_build(),
        "app/companion",
    );
    let checksums = checksum_line(&manager.release_asset().archive_name, &archive);
    manager
        .install_verified_archive(&archive, checksums.as_bytes())
        .unwrap();
}

#[derive(Clone)]
struct HealthFixture {
    token: String,
    body: Arc<Mutex<serde_json::Value>>,
}

async fn health_fixture(State(state): State<HealthFixture>, headers: HeaderMap) -> Response {
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        != Some(&format!("Bearer {}", state.token))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(state.body.lock().unwrap().clone()).into_response()
}

async fn publish_health_fixture(
    manager: &BrowserCompanionManager,
    body: Arc<Mutex<serde_json::Value>>,
) -> tokio::task::JoinHandle<()> {
    let token = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/health", get(health_fixture))
        .with_state(HealthFixture {
            token: token.clone(),
            body,
        });
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::fs::write(
        &manager.layout().descriptor_path,
        serde_json::to_vec(&json!({
            "version": PROTOCOL_VERSION,
            "kind": "codexify-browser-companion",
            "pid": std::process::id(),
            "endpoint": endpoint,
            "token": token,
            "build": manager.expected_build(),
            "createdAt": "2026-10-10T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            &manager.layout().descriptor_path,
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    task
}

#[test]
fn lifecycle_layout_and_release_asset_are_versioned_and_platform_exact() {
    let root = tempfile::tempdir().unwrap();
    let manager = BrowserCompanionManager::for_test(
        root.path().join("browser"),
        enabled_config(&root.path().join("browser/descriptor.json")),
        "1.7.0",
        "macos",
        "aarch64",
        "https://example.invalid/releases/download",
    )
    .unwrap();

    assert_eq!(
        manager.layout().runtime_dir,
        root.path().join("browser/runtime/1.7.0")
    );
    assert_eq!(
        manager.layout().manifest_path,
        root.path().join("browser/runtime/1.7.0/manifest.json")
    );
    assert_eq!(manager.release_asset().platform, "darwin-arm64");
    assert_eq!(manager.release_asset().kind, BrowserArchiveKind::TarGz);
    assert_eq!(
        manager.release_asset().archive_name,
        "codexify-browser-v1.7.0-darwin-arm64.tar.gz"
    );
    assert_eq!(
        manager.expected_build(),
        format!("1.7.0{PROTOCOL_BUILD_SUFFIX}")
    );
}

#[test]
fn verified_archive_installs_atomically_under_the_private_versioned_runtime() {
    let root = tempfile::tempdir().unwrap();
    let browser_root = root.path().join("browser");
    let manager = BrowserCompanionManager::for_test(
        browser_root.clone(),
        enabled_config(&browser_root.join("descriptor.json")),
        "1.7.0",
        "macos",
        "aarch64",
        "https://example.invalid/releases/download",
    )
    .unwrap();
    let build = manager.expected_build().to_string();
    let archive = tar_package("darwin-arm64", "1.7.0", &build, "app/companion");
    let checksums = checksum_line(&manager.release_asset().archive_name, &archive);

    let receipt = manager
        .install_verified_archive(&archive, checksums.as_bytes())
        .unwrap();

    assert_eq!(receipt.version, "1.7.0");
    assert_eq!(receipt.build, build);
    assert_eq!(receipt.runtime_dir, manager.layout().runtime_dir);
    assert_eq!(
        receipt.entrypoint,
        manager.layout().runtime_dir.join("app/companion")
    );
    assert_eq!(
        std::fs::read_to_string(&receipt.entrypoint).unwrap(),
        "#!/bin/sh\nexit 0\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&browser_root)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&receipt.runtime_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_ne!(
            std::fs::metadata(&receipt.entrypoint)
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
}

#[test]
fn install_rejects_disabled_automation_bad_checksums_and_incompatible_manifests() {
    let root = tempfile::tempdir().unwrap();
    let browser_root = root.path().join("browser");
    let mut disabled = enabled_config(&browser_root.join("descriptor.json"));
    disabled.enabled = false;
    let disabled_manager = BrowserCompanionManager::for_test(
        browser_root.clone(),
        disabled,
        "1.7.0",
        "macos",
        "aarch64",
        "https://example.invalid/releases/download",
    )
    .unwrap();
    assert!(
        disabled_manager
            .install_verified_archive(b"private archive", b"not a checksum")
            .unwrap_err()
            .contains("disabled")
    );
    assert!(!browser_root.exists());

    let manager = BrowserCompanionManager::for_test(
        browser_root.clone(),
        enabled_config(&browser_root.join("descriptor.json")),
        "1.7.0",
        "macos",
        "aarch64",
        "https://example.invalid/releases/download",
    )
    .unwrap();
    let archive = tar_package(
        "darwin-arm64",
        "1.7.0",
        manager.expected_build(),
        "app/companion",
    );
    assert!(
        manager
            .install_verified_archive(&archive, b"0  other-file\n")
            .unwrap_err()
            .contains("does not contain")
    );
    assert!(!manager.layout().runtime_dir.exists());

    let incompatible = tar_package(
        "darwin-arm64",
        "1.7.0",
        &format!("wrong{PROTOCOL_BUILD_SUFFIX}"),
        "app/companion",
    );
    let checksums = checksum_line(&manager.release_asset().archive_name, &incompatible);
    assert!(
        manager
            .install_verified_archive(&incompatible, checksums.as_bytes())
            .unwrap_err()
            .contains("build")
    );
    assert!(!manager.layout().runtime_dir.exists());
}

#[cfg(unix)]
#[test]
fn tar_install_preserves_only_relative_links_that_stay_inside_the_package() {
    let root = tempfile::tempdir().unwrap();
    let browser_root = root.path().join("browser");
    let manager = BrowserCompanionManager::for_test(
        browser_root,
        enabled_config(&root.path().join("browser/descriptor.json")),
        "1.7.0",
        "macos",
        "aarch64",
        "https://example.invalid/releases/download",
    )
    .unwrap();
    let package_root = format!(
        "codexify-browser-v{}-{}",
        manager.release_asset().version,
        manager.release_asset().platform
    );
    let manifest = serde_json::to_vec(&json!({
        "version": 1,
        "codexifyVersion": manager.release_asset().version,
        "protocolVersion": PROTOCOL_VERSION,
        "protocolSchemaHash": PROTOCOL_SCHEMA_HASH,
        "build": manager.expected_build(),
        "platform": manager.release_asset().platform,
        "entrypoint": "app/companion"
    }))
    .unwrap();

    let build_archive = |target: &str| {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);
        append_tar_file(
            &mut builder,
            &format!("{package_root}/manifest.json"),
            &manifest,
            0o600,
        );
        append_tar_file(
            &mut builder,
            &format!("{package_root}/app/companion"),
            b"#!/bin/sh\nexit 0\n",
            0o700,
        );
        append_tar_file(
            &mut builder,
            &format!("{package_root}/app/Versions/A/resource"),
            b"resource",
            0o600,
        );
        append_tar_symlink(
            &mut builder,
            &format!("{package_root}/app/Versions/Current"),
            target,
        );
        builder.into_inner().unwrap().finish().unwrap()
    };

    let archive = build_archive("A");
    let checksums = checksum_line(&manager.release_asset().archive_name, &archive);
    let receipt = manager
        .install_verified_archive(&archive, checksums.as_bytes())
        .unwrap();
    assert_eq!(
        std::fs::read_link(receipt.runtime_dir.join("app/Versions/Current")).unwrap(),
        Path::new("A")
    );

    let unsafe_archive = build_archive("../../../outside");
    let unsafe_checksums = checksum_line(&manager.release_asset().archive_name, &unsafe_archive);
    let error = manager
        .install_verified_archive(&unsafe_archive, unsafe_checksums.as_bytes())
        .unwrap_err();
    assert!(error.contains("symlink"), "{error}");
}

#[tokio::test]
async fn lifecycle_status_distinguishes_disabled_missing_stopped_and_live_states() {
    let root = tempfile::tempdir().unwrap();
    let browser_root = root.path().join("browser");
    let mut disabled = enabled_config(&browser_root.join("descriptor.json"));
    disabled.enabled = false;
    let disabled_manager = BrowserCompanionManager::for_test(
        browser_root.clone(),
        disabled,
        "1.7.0",
        "macos",
        "aarch64",
        "https://example.invalid/releases/download",
    )
    .unwrap();
    let status = disabled_manager.status().await;
    assert_eq!(status.state, BrowserLifecycleState::Disabled);
    assert!(!browser_root.exists());

    let manager = BrowserCompanionManager::for_test(
        browser_root.clone(),
        enabled_config(&browser_root.join("descriptor.json")),
        "1.7.0",
        "macos",
        "aarch64",
        "https://example.invalid/releases/download",
    )
    .unwrap();
    assert_eq!(
        manager.status().await.state,
        BrowserLifecycleState::NotInstalled
    );
    install_runtime(&manager);
    assert_eq!(manager.status().await.state, BrowserLifecycleState::Stopped);

    let body = Arc::new(Mutex::new(json!({
        "protocolVersion": PROTOCOL_VERSION,
        "kind": "codexify-browser-companion",
        "build": manager.expected_build(),
        "status": "idle",
        "authenticated": true,
        "accountFingerprint": "account-1",
        "activeActionId": null
    })));
    let server = publish_health_fixture(&manager, body.clone()).await;
    assert_eq!(manager.status().await.state, BrowserLifecycleState::Ready);

    *body.lock().unwrap() = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "kind": "codexify-browser-companion",
        "build": manager.expected_build(),
        "status": "authentication_required",
        "authenticated": false,
        "accountFingerprint": null,
        "activeActionId": null
    });
    assert_eq!(
        manager.status().await.state,
        BrowserLifecycleState::AuthenticationRequired
    );

    for authenticated in [false, true] {
        *body.lock().unwrap() = json!({
            "protocolVersion": PROTOCOL_VERSION, "kind": "codexify-browser-companion",
            "build": manager.expected_build(), "status": "unavailable", "authenticated": authenticated,
            "accountFingerprint": if authenticated { Some("account-1") } else { None }, "activeActionId": null
        });
        assert_eq!(
            manager.status().await.state,
            BrowserLifecycleState::Unavailable
        );
    }

    *body.lock().unwrap() = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "kind": "codexify-browser-companion",
        "build": manager.expected_build(),
        "status": "busy",
        "authenticated": true,
        "accountFingerprint": "account-1",
        "activeActionId": "action-1"
    });
    let status = manager.status().await;
    assert_eq!(status.state, BrowserLifecycleState::Busy);
    assert_eq!(status.active_action_id.as_deref(), Some("action-1"));

    manager.record_account_fingerprint("account-1").unwrap();
    *body.lock().unwrap() = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "kind": "codexify-browser-companion",
        "build": manager.expected_build(),
        "status": "idle",
        "authenticated": true,
        "accountFingerprint": "account-2",
        "activeActionId": null
    });
    let status = manager.status().await;
    assert_eq!(status.state, BrowserLifecycleState::AccountChanged);
    assert!(
        !serde_json::to_string(&status)
            .unwrap()
            .contains("account-1")
    );
    assert!(
        !serde_json::to_string(&status)
            .unwrap()
            .contains("account-2")
    );
    server.abort();
}

#[tokio::test]
async fn network_install_downloads_the_exact_current_release_asset_and_checksum() {
    let root = tempfile::tempdir().unwrap();
    let browser_root = root.path().join("browser");
    let preliminary = BrowserCompanionManager::for_test(
        browser_root.clone(),
        enabled_config(&browser_root.join("descriptor.json")),
        "1.7.0",
        "macos",
        "aarch64",
        "http://127.0.0.1:1",
    )
    .unwrap();
    let archive = tar_package(
        "darwin-arm64",
        "1.7.0",
        preliminary.expected_build(),
        "app/companion",
    );
    let checksums = checksum_line(&preliminary.release_asset().archive_name, &archive);
    let (release_root, server) = release_server(
        preliminary.release_asset().archive_name.clone(),
        archive,
        checksums.into_bytes(),
    )
    .await;
    let manager = BrowserCompanionManager::for_test(
        browser_root,
        enabled_config(&root.path().join("browser/descriptor.json")),
        "1.7.0",
        "macos",
        "aarch64",
        &release_root,
    )
    .unwrap();

    let receipt = manager.install().await.unwrap();
    assert_eq!(receipt.version, "1.7.0");
    assert!(receipt.entrypoint.is_file());
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn installed_companion_starts_logs_in_reveals_and_stops_as_a_user_process() {
    const FIXTURE: &str = r#"#!/usr/bin/env python3
import http.server
import json
import os
import signal
import sys

descriptor = os.environ["CODEXIFY_BROWSER_DESCRIPTOR"]
build = os.environ["CODEXIFY_BROWSER_BUILD"]
assert json.loads(os.environ["CODEXIFY_BROWSER_REQUEST_BLOCK_LIST"]) == [r"^https?://example\.invalid/blocked"]
assert json.loads(os.environ["CODEXIFY_BROWSER_NAVIGATION_TIMINGS"]) == {
    "conversationLoadIntervalSeconds": 23,
    "conversationUnavailableCooldownSeconds": 47,
    "rateLimitCooldownSeconds": 89,
}
token = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"

class Handler(http.server.BaseHTTPRequestHandler):
    def authorized(self):
        return self.headers.get("Authorization") == "Bearer " + token

    def send_json(self, value, status=200):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def health(self):
        return {
            "protocolVersion": 1,
            "kind": "codexify-browser-companion",
            "build": build,
            "status": "idle",
            "authenticated": True,
            "accountFingerprint": "fixture-account",
            "activeActionId": None,
        }

    def do_GET(self):
        if not self.authorized():
            self.send_json({}, 401)
        elif self.path == "/v1/health":
            self.send_json(self.health())
        else:
            self.send_json({}, 404)

    def do_POST(self):
        if not self.authorized():
            self.send_json({}, 401)
            return
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        if self.path == "/v1/session/login":
            self.send_json(self.health())
        elif self.path == "/v1/window/reveal":
            self.send_json({
                "protocolVersion": 1,
                "build": build,
                "revealed": True,
                "activeActionId": None,
            })
        else:
            self.send_json({}, 404)

    def log_message(self, *_args):
        pass

server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
endpoint = "http://127.0.0.1:%d/" % server.server_address[1]
temporary = descriptor + ".tmp"
with open(temporary, "w") as output:
    json.dump({
        "version": 1,
        "kind": "codexify-browser-companion",
        "pid": os.getpid(),
        "endpoint": endpoint,
        "token": token,
        "build": build,
        "createdAt": "2026-10-10T00:00:00Z",
    }, output)
os.chmod(temporary, 0o600)
os.replace(temporary, descriptor)

def stop(_signal, _frame):
    try:
        os.remove(descriptor)
    except FileNotFoundError:
        pass
    sys.exit(0)

signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)
server.serve_forever()
"#;

    let root = tempfile::tempdir().unwrap();
    let browser_root = root.path().join("browser");
    let mut launch_config = enabled_config(&browser_root.join("descriptor.json"));
    launch_config.request_blocking_enabled = true;
    launch_config
        .retry_timings
        .conversation_load_interval_seconds = 23;
    launch_config
        .retry_timings
        .conversation_unavailable_cooldown_seconds = 47;
    launch_config.retry_timings.rate_limit_cooldown_seconds = 89;
    launch_config.request_block_list = vec![r"^https?://example\.invalid/blocked".into()];
    let manager = BrowserCompanionManager::for_test(
        browser_root.clone(),
        launch_config,
        "1.7.0",
        "macos",
        "aarch64",
        "https://example.invalid/releases/download",
    )
    .unwrap();
    let archive = tar_package_with_entrypoint(
        "darwin-arm64",
        "1.7.0",
        manager.expected_build(),
        "app/companion",
        FIXTURE.as_bytes(),
    );
    let checksums = checksum_line(&manager.release_asset().archive_name, &archive);
    manager
        .install_verified_archive(&archive, checksums.as_bytes())
        .unwrap();

    assert_eq!(
        manager.start().await.unwrap().state,
        BrowserLifecycleState::Ready
    );
    assert_eq!(
        manager.login().await.unwrap().state,
        BrowserLifecycleState::Ready
    );
    assert_eq!(
        manager.reveal().await.unwrap().state,
        BrowserLifecycleState::Ready
    );
    assert!(manager.layout().log_path.is_file());
    let newer_manager = BrowserCompanionManager::for_test(
        browser_root.clone(),
        enabled_config(&browser_root.join("descriptor.json")),
        "1.8.0",
        "macos",
        "aarch64",
        "https://example.invalid/releases/download",
    )
    .unwrap();
    assert_eq!(
        newer_manager.stop().await.unwrap().state,
        BrowserLifecycleState::Stopped
    );
    assert!(!manager.layout().descriptor_path.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn stop_refuses_to_signal_a_pid_until_the_authenticated_companion_responds() {
    let root = tempfile::tempdir().unwrap();
    let browser_root = root.path().join("browser");
    let manager = BrowserCompanionManager::for_test(
        browser_root.clone(),
        enabled_config(&browser_root.join("descriptor.json")),
        "1.7.0",
        "macos",
        "aarch64",
        "https://example.invalid/releases/download",
    )
    .unwrap();
    install_runtime(&manager);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    drop(listener);
    let mut unrelated = std::process::Command::new("/bin/sleep")
        .arg("60")
        .spawn()
        .unwrap();
    std::fs::write(
        &manager.layout().descriptor_path,
        serde_json::to_vec(&json!({
            "version": PROTOCOL_VERSION,
            "kind": "codexify-browser-companion",
            "pid": unrelated.id(),
            "endpoint": endpoint,
            "token": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "build": manager.expected_build(),
            "createdAt": "2026-10-10T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        &manager.layout().descriptor_path,
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();

    let error = manager.stop().await.unwrap_err();
    assert!(error.contains("request failed"), "{error}");
    assert!(unrelated.try_wait().unwrap().is_none());

    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
}

#[cfg(unix)]
fn recovery_fixture() -> (
    tempfile::TempDir,
    BrowserCompanionManager,
    codexify::agent_automation::BrowserCompanionControl,
) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("companion.c");
    std::fs::write(
        &source,
        include_bytes!("fixtures/browser_companion_lifecycle.c"),
    )
    .unwrap();
    let executable = root.path().join("companion");
    let compiled = std::process::Command::new("/usr/bin/cc")
        .args(["-std=c11", "-D_POSIX_C_SOURCE=200809L"])
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let browser = root.path().join("browser");
    let config = enabled_config(&browser.join("descriptor.json"));
    let manager = BrowserCompanionManager::for_test(
        browser,
        config.clone(),
        "1.7.0",
        std::env::consts::OS,
        std::env::consts::ARCH,
        "https://example.invalid/releases/download",
    )
    .unwrap();
    let archive = tar_package_with_entrypoint(
        &manager.release_asset().platform,
        "1.7.0",
        manager.expected_build(),
        "app/companion",
        &std::fs::read(executable).unwrap(),
    );
    let checksums = checksum_line(&manager.release_asset().archive_name, &archive);
    manager
        .install_verified_archive(&archive, checksums.as_bytes())
        .unwrap();
    let control =
        codexify::agent_automation::BrowserCompanionControl::with_manager(config, manager.clone());
    (root, manager, control)
}
#[cfg(unix)]
async fn wait_until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while !ready() {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
}
#[cfg(unix)]
#[tokio::test]
async fn service_maintenance_preserves_recovery_and_manual_stop_without_replaying_actions() {
    use tokio_util::sync::CancellationToken;
    let (root, manager, control) = recovery_fixture();
    let token = CancellationToken::new();
    let first = control.start(&token).await.unwrap().pid();
    let profile = manager.layout().root.join("chrome-profile");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("keep"), b"signed-in-profile").unwrap();

    // A normal server restart holds recovery in the old server, but does not
    // claim that the user chose to stop the browser permanently.
    control.stop_for_restart().await.unwrap();
    assert_eq!(control.public_status()["manuallyStopped"], false);
    assert_eq!(control.public_status()["waitingForServerRestart"], true);
    assert!(
        control
            .ensure_running(&token)
            .await
            .unwrap_err()
            .contains("waiting for Codexify to restart")
    );

    let config = root.path().join("maintenance.json");
    std::fs::write(&config, json!({
        "workDir":root.path(),"codexMcp":{"enabled":false},
        "agentChat":{"automation":{"enabled":true,"descriptorPath":manager.layout().descriptor_path}}
    }).to_string()).unwrap();
    let maintenance_stop = || {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_codexify"));
        command
            .args(["browser", "stop-for-restart", "--config"])
            .arg(&config)
            .env("CODEXIFY_BROWSER_HOME", &manager.layout().root);
        command
    };
    // This is the same separate-process handoff used by both updater scripts.
    // The next server sees a different PID and may recover automatically.
    let output = maintenance_stop().output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(control.public_status()["manuallyStopped"], false);
    assert_eq!(control.public_status()["waitingForServerRestart"], false);
    let next = control.ensure_running(&token).await.unwrap().pid();
    assert_ne!(first, next);

    control.stop(&token).await.unwrap();
    let starts = std::fs::read(manager.layout().root.join("starts")).unwrap();
    let output = maintenance_stop().output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(control.public_status()["manuallyStopped"], true);
    assert!(
        control
            .ensure_running(&token)
            .await
            .unwrap_err()
            .contains("stopped by you")
    );
    assert_eq!(
        std::fs::read(manager.layout().root.join("starts")).unwrap(),
        starts
    );
    assert_eq!(
        std::fs::read(profile.join("keep")).unwrap(),
        b"signed-in-profile"
    );
    assert!(
        std::fs::read_to_string(manager.layout().root.join("requests"))
            .unwrap()
            .lines()
            .all(|line| matches!(
                line,
                "GET /v1/health HTTP/1.1" | "GET /v1/liveness HTTP/1.1"
            ))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn browser_watchdog_reopens_a_closed_companion_and_honors_manual_stop_across_controls() {
    use codexify::agent_automation::{BrowserCompanionClient, BrowserCompanionControl};
    use tokio_util::sync::CancellationToken;
    let (_root, manager, control) = recovery_fixture();
    let token = CancellationToken::new();
    let client = control.start(&token).await.unwrap();
    let first = client.pid();
    let profile = manager.layout().root.join("chrome-profile");
    std::fs::create_dir(&profile).unwrap();
    std::fs::write(profile.join("keep"), b"signed-in-profile").unwrap();
    let worker = control.watch();
    unsafe { libc::kill(first as i32, libc::SIGTERM) };
    wait_until(|| {
        BrowserCompanionClient::from_descriptor_path(&manager.layout().descriptor_path)
            .is_ok_and(|c| c.pid() != first)
    })
    .await;
    control.stop(&token).await.unwrap();
    let starts = std::fs::read(manager.layout().root.join("starts")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5300)).await;
    assert!(!manager.layout().descriptor_path.exists());
    assert_eq!(
        std::fs::read(manager.layout().root.join("starts")).unwrap(),
        starts
    );
    let config = enabled_config(&manager.layout().descriptor_path);
    let reopened = BrowserCompanionControl::with_manager(config, manager.clone());
    assert!(
        reopened
            .ensure_running(&token)
            .await
            .unwrap_err()
            .contains("stopped by you")
    );
    let client = reopened.start(&token).await.unwrap();
    assert_ne!(client.pid(), first);
    reopened.stop(&token).await.unwrap();
    drop(worker);
    assert_eq!(
        std::fs::read(profile.join("keep")).unwrap(),
        b"signed-in-profile"
    );
    assert!(
        std::fs::read_to_string(manager.layout().root.join("requests"))
            .unwrap()
            .lines()
            .all(|line| matches!(
                line,
                "GET /v1/health HTTP/1.1" | "GET /v1/liveness HTTP/1.1"
            ))
    );
}
#[cfg(unix)]
#[tokio::test]
async fn browser_recovery_confirms_renderer_failure_and_preserves_unobserved_processes() {
    use tokio_util::sync::CancellationToken;
    let (_root, manager, control) = recovery_fixture();
    let token = CancellationToken::new();
    let peer = control.clone();
    let (first_start, second_start) = tokio::join!(control.start(&token), peer.start(&token));
    let client = first_start.unwrap();
    assert_eq!(client.pid(), second_start.unwrap().pid());
    assert_eq!(
        std::fs::read_to_string(manager.layout().root.join("starts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let first = client.pid();
    let mode = manager.layout().root.join("health-mode");
    for state in ["busy", "authentication_required", "session_unavailable"] {
        std::fs::write(&mode, state).unwrap();
        assert_eq!(control.ensure_running(&token).await.unwrap().pid(), first);
    }
    std::fs::write(&mode, "unavailable").unwrap();
    let deferred = control.ensure_running(&token).await.unwrap_err();
    assert!(
        deferred.contains("retaining its live process"),
        "{deferred}"
    );
    assert_eq!(
        codexify::agent_automation::BrowserCompanionClient::from_descriptor_path(
            &manager.layout().descriptor_path
        )
        .unwrap()
        .pid(),
        first
    );
    // Recovery requires separate negative probes spanning a full minute.
    // Exercise that policy with the real process instead of expecting the
    // first unavailable observation to authorize a restart.
    let second = tokio::time::timeout(std::time::Duration::from_secs(90), async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            match control.ensure_running(&token).await {
                Ok(client) => break client.pid(),
                Err(error) => assert!(error.contains("retaining its live process"), "{error}"),
            }
        }
    })
    .await
    .unwrap();
    assert_ne!(second, first);
    std::fs::write(&mode, "frozen").unwrap();
    let unobserved = tokio::time::timeout(
        std::time::Duration::from_secs(40),
        control.ensure_running(&token),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(
        unobserved.contains("live browser was preserved"),
        "{unobserved}"
    );
    assert_eq!(
        codexify::agent_automation::BrowserCompanionClient::from_descriptor_path(
            &manager.layout().descriptor_path
        )
        .unwrap()
        .pid(),
        second
    );
    control.stop(&token).await.unwrap();
    assert!(
        std::fs::read_to_string(manager.layout().root.join("requests"))
            .unwrap()
            .lines()
            .all(|line| matches!(
                line,
                "GET /v1/health HTTP/1.1" | "GET /v1/liveness HTTP/1.1"
            ))
    );
}
#[cfg(unix)]
#[tokio::test]
async fn explicit_stop_cancels_browser_startup_without_waiting_for_its_deadline() {
    use tokio_util::sync::CancellationToken;
    let (_root, manager, control) = recovery_fixture();
    std::fs::write(manager.layout().root.join("delay-start"), b"delay").unwrap();
    let starting = control.clone();
    let start = tokio::spawn(async move { starting.start(&CancellationToken::new()).await });
    wait_until(|| manager.layout().root.join("starts").exists()).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        control.stop(&CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(start.await.unwrap().unwrap_err().contains("stopped by you"));
    assert!(!manager.layout().descriptor_path.exists());
}
#[cfg(unix)]
#[tokio::test]
async fn stopping_an_orphan_only_targets_the_managed_executable_with_the_matching_profile() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (_root, manager, _control) = recovery_fixture();
    let runtime = manager.layout().runtime_dir.join("app/companion");
    // A copied shell blocks in a builtin without spawning unrelated subprocesses.
    std::fs::copy("/bin/sh", &runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
    let profile = manager.layout().root.join("chrome-profile");
    std::fs::create_dir(&profile).unwrap();
    let other = manager.layout().root.join("other-profile");
    std::fs::create_dir(&other).unwrap();
    for matching in [false, true] {
        let mut child = std::process::Command::new(&runtime)
            .args(["-c", "read -r value"])
            .arg(format!(
                "--user-data-dir={}",
                if matching { &profile } else { &other }.display()
            ))
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        symlink(
            format!("lifecycle-host-{pid}"),
            profile.join("SingletonLock"),
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        manager.stop().await.unwrap();
        if matching {
            child.wait().unwrap();
        } else {
            assert!(child.try_wait().unwrap().is_none());
            child.kill().unwrap();
            child.wait().unwrap();
        }
        std::fs::remove_file(profile.join("SingletonLock")).unwrap();
    }
}
