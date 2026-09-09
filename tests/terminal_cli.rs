use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_codexify")
}

fn isolated_command(root: &TempDir) -> Command {
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(binary());
    command
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("CLICOLOR_FORCE", "1")
        .env_remove("NO_COLOR")
        .env_remove("CODEXIFY_CONFIG");
    command
}

fn strip_ansi(input: &[u8]) -> String {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == 0x1b && input.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < input.len() {
                let byte = input[index];
                index += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
        } else {
            output.push(input[index]);
            index += 1;
        }
    }
    String::from_utf8(output).unwrap()
}

#[test]
fn service_logs_colorizes_structure_and_pretty_prints_payload_json() {
    let root = TempDir::new().unwrap();
    let log_dir = root.path().join("home/.codexify/logs");
    fs::create_dir_all(&log_dir).unwrap();
    fs::write(
        log_dir.join("codexify.log"),
        concat!(
            "2026-09-09T13:28:56.990854Z  INFO codexify::tool_payload: ",
            "tool invocation completed call_id=7 phase=\"finish\" tool=apply_patch ",
            "status=\"ok\" duration_ms=12 response={\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}\n"
        ),
    )
    .unwrap();

    let output = isolated_command(&root)
        .args(["service", "logs"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.windows(2).any(|bytes| bytes == b"\x1b["));
    let plain = strip_ansi(&output.stdout);
    assert!(
        plain.contains("codexify::tool_payload: [apply_patch] tool invocation completed"),
        "{plain}"
    );
    assert!(plain.contains("response:\n"), "{plain}");
    assert!(plain.contains("  {\n"), "{plain}");
    assert!(plain.contains("    \"content\": [\n"), "{plain}");
    assert!(!plain.contains(" tool=apply_patch "), "{plain}");
}

#[test]
fn service_logs_remains_structured_but_plain_when_color_is_disabled() {
    let root = TempDir::new().unwrap();
    let log_dir = root.path().join("home/.codexify/logs");
    fs::create_dir_all(&log_dir).unwrap();
    fs::write(
        log_dir.join("codexify.log"),
        "2026-09-09T13:28:56Z WARN codexify::tool_payload: [doctor] tool invocation completed status=\"error\" response={\"ok\":false}\n",
    )
    .unwrap();

    let output = isolated_command(&root)
        .env_remove("CLICOLOR_FORCE")
        .env("NO_COLOR", "1")
        .args(["service", "logs"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(!output.stdout.windows(2).any(|bytes| bytes == b"\x1b["));
    let plain = String::from_utf8(output.stdout).unwrap();
    assert!(
        plain.contains("[doctor] tool invocation completed"),
        "{plain}"
    );
    assert!(plain.contains("response:\n"), "{plain}");
    assert!(
        plain.contains("    {\n      \"ok\": false\n    }"),
        "{plain}"
    );
}

#[test]
fn doctor_uses_adaptive_color_without_coloring_json_output() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let config = root.path().join("config.json");
    fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "workDir": project,
            "codexMcp": { "enabled": false }
        }))
        .unwrap(),
    )
    .unwrap();

    let colored = isolated_command(&root)
        .args(["doctor", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(colored.stdout.windows(2).any(|bytes| bytes == b"\x1b["));
    let plain = strip_ansi(&colored.stdout);
    assert!(plain.contains("Codexify doctor"), "{plain}");
    assert!(plain.contains("PASS runtime:"), "{plain}");
    assert!(plain.contains("Result:"), "{plain}");

    let json = isolated_command(&root)
        .args(["doctor", "--json", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(!json.stdout.windows(2).any(|bytes| bytes == b"\x1b["));
    let _: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
}
