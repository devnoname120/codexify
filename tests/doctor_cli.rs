use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_codexify")
}

fn isolated_command(root: &TempDir) -> Command {
    let home = root.path().join("home");
    let codex_home = root.path().join("codex-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&codex_home).unwrap();

    let mut command = Command::new(binary());
    command
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("CODEX_HOME", &codex_home)
        .env("SHELL", if cfg!(windows) { "cmd.exe" } else { "/bin/sh" })
        .env_remove("CODEXIFY_CONFIG");
    command
}

fn fake_bin_dir(root: &TempDir, tools: &[&str]) -> PathBuf {
    let directory = root.path().join("bin");
    fs::create_dir_all(&directory).unwrap();
    for tool in tools {
        let target = directory.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::write(&target, format!("#!/bin/sh\nprintf '%s\\n' '{tool} 1.0'\n")).unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        }
        #[cfg(windows)]
        fs::copy(binary(), target).unwrap();
    }
    directory
}

#[cfg(unix)]
fn fake_failing_tool(directory: &Path, tool: &str) {
    use std::os::unix::fs::PermissionsExt;

    let target = directory.join(tool);
    fs::write(&target, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
}

fn write_config(path: &Path, project: &Path, extra: Value) {
    let mut config = serde_json::json!({
        "workDir": project,
        "codexMcp": { "enabled": false }
    });
    let Value::Object(extra) = extra else {
        panic!("test config overlay must be an object");
    };
    config.as_object_mut().unwrap().extend(extra);
    fs::write(path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
}

fn check<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == id)
        .unwrap_or_else(|| panic!("missing doctor check {id}"))
}

#[test]
fn valid_json_report_is_clean_and_successful() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let config = root.path().join("codexify.config.json");
    write_config(&config, &project, serde_json::json!({}));

    let output = isolated_command(&root)
        .args(["doctor", "--json", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(output.stderr.is_empty());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], true);
    assert_eq!(check(&report, "config_path")["status"], "pass");
    assert_eq!(check(&report, "configuration")["status"], "pass");
    assert!(matches!(
        check(&report, "updates")["status"].as_str(),
        Some("pass" | "warning")
    ));
    assert_eq!(check(&report, "service")["status"], "skipped");
}

