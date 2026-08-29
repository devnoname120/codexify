use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_codexify")
}

fn run(args: &[&str], codex_home: &Path) -> Output {
    Command::new(binary())
        .args(args)
        .env("CODEX_HOME", codex_home)
        .output()
        .unwrap()
}

fn write_native_config(codex_home: &Path, project: &Path, trust: &str) -> PathBuf {
    fs::create_dir_all(codex_home).unwrap();
    let path = codex_home.join("config.toml");
    fs::write(
        &path,
        format!(
            "[projects.{}]\ntrust_level = {:?}\n",
            serde_json::to_string(project.to_str().unwrap()).unwrap(),
            trust
        ),
    )
    .unwrap();
    path
}

fn write_catalog_config(path: &Path, project: &Path, name: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "codexMcp": { "enabled": false },
            "projectCatalog": {
                "codexConfig": { "enabled": false },
                "entries": [{ "path": project, "name": name }]
            }
        }))
        .unwrap(),
    )
    .unwrap();
}

fn run_discovered_config(
    access: &Path,
    current_dir: &Path,
    home: &Path,
    codex_home: &Path,
    env_config: Option<&Path>,
) -> (Value, String) {
    let mut command = Command::new(binary());
    command
        .args([
            "projects",
            "list",
            "--work-dir",
            access.to_str().unwrap(),
            "--json",
        ])
        .current_dir(current_dir)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CODEX_HOME", codex_home);
    match env_config {
        Some(path) => {
            command.env("CODEXIFY_CONFIG", path);
        }
        None => {
            command.env_remove("CODEXIFY_CONFIG");
        }
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        serde_json::from_slice(&output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn config_discovery_prefers_env_then_user_and_ignores_working_directory() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    let current_dir = root.path().join("cwd");
    let home = root.path().join("home");
    let codex_home = root.path().join("codex-home");
    let env_project = access.join("env-project");
    let user_project = access.join("user-project");
    let working_directory_project = access.join("working-directory-project");
    for directory in [
        &access,
        &current_dir,
        &home,
        &codex_home,
        &env_project,
        &user_project,
        &working_directory_project,
    ] {
        fs::create_dir_all(directory).unwrap();
    }

    let env_config = root.path().join("env.json");
    let user_config = home.join(".codexify").join("codexify.config.json");
    let working_directory_config = current_dir.join("codexify.config.json");
    write_catalog_config(&env_config, &env_project, "Environment");
    write_catalog_config(&user_config, &user_project, "User");
    write_catalog_config(
        &working_directory_config,
        &working_directory_project,
        "Working Directory",
    );

    let (value, _) =
        run_discovered_config(&access, &current_dir, &home, &codex_home, Some(&env_config));
    assert_eq!(value["projects"][0]["name"], "Environment");

    let (value, _) = run_discovered_config(&access, &current_dir, &home, &codex_home, None);
    assert_eq!(value["projects"][0]["name"], "User");

    fs::remove_file(user_config).unwrap();
    let (value, _) = run_discovered_config(&access, &current_dir, &home, &codex_home, None);
    assert!(value["projects"].as_array().unwrap().is_empty());
}

#[test]
fn json_command_lists_native_projects_with_explicit_metadata() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    let project = access.join("codexify");
    let codex_home = root.path().join("codex-home");
    fs::create_dir_all(&project).unwrap();
    write_native_config(&codex_home, &project, "trusted");
    let local_config = root.path().join("codexify.config.json");
    fs::write(
        &local_config,
        format!(
            r#"{{
                "projectCatalog": {{
                    "entries": [{{
                        "path": {},
                        "name": "Codexify",
                        "aliases": ["ChatGPT bridge"],
                        "description": "Rust MCP bridge"
                    }}]
                }}
            }}"#,
            serde_json::to_string(project.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();

    let output = run(
        &[
            "projects",
            "list",
            "--work-dir",
            access.to_str().unwrap(),
            "--config",
            local_config.to_str().unwrap(),
            "--query",
            "bridge",
            "--json",
        ],
        &codex_home,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("Config:"));
    let value: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["total"], 1);
    assert_eq!(value["projects"][0]["selector"], "codexify");
    assert_eq!(value["projects"][0]["name"], "Codexify");
    assert_eq!(value["projects"][0]["trust_level"], "trusted");
    assert_eq!(
        value["projects"][0]["sources"],
        serde_json::json!(["codex_config", "explicit_metadata"])
    );
    assert_eq!(value["diagnostics"], serde_json::json!([]));
}

#[test]
fn missing_native_and_local_configs_produce_an_empty_successful_result() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    let codex_home = root.path().join("codex-home");
    fs::create_dir_all(&access).unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    let missing_config = root.path().join("missing.json");

    let output = run(
        &[
            "projects",
            "list",
            "--work-dir",
            access.to_str().unwrap(),
            "--config",
            missing_config.to_str().unwrap(),
            "--json",
        ],
        &codex_home,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["total"], 0);
    assert_eq!(value["projects"], serde_json::json!([]));
}

#[test]
fn invalid_native_toml_fails_without_echoing_secrets() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    let codex_home = root.path().join("codex-home");
    fs::create_dir_all(&access).unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    let secret = "native-config-secret-must-not-leak";
    fs::write(
        codex_home.join("config.toml"),
        format!("[projects.demo]\ntrust_level = \\\"{secret}"),
    )
    .unwrap();

    let output = run(
        &[
            "projects",
            "list",
            "--work-dir",
            access.to_str().unwrap(),
            "--config",
            root.path().join("missing.json").to_str().unwrap(),
            "--json",
        ],
        &codex_home,
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("invalid TOML"));
    assert!(!stderr.contains(secret));
    assert!(output.stdout.is_empty());
}

#[test]
fn skipped_absolute_paths_are_disclosed_only_with_the_local_diagnostic_flag() {
    let root = TempDir::new().unwrap();
    let access = root.path().join("projects");
    let outside = root.path().join("private-outside-project");
    let codex_home = root.path().join("codex-home");
    fs::create_dir_all(&access).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    let local_config = root.path().join("codexify.config.json");
    fs::write(
        &local_config,
        format!(
            r#"{{
                "projectCatalog": {{
                    "codexConfig": {{ "enabled": false }},
                    "entries": [{{ "path": {} }}]
                }}
            }}"#,
            serde_json::to_string(outside.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();

    let common = [
        "projects",
        "list",
        "--work-dir",
        access.to_str().unwrap(),
        "--config",
        local_config.to_str().unwrap(),
        "--json",
    ];
    let hidden = run(&common, &codex_home);
    assert!(hidden.status.success());
    let hidden_stdout = String::from_utf8(hidden.stdout).unwrap();
    assert!(!hidden_stdout.contains("private-outside-project"));
    let hidden_value: Value = serde_json::from_str(&hidden_stdout).unwrap();
    assert_eq!(hidden_value["diagnostics"], serde_json::json!([]));
    assert!(
        hidden_value["warnings"][0]
            .as_str()
            .unwrap()
            .contains("outside the configured access root")
    );

    let mut detailed_args = common.to_vec();
    detailed_args.push("--show-skipped");
    let detailed = run(&detailed_args, &codex_home);
    assert!(detailed.status.success());
    let detailed_stdout = String::from_utf8(detailed.stdout).unwrap();
    assert!(detailed_stdout.contains("private-outside-project"));
    let detailed_value: Value = serde_json::from_str(&detailed_stdout).unwrap();
    assert_eq!(
        detailed_value["diagnostics"][0]["reason"],
        "outside_access_root"
    );
}
