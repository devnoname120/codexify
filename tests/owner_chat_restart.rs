use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep, timeout};

#[derive(Deserialize)]
struct ChatInfo {
    port: u16,
    token: String,
}

fn command(home: &Path) -> Command {
    isolated_command(env!("CARGO_BIN_EXE_codexify"), home)
}

fn isolated_command(program: impl AsRef<std::ffi::OsStr>, home: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CODEX_HOME", home.join("codex"))
        .env_remove("CODEXIFY_CONFIG")
        .current_dir(home)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

fn config(home: &Path, port: Option<u16>) {
    fs::write(
        home.join("config.json"),
        serde_json::to_vec(&json!({
            "workDir": home,
            "port": 0,
            "codexMcp": {"enabled": false, "useCli": false},
            "agentChat": {"enabled": true, "port": port}
        }))
        .unwrap(),
    )
    .unwrap();
}

async fn launch(home: &Path, client: &Client) -> (Child, ChatInfo) {
    let log = fs::File::create(home.join("server.log")).unwrap();
    let mut child = isolated_command(std::env::current_exe().unwrap(), home)
        .args([
            "--exact",
            "owner_server_process_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env("CODEXIFY_OWNER_CHAT_TEST_CHILD", "1")
        .stdin(Stdio::piped())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            child.try_wait().unwrap().is_none(),
            "server exited: {}",
            fs::read_to_string(home.join("server.log")).unwrap()
        );
        if let Ok(bytes) = fs::read(home.join(".codexify/owner-chat.json"))
            && let Ok(info) = serde_json::from_slice::<ChatInfo>(&bytes)
            && let Ok(response) = client
                .get(format!("http://127.0.0.1:{}/api/chats", info.port))
                .bearer_auth(&info.token)
                .send()
                .await
            && response.status() == StatusCode::OK
        {
            return (child, info);
        }
        assert!(Instant::now() < deadline, "owner chat did not become ready");
        sleep(Duration::from_millis(50)).await;
    }
}

async fn stop(child: &mut Child, graceful: bool) {
    if graceful {
        child.stdin.as_mut().unwrap().write_all(b"x").await.unwrap();
    } else {
        child.start_kill().unwrap();
    }
    let status = timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("server shutdown timed out")
        .unwrap();
    if graceful {
        assert!(status.success(), "clean shutdown failed: {status}");
    }
}

#[tokio::test]
#[ignore = "invoked by the restart test in an isolated subprocess"]
async fn owner_server_process_fixture() {
    if std::env::var("CODEXIFY_OWNER_CHAT_TEST_CHILD").as_deref() != Ok("1") {
        return;
    }
    let home = std::env::current_dir().unwrap();
    let settings: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join("config.json")).unwrap()).unwrap();
    let mut config = codexify::config::default_config(home.clone());
    config.markdown_chat.enabled = true;
    config.markdown_chat.port = settings["agentChat"]["port"]
        .as_u64()
        .map(|port| port as u16);
    config.memory.dir = Some(home.join("state").display().to_string());
    let artifacts = Arc::new(codexify::artifact_egress::ArtifactEgressStore::new_at(
        config.artifact_egress.clone(),
        home.join("artifacts"),
    ));
    let owner = codexify::owner_chat::start(
        Arc::new(config),
        Arc::new(codexify::markdown_chat::MarkdownChatStore::default()),
        artifacts,
    )
    .await
    .unwrap();
    tokio::io::stdin().read_exact(&mut [0u8; 1]).await.unwrap();
    drop(owner);
}

#[tokio::test]
async fn owner_chat_url_survives_clean_and_abrupt_restarts() {
    codexify::tls::ensure_crypto_provider();
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    config(home, None);
    let (mut child, original) = launch(home, &client).await;
    let original_url = format!("http://127.0.0.1:{}/#{}", original.port, original.token);
    stop(&mut child, true).await;
    assert!(
        home.join(".codexify/owner-chat.json").is_file(),
        "clean shutdown must preserve the chat credential"
    );
    let offline = command(home).arg("chat").output().await.unwrap();
    assert!(
        !offline.status.success(),
        "saved credentials do not imply a live server"
    );

    config(home, Some(original.port));
    for graceful in [false, true] {
        let (mut child, restored) = launch(home, &client).await;
        assert!(
            restored.token == original.token,
            "restart rotated the chat token"
        );
        let printed = command(home).arg("chat").output().await.unwrap();
        assert!(printed.status.success());
        assert!(String::from_utf8(printed.stdout).unwrap().trim() == original_url);
        let endpoint = format!("http://127.0.0.1:{}/api/chats", restored.port);
        let authorized = client
            .get(&endpoint)
            .bearer_auth(&original.token)
            .send()
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);
        let wrong = client
            .get(&endpoint)
            .bearer_auth("wrong-token")
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        let missing = client.get(&endpoint).send().await.unwrap();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        stop(&mut child, graceful).await;
    }

    let reservation = tokio::net::TcpListener::bind(("127.0.0.1", original.port))
        .await
        .unwrap();
    config(home, None);
    let (mut child, moved) = launch(home, &client).await;
    assert_ne!(moved.port, original.port);
    assert!(
        moved.token == original.token,
        "changing the listener port rotated its credential"
    );
    stop(&mut child, true).await;
    drop(reservation);
}