#[test]
#[cfg(unix)]
fn command_warnings_match_codex_and_include_gh() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    fs::create_dir_all(project.join(".git")).unwrap();
    let config = root.path().join("codexify.config.json");
    write_config(&config, &project, serde_json::json!({}));
    let tools = fake_bin_dir(&root, &["git"]);

    let output = isolated_command(&root)
        .env("PATH", tools)
        .args(["doctor", "--json", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(check(&report, "git")["status"], "pass");
    assert_eq!(check(&report, "rg")["status"], "warning");
    assert_eq!(check(&report, "gh")["status"], "warning");
}

#[test]
#[cfg(unix)]
fn missing_git_warns_when_work_dir_is_a_git_checkout() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    fs::create_dir_all(project.join(".git")).unwrap();
    let config = root.path().join("codexify.config.json");
    write_config(&config, &project, serde_json::json!({}));
    let tools = fake_bin_dir(&root, &["rg", "gh"]);

    let output = isolated_command(&root)
        .env("PATH", tools)
        .args(["doctor", "--json", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(check(&report, "git")["status"], "warning");
}

#[test]
#[cfg(unix)]
fn unusable_git_warns_even_outside_a_git_checkout() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let config = root.path().join("codexify.config.json");
    write_config(&config, &project, serde_json::json!({}));
    let tools = fake_bin_dir(&root, &["rg", "gh"]);
    fake_failing_tool(&tools, "git");

    let output = isolated_command(&root)
        .env("PATH", tools)
        .args(["doctor", "--json", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(check(&report, "git")["status"], "warning");
}

#[test]
fn missing_configured_shell_is_a_failure() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let config = root.path().join("codexify.config.json");
    write_config(
        &config,
        &project,
        serde_json::json!({ "exec": { "defaultShell": "definitely-missing-shell" } }),
    );
    let output = isolated_command(&root)
        .args(["doctor", "--json", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(check(&report, "shell")["status"], "failure");
}

#[test]
#[cfg(unix)]
fn optional_missing_codex_cli_warns_but_required_cli_fails() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let config = root.path().join("codexify.config.json");
    write_config(
        &config,
        &project,
        serde_json::json!({
            "codexMcp": {
                "enabled": true,
                "useCli": true,
                "cliPath": "definitely-missing-codex"
            }
        }),
    );
    let tools = fake_bin_dir(&root, &["git", "rg", "gh"]);

    let optional = isolated_command(&root)
        .env("PATH", &tools)
        .args(["doctor", "--json", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(optional.status.success());
    let report: Value = serde_json::from_slice(&optional.stdout).unwrap();
    assert_eq!(check(&report, "codex_cli")["status"], "warning");

    let required = isolated_command(&root)
        .env("PATH", tools)
        .args([
            "doctor",
            "--json",
            "--codex-cli",
            "--config",
            config.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!required.status.success());
    let report: Value = serde_json::from_slice(&required.stdout).unwrap();
    assert_eq!(check(&report, "codex_cli")["status"], "failure");
}

#[test]
#[cfg(unix)]
fn missing_mcp_stdio_command_warns_without_starting_it() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let marker = root.path().join("mcp-started");
    let config = root.path().join("codexify.config.json");
    write_config(
        &config,
        &project,
        serde_json::json!({
            "mcpServers": {
                "missing": {
                    "command": "definitely-missing-mcp",
                    "args": [marker]
                }
            }
        }),
    );
    let tools = fake_bin_dir(&root, &["git", "rg", "gh"]);

    let output = isolated_command(&root)
        .env("PATH", tools)
        .args(["doctor", "--json", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(check(&report, "mcp_stdio")["status"], "warning");
    assert!(!marker.exists());
}

#[test]
#[cfg(unix)]
fn missing_mcp_stdio_cwd_warns_even_when_command_resolves() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let config = root.path().join("codexify.config.json");
    let tools = fake_bin_dir(&root, &["git", "rg", "gh", "mcp-tool"]);
    write_config(
        &config,
        &project,
        serde_json::json!({
            "mcpServers": {
                "broken-cwd": {
                    "command": "mcp-tool",
                    "cwd": root.path().join("missing-cwd"),
                    "env": { "PATH": tools }
                }
            }
        }),
    );

    let output = isolated_command(&root)
        .env("PATH", tools)
        .args(["doctor", "--json", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(check(&report, "mcp_stdio")["status"], "warning");
    assert!(
        check(&report, "mcp_stdio")["detail"]
            .as_str()
            .unwrap()
            .contains("cwd does not exist")
    );
}

#[test]
fn invalid_config_returns_failure_after_emitting_complete_json() {
    let root = TempDir::new().unwrap();
    let config = root.path().join("codexify.config.json");
    fs::write(&config, "{ invalid json").unwrap();

    let output = isolated_command(&root)
        .args(["doctor", "--json", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(check(&report, "configuration")["status"], "failure");
    assert_eq!(check(&report, "shell")["status"], "skipped");
    assert_eq!(check(&report, "openai_tunnel_runtime")["status"], "skipped");
}

#[test]
fn malformed_tunnel_credential_fails_without_leaking_the_secret() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let config = root.path().join("codexify.config.json");
    write_config(
        &config,
        &project,
        serde_json::json!({
            "openaiTunnel": {
                "tunnelId": "tunnel_0123456789abcdef0123456789abcdef",
                "apiKeyRef": "env:DOCTOR_TUNNEL_KEY"
            }
        }),
    );
    let secret = "secret value that must never leak";

    let output = isolated_command(&root)
        .env("DOCTOR_TUNNEL_KEY", secret)
        .args(["doctor", "--json", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stdout.contains(secret));
    assert!(!stderr.contains(secret));
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        check(&report, "openai_tunnel_credential")["status"],
        "failure"
    );
    assert_eq!(check(&report, "openai_tunnel_runtime")["status"], "warning");
}

#[test]
fn retained_update_lock_is_reported_as_a_warning() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let config = root.path().join("codexify.config.json");
    write_config(&config, &project, serde_json::json!({}));
    let update_dir = root.path().join("home/.codexify/update");
    fs::create_dir_all(&update_dir).unwrap();
    fs::write(update_dir.join("update.lock"), "abc123\n").unwrap();

    let output = isolated_command(&root)
        .args(["doctor", "--json", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(check(&report, "self_update")["status"], "warning");
    assert!(
        check(&report, "self_update")["detail"]
            .as_str()
            .unwrap()
            .contains("abc123")
    );
}

#[test]
fn human_report_uses_stable_status_labels_and_summary() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let config = root.path().join("codexify.config.json");
    write_config(&config, &project, serde_json::json!({}));

    let output = isolated_command(&root)
        .args(["doctor", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("PASS runtime:"));
    assert!(stdout.contains("SKIP service:"));
    assert!(stdout.contains("Result: healthy"));
}
