//! Exercise real search processes blocked in filesystem I/O, including the
//! single-thread HTTP runtime that would freeze if grep ran synchronously.
#![cfg(unix)]

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    routing::{get, post},
};
use codexify::artifact_egress::ArtifactEgressStore;
use codexify::config::default_config;
use codexify::conversation_auth::ConversationAuthorizationStore;
use codexify::diff::DiffCheckpointManager;
use codexify::exec_sessions::SessionState;
use codexify::tool::{Tool, ToolRequestContext};
use codexify::tools::grep::Grep;
use codexify::types::{ArtifactEgressConfig, ToolResult};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::time::{Instant, sleep, timeout};
use tokio_util::sync::CancellationToken;

// Each test deliberately holds workers open. Serialize the fixtures so a
// timeout test exercises a running worker rather than another test's queue.
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

struct Fixture {
    root: TempDir,
    executable: PathBuf,
    pid_file: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let fifo = CString::new(root.path().join("blocked.txt").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        std::fs::write(root.path().join("small.txt"), "needle\n").unwrap();
        let pid_file = root.path().join("worker.pid");
        let executable = root.path().join("worker-launcher");
        // Record the PID then exec the real binary: the PID we check belongs
        // to the search worker itself, with no shell/sleep descendant to leak.
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > {}\nexec {} \"$@\"\n",
                shell_quote(&pid_file),
                shell_quote(Path::new(env!("CARGO_BIN_EXE_codexify"))),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            root,
            executable,
            pid_file,
        }
    }

    async fn pid(&self) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(text) = tokio::fs::read_to_string(&self.pid_file).await
                && let Ok(pid) = text.parse()
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "worker did not start");
            sleep(Duration::from_millis(10)).await;
        }
    }

    fn context(&self, cancellation: CancellationToken) -> ToolRequestContext {
        ToolRequestContext {
            conversation: None,
            task_conversation: None,
            task_owner_generation: None,
            conversation_retired: false,
            connector_schema_version: None,
            conversation_schema_version: None,
            markdown_chat: Arc::new(codexify::markdown_chat::MarkdownChatStore::default()),
            conversation_authorizations: Arc::new(ConversationAuthorizationStore::new()),
            project_bindings: Arc::new(codexify::project_bindings::ProjectBindingStore::new(
                self.root.path().join("project-bindings"),
            )),
            diff_checkpoints: Arc::new(DiffCheckpointManager::new()),
            artifact_egress: Arc::new(ArtifactEgressStore::new_at(
                ArtifactEgressConfig::default(),
                self.root.path().join("artifact-state"),
            )),
            agent_automation: None,
            agent_runtime:None,
            cancellation,
        }
    }

    fn start(
        &self,
        timeout_ms: u64,
        cancellation: CancellationToken,
    ) -> tokio::task::JoinHandle<ToolResult> {
        let context = self.context(cancellation);
        let config = default_config(self.root.path().to_path_buf());
        let grep = Grep::new(self.executable.clone());
        tokio::spawn(async move {
            grep.call_with_context(
                json!({"pattern": "needle", "timeoutMs": timeout_ms}),
                &config,
                &SessionState::new(),
                &context,
            )
            .await
        })
    }
}

fn assert_reaped(pid: i32) {
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "search worker {pid} still exists"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[tokio::test]
async fn timed_out_filesystem_read_is_killed_and_reaped() {
    let _guard = TEST_LOCK.lock().await;
    let fixture = Fixture::new();
    let search = fixture.start(1_000, CancellationToken::new());
    let pid = fixture.pid().await;
    let result = timeout(Duration::from_secs(3), search)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_error, "{}", result.joined_text());
    assert!(result.joined_text().contains("timed out after 1000 ms"));
    assert_reaped(pid);
}

#[tokio::test]
async fn request_cancellation_kills_and_reaps_a_blocked_search() {
    let _guard = TEST_LOCK.lock().await;
    let fixture = Fixture::new();
    let cancellation = CancellationToken::new();
    let search = fixture.start(30_000, cancellation.clone());
    let pid = fixture.pid().await;
    cancellation.cancel();
    let result = timeout(Duration::from_secs(1), search)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_error);
    assert_eq!(result.joined_text(), "Search cancelled.");
    assert_reaped(pid);
}

