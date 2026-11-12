use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};
use tokio::process::{Child, Command};

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codexify"));
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

fn write_config(home: &Path, enabled: bool, native: bool) {
    let mut config = json!({"workDir":home,"port":0,"codexMcp":{"enabled":false,"useCli":false},
        "agentChat":{"enabled":true,"port":null,"features":{"agentTeams":true,"externalAgentControl":enabled},
            "automation":{"enabled":true,"connectorName":"Test connector","descriptorPath":home.join("browser/descriptor.json")}}});
    if native {
        std::fs::write(home.join("tunnel.key"), "test-only-control-plane-key").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                home.join("tunnel.key"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        // A deliberately missing runtime proves connection publication precedes
        // tunnel startup without downloading or connecting to a real tunnel.
        config["openaiTunnel"] = json!({"tunnelId":"tunnel_0123456789abcdef0123456789abcdef",
            "apiKeyRef":format!("file:{}",home.join("tunnel.key").display()),"clientPath":home.join("missing-tunnel-client")});
    }
    std::fs::write(
        home.join("config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
}

fn launch(home: &Path) -> Child {
    let log = std::fs::File::create(home.join("boot.log")).unwrap();
    command(home)
        .args(["--config", "config.json"])
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap()
}

async fn ready(home: &Path, child: &mut Child, enabled: bool) -> Value {
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "{}",
                std::fs::read_to_string(home.join("boot.log")).unwrap()
            );
            if let Ok(bytes) = std::fs::read(home.join(".codexify/agents/connection.json"))
                && let Ok(info) = serde_json::from_slice::<Value>(&bytes)
                && info["enabled"] == enabled
            {
                let owner: Value = serde_json::from_slice(
                    &std::fs::read(home.join(".codexify/owner-chat.json")).unwrap(),
                )
                .unwrap();
                if client
                    .get(format!(
                        "http://127.0.0.1:{}/api/chats",
                        owner["port"].as_u64().unwrap()
                    ))
                    .bearer_auth(owner["token"].as_str().unwrap())
                    .send()
                    .await
                    .is_ok_and(|response| response.status().is_success())
                {
                    return info;
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("service published a ready agent connection")
}

#[tokio::test]
async fn universal_connection_and_js_helper_boot_with_the_service_and_follow_restarts() {
    codexify::tls::ensure_crypto_provider();
    let root = tempfile::tempdir().unwrap();
    let home = root.path();
    write_config(home, true, false);
    let mut child = launch(home);
    let original = ready(home, &mut child, true).await;
    assert!(Path::new(original["javascript"].as_str().unwrap()).is_file());
    let profile = command(home)
        .args(["agents", "connection"])
        .output()
        .await
        .unwrap();
    assert!(profile.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&profile.stdout).unwrap(),
        original
    );
    let instructions = command(home)
        .args(["agents", "instructions"])
        .output()
        .await
        .unwrap();
    assert!(instructions.status.success());
    assert!(instructions.stdout.len() < 1000);
    let result: Value = Client::new()
        .post(original["requestUrl"].as_str().unwrap())
        .header(
            "Authorization",
            original["headers"]["Authorization"].as_str().unwrap(),
        )
        .json(&json!({"action":"list"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["teams"], json!([]));
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let mut child = launch(home);
    let restarted = ready(home, &mut child, true).await;
    assert_eq!(restarted["headers"], original["headers"]);
    assert_eq!(restarted["javascript"], original["javascript"]);
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    write_config(home, false, false);
    let mut child = launch(home);
    let disabled = ready(home, &mut child, false).await;
    assert!(disabled.get("headers").is_none());
    assert!(
        !command(home)
            .args(["agents", "connection"])
            .output()
            .await
            .unwrap()
            .status
            .success()
    );
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

#[tokio::test]
async fn universal_agent_connection_is_published_before_native_tunnel_runtime_startup() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path();
    write_config(home, true, true);
    let mut child = launch(home);
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        !status.success(),
        "missing fixture runtime must fail without connecting externally"
    );
    let logs = std::fs::read_to_string(home.join("boot.log")).unwrap();
    assert!(logs.contains("starting OpenAI Secure MCP Tunnel"), "{logs}");
    let connection: Value = serde_json::from_slice(
        &std::fs::read(home.join(".codexify/agents/connection.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(connection["enabled"], true);
    assert!(Path::new(connection["javascript"].as_str().unwrap()).is_file());
    assert!(!logs.contains(connection["headers"]["Authorization"].as_str().unwrap()));
}
