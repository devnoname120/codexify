use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_codexify")
}

fn command(root: &TempDir) -> Command {
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(binary());
    command
        .current_dir(root.path())
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("CODEXIFY_CONFIG")
        .env_remove("VISUAL")
        .env_remove("EDITOR");
    command
}

fn run(root: &TempDir, args: &[&str]) -> Output {
    command(root).args(args).output().unwrap()
}

fn config_path(root: &TempDir) -> PathBuf {
    root.path().join("home/.codexify/codexify.config.json")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn config_and_path_read_missing_config_without_creating_it() {
    let root = TempDir::new().unwrap();

    let output = run(&root, &["config"]);
    assert_success(&output);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({})
    );
    assert!(output.stderr.is_empty());
    assert!(!config_path(&root).exists());

    let output = run(&root, &["config", "path"]);
    assert_success(&output);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        config_path(&root).to_string_lossy()
    );
    assert!(output.stderr.is_empty());
    assert!(!config_path(&root).exists());
}

#[test]
fn config_set_get_and_unset_preserve_unrelated_values() {
    let root = TempDir::new().unwrap();
    let path = config_path(&root);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "workDir": "/existing/project",
            "unknownFutureSetting": {"keep": true},
            "mcpServers": {"server.with.dot": {"enabled": false}}
        }))
        .unwrap(),
    )
    .unwrap();

    let output = run(&root, &["config", "set", "worktrees.mode", "always"]);
    assert_success(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Set worktrees.mode"), "{text}");
    assert!(text.contains("codexify service restart"), "{text}");

    let output = run(&root, &["config", "get", "worktrees.mode"]);
    assert_success(&output);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        "always"
    );

    let output = run(
        &root,
        &[
            "config",
            "set",
            r"mcpServers.server\.with\.dot.enabled",
            "true",
        ],
    );
    assert_success(&output);
    let output = run(&root, &["config", "get", r"mcpServers.server\.with\.dot"]);
    assert_success(&output);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"enabled": true})
    );

    let output = run(&root, &["config", "set", "port", "4100"]);
    assert_success(&output);
    let output = run(&root, &["config", "set", "debug", "true"]);
    assert_success(&output);
    let output = run(&root, &["config", "set", "label", r#""quoted""#]);
    assert_success(&output);

    let output = run(&root, &["config", "unset", "worktrees.mode"]);
    assert_success(&output);
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Removed worktrees.mode")
    );

    let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(value["unknownFutureSetting"], json!({"keep": true}));
    assert_eq!(value["mcpServers"]["server.with.dot"]["enabled"], true);
    assert_eq!(value["port"], 4100);
    assert_eq!(value["debug"], true);
    assert_eq!(value["label"], "quoted");
    assert!(value["worktrees"].get("mode").is_none());
}

#[test]
fn config_get_missing_path_and_unset_missing_path_are_explicit() {
    let root = TempDir::new().unwrap();

    let get = run(&root, &["config", "get", "missing.value"]);
    assert_eq!(get.status.code(), Some(1));
    assert!(get.stdout.is_empty());
    assert!(
        String::from_utf8(get.stderr)
            .unwrap()
            .contains("setting not found: missing.value")
    );

    let unset = run(&root, &["config", "unset", "missing.value"]);
    assert_eq!(unset.status.code(), Some(1));
    assert!(unset.stdout.is_empty());
    assert!(
        String::from_utf8(unset.stderr)
            .unwrap()
            .contains("setting not found: missing.value")
    );
    assert!(!config_path(&root).exists());
}

#[test]
fn malformed_paths_and_non_object_intermediates_fail_without_writes() {
    let root = TempDir::new().unwrap();
    let path = config_path(&root);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"{\n  \"port\": 3000\n}\n").unwrap();
    let before = fs::read(&path).unwrap();

    for key in ["", ".port", "port.", "a..b", "trailing\\"] {
        let output = run(&root, &["config", "set", key, "value"]);
        assert_eq!(output.status.code(), Some(1), "key={key:?}");
        assert!(output.stdout.is_empty(), "key={key:?}");
        assert_eq!(fs::read(&path).unwrap(), before, "key={key:?}");
    }

    let output = run(&root, &["config", "set", "port.value", "1"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("port is not an object")
    );
    assert_eq!(fs::read(path).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn config_set_refuses_symlinks_and_preserves_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let root = TempDir::new().unwrap();
    let path = config_path(&root);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"{}\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

    let output = run(&root, &["config", "set", "port", "4100"]);
    assert_success(&output);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );

    let fresh = TempDir::new().unwrap();
    let output = run(&fresh, &["config", "set", "port", "4100"]);
    assert_success(&output);
    assert_eq!(
        fs::metadata(config_path(&fresh))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let target = root.path().join("target.json");
    fs::write(&target, b"{}\n").unwrap();
    fs::remove_file(&path).unwrap();
    symlink(&target, &path).unwrap();
    let output = run(&root, &["config", "set", "port", "4200"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("symlinked config file")
    );
    assert_eq!(fs::read(target).unwrap(), b"{}\n");
}

#[cfg(unix)]
#[test]
fn config_edit_uses_visual_then_editor_and_commits_only_valid_objects() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new().unwrap();
    let editor = root.path().join("editor.sh");
    fs::write(
        &editor,
        r#"#!/bin/sh
printf '%s\n' "$@" > "$EDITOR_ARGS"
printf '%s\n' '{"port":4567,"future":{"kept":true}}' > "$1"
"#,
    )
    .unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
    let args_file = root.path().join("editor-args");

    let output = command(&root)
        .env("VISUAL", &editor)
        .env("EDITOR", "/definitely/not/used")
        .env("EDITOR_ARGS", &args_file)
        .args(["config", "edit"])
        .output()
        .unwrap();
    assert_success(&output);
    let edited: Value = serde_json::from_slice(&fs::read(config_path(&root)).unwrap()).unwrap();
    assert_eq!(edited, json!({"port": 4567, "future": {"kept": true}}));
    assert_eq!(fs::read_to_string(args_file).unwrap().lines().count(), 1);
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Saved configuration")
    );

    let invalid = root.path().join("invalid-editor.sh");
    fs::write(&invalid, "#!/bin/sh\nprintf '[]\\n' > \"$1\"\n").unwrap();
    fs::set_permissions(&invalid, fs::Permissions::from_mode(0o755)).unwrap();
    let before = fs::read(config_path(&root)).unwrap();
    let output = command(&root)
        .env("EDITOR", &invalid)
        .args(["config", "edit"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("must contain a JSON object")
    );
    assert_eq!(fs::read(config_path(&root)).unwrap(), before);
}

#[test]
fn config_help_lists_management_commands() {
    let output = run(&TempDir::new().unwrap(), &["config", "--help"]);
    assert_success(&output);
    let help = String::from_utf8(output.stdout).unwrap();
    for command in ["path", "get", "set", "unset", "edit"] {
        assert!(
            help.lines()
                .any(|line| line.trim_start().starts_with(&format!("{command} "))),
            "missing {command}:\n{help}"
        );
    }
}