#[tokio::test]
async fn dropping_the_request_does_not_leave_a_search_process() {
    let _guard = TEST_LOCK.lock().await;
    let fixture = Fixture::new();
    let search = fixture.start(30_000, CancellationToken::new());
    let pid = fixture.pid().await;
    search.abort();
    assert!(search.await.unwrap_err().is_cancelled());
    // Tokio's kill-on-drop reaper is asynchronous; keep the runtime alive
    // until it has collected the child's exit, rather than observing a zombie.
    let deadline = Instant::now() + Duration::from_secs(1);
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(Instant::now() < deadline, "abandoned worker still exists");
        sleep(Duration::from_millis(10)).await;
    }
    assert_reaped(pid);
}

#[tokio::test]
async fn already_cancelled_requests_do_not_launch_workers() {
    let _guard = TEST_LOCK.lock().await;
    let fixture = Fixture::new();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let result = fixture.start(30_000, cancellation).await.unwrap();
    assert_eq!(result.joined_text(), "Search cancelled.");
    assert!(!fixture.pid_file.exists());
}

#[tokio::test]
async fn directory_tools_kill_workers_blocked_reading_ignore_files() {
    use codexify::tools::{glob::Glob, list_directory::ListDirectory, tree::Tree};
    let _guard = TEST_LOCK.lock().await;
    for (tool, args) in [
        (
            Box::new(Glob) as Box<dyn Tool>,
            json!({"pattern":"*","timeoutMs":1000}),
        ),
        (Box::new(Tree) as Box<dyn Tool>, json!({"timeoutMs":1000})),
        (
            Box::new(ListDirectory) as Box<dyn Tool>,
            json!({"timeoutMs":1000}),
        ),
    ] {
        let fixture = Fixture::new();
        let fifo = CString::new(
            fixture
                .root
                .path()
                .join(".gitignore")
                .as_os_str()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let mut config = default_config(fixture.root.path().to_path_buf());
        config.set_filesystem_worker_executable(fixture.executable.clone());
        let work =
            tokio::spawn(async move { tool.call(args, &config, &SessionState::new()).await });
        let pid = fixture.pid().await;
        let result = timeout(Duration::from_secs(3), work)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_error, "{}", result.joined_text());
        assert!(result.joined_text().contains("timed out after 1000 ms"));
        assert_reaped(pid);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn http_and_another_search_progress_while_a_search_is_blocked() {
    let _guard = TEST_LOCK.lock().await;
    codexify::tls::ensure_crypto_provider();
    let fixture = Fixture::new();
    let root = fixture.root.path().to_path_buf();
    let executable = fixture.executable.clone();
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/search",
            post(move |Json(args): Json<Value>| {
                let config = default_config(root.clone());
                let grep = Grep::new(executable.clone());
                async move {
                    let result = grep.call(args, &config, &SessionState::new()).await;
                    Json(json!({"isError": result.is_error, "text": result.joined_text()}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let search_client = client.clone();
    let search_url = format!("{base_url}/search");
    let blocked = tokio::spawn(async move {
        search_client
            .post(search_url)
            .json(&json!({"pattern": "needle", "timeoutMs": 2_000}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    });
    let pid = fixture.pid().await;
    assert!(!blocked.is_finished());
    let response = timeout(
        Duration::from_millis(500),
        client.get(format!("{base_url}/health")).send(),
    )
    .await
    .expect("HTTP stalled behind grep")
    .unwrap();
    assert_eq!(response.text().await.unwrap(), "ok");
    let response = timeout(
        Duration::from_secs(1),
        client
            .post(format!("{base_url}/search"))
            .json(&json!({"pattern": "needle", "path": "small.txt"}))
            .send(),
    )
    .await
    .expect("second search stalled behind the first")
    .unwrap();
    let response: Value = response.json().await.unwrap();
    assert_eq!(response["isError"], false, "{response}");
    assert!(
        response["text"]
            .as_str()
            .unwrap()
            .contains("small.txt:1:needle")
    );
    assert!(!blocked.is_finished());
    let response = timeout(Duration::from_secs(3), blocked)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response["isError"], true);
    assert!(response["text"].as_str().unwrap().contains("timed out"));
    assert_reaped(pid);
    server.abort();
}
