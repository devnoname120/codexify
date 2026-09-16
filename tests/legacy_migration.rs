use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codexify"));
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CODEX_HOME", home.join("codex"))
        .env_remove("CODEXIFY_CONFIG")
        .current_dir(home);
    command
}

fn write_config(home: &Path, legacy: bool, value: &Value) -> std::path::PathBuf {
    let path = if legacy {
        home.join(".codex-free/codex.config.json")
    } else {
        home.join(".codexify/codexify.config.json")
    };
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    path
}

fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn validate_migrated(home: &Path) -> Value {
    let path = home.join(".codexify/codexify.config.json");
    assert_success(
        command(home)
            .args(["--config"])
            .arg(&path)
            .args(["config", "validate"])
            .output()
            .unwrap(),
    );
    let config: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(config["schemaVersion"], 1);
    config
}

#[test]
fn migrated_configs_pass_the_actual_startup_loader_without_cli_overrides() {
    for settings in [
        json!({}),
        json!({"port":4242,"multiProject":true,"exec":{"maxSessions":12}}),
        json!({"review":{"maxPatchBytes":8388608},"output":{"maxFileLines":2000}}),
        json!({"openaiTunnel":{"tunnelId":"tunnel_0123456789abcdef0123456789abcdef","apiKeyRef":"env:MIGRATION_TEST_KEY"}}),
    ] {
        let home = tempfile::tempdir().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let mut settings = settings;
        settings["workDir"] = json!(workspace);
        settings["codexMcp"] = json!({"enabled":false});
        write_config(home.path(), true, &settings);
        assert_success(
            command(home.path())
                .arg("migrate-legacy-install")
                .output()
                .unwrap(),
        );
        assert_eq!(validate_migrated(home.path())["workDir"], json!(workspace));
        assert_success(
            command(home.path())
                .arg("migrate-legacy-install")
                .output()
                .unwrap(),
        );
        validate_migrated(home.path());
    }
}

#[test]
fn legacy_cli_work_dir_can_be_supplied_without_guessing_an_access_root() {
    let home = tempfile::tempdir().unwrap();
    let legacy = write_config(
        home.path(),
        true,
        &json!({"port":4242,"codexMcp":{"enabled":false}}),
    );
    let original = fs::read(&legacy).unwrap();
    let missing = command(home.path())
        .arg("migrate-legacy-install")
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--work-dir"));
    assert_eq!(fs::read(&legacy).unwrap(), original);
    assert!(!home.path().join(".codexify/codexify.config.json").exists());
    assert_success(
        command(home.path())
            .args(["--work-dir", ".", "migrate-legacy-install"])
            .output()
            .unwrap(),
    );
    let config = validate_migrated(home.path());
    assert!(Path::new(config["workDir"].as_str().unwrap()).is_absolute());
    assert_eq!(
        fs::canonicalize(config["workDir"].as_str().unwrap()).unwrap(),
        fs::canonicalize(home.path()).unwrap()
    );
}

#[test]
fn migration_accepts_the_work_dir_after_the_subcommand_as_documented() {
    let home = tempfile::tempdir().unwrap();
    write_config(home.path(), true, &json!({"codexMcp":{"enabled":false}}));
    assert_success(
        command(home.path())
            .args(["migrate-legacy-install", "--work-dir", "."])
            .output()
            .unwrap(),
    );
    validate_migrated(home.path());
}

#[test]
fn even_an_unchanged_destination_must_pass_startup_validation() {
    let home = tempfile::tempdir().unwrap();
    let legacy = write_config(home.path(), true, &json!({}));
    let current = write_config(
        home.path(),
        false,
        &json!({
            "workDir":home.path(), "codexMcp":{"enabled":false},
            "conversationAuthToken":"invalid"
        }),
    );
    let before = fs::read(&current).unwrap();
    let output = command(home.path())
        .arg("migrate-legacy-install")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(legacy.is_file());
    assert_eq!(fs::read(current).unwrap(), before);
}

#[test]
fn existing_configuration_wins_and_new_defaults_do_not_hide_invalid_values() {
    let home = tempfile::tempdir().unwrap();
    let legacy = json!({"workDir":home.path(),"port":4242,"exec":{"maxSessions":12},"codexMcp":{"enabled":false}});
    write_config(home.path(), true, &legacy);
    write_config(
        home.path(),
        false,
        &json!({"workDir":home.path(),"port":5555,"codexMcp":{"enabled":false}}),
    );
    assert_success(
        command(home.path())
            .arg("migrate-legacy-install")
            .output()
            .unwrap(),
    );
    let migrated = validate_migrated(home.path());
    assert_eq!(migrated["port"], 5555);
    assert_eq!(migrated["exec"]["maxSessions"], 12);
}

#[test]
fn invalid_candidates_preserve_both_configs_and_all_legacy_state() {
    for invalid in [
        json!({"workDir":"relative/path"}),
        json!({"workDir":"/this-migration-test-directory-does-not-exist"}),
        json!({"artifactIngress":{"maxFileBytes":0}}),
        json!({"conversationAuthToken":"too-short"}),
        json!({"openaiTunnel":{"tunnelId":"invalid","apiKeyRef":"env:KEY"}}),
    ] {
        for existing in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let mut base = json!({"workDir":home.path(),"codexMcp":{"enabled":false}});
            base.as_object_mut()
                .unwrap()
                .extend(invalid.as_object().unwrap().clone());
            let legacy_value = if existing {
                json!({"port":4242})
            } else {
                base.clone()
            };
            let legacy = write_config(home.path(), true, &legacy_value);
            let before_legacy = fs::read(&legacy).unwrap();
            let current = existing.then(|| write_config(home.path(), false, &base));
            let before_current = current.as_ref().map(|path| fs::read(path).unwrap());
            let state = home.path().join(".codex-free/credentials.txt");
            fs::write(&state, "keep this state").unwrap();
            let output = command(home.path())
                .arg("migrate-legacy-install")
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "candidate unexpectedly accepted: {base}"
            );
            assert_eq!(fs::read(&legacy).unwrap(), before_legacy);
            assert_eq!(fs::read_to_string(&state).unwrap(), "keep this state");
            if let Some(current) = current {
                assert_eq!(fs::read(current).unwrap(), before_current.unwrap());
            } else {
                assert!(!home.path().join(".codexify/codexify.config.json").exists());
            }
        }
    }
}
