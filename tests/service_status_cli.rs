use std::process::Command;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_codexify")
}

#[test]
fn service_help_lists_public_lifecycle_commands() {
    let output = Command::new(binary())
        .args(["service", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    for command in [
        "start", "stop", "restart", "enable", "disable", "status", "logs",
    ] {
        assert!(
            text.lines()
                .any(|line| line.trim_start().starts_with(&format!("{command} "))),
            "missing {command}:\n{text}"
        );
    }
}

#[test]
fn service_status_help_exposes_json_and_exit_codes() {
    let output = Command::new(binary())
        .args(["service", "status", "--help"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("--json"), "{text}");
    for state in [
        "0",
        "running",
        "3",
        "stopped",
        "4",
        "not installed",
        "1",
        "query",
    ] {
        assert!(text.contains(state), "missing {state}: {text}");
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod native {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use serde_json::{Value, json};
    use tempfile::TempDir;

    struct Fixture {
        root: TempDir,
        home: PathBuf,
        bin: PathBuf,
        definition: PathBuf,
        config: PathBuf,
    }

    impl Fixture {
        fn new(installed: bool) -> Self {
            let root = TempDir::new().unwrap();
            let home = root.path().join("home");
            let bin = root.path().join("bin");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&bin).unwrap();
            let config = root.path().join("invalid-config.json");
            fs::write(&config, "deliberately invalid configuration").unwrap();
            #[cfg(target_os = "linux")]
            let definition = home.join("xdg/systemd/user/codexify.service");
            #[cfg(target_os = "macos")]
            let definition = home.join("Library/LaunchAgents/dev.codexify.service.plist");
            if installed {
                fs::create_dir_all(definition.parent().unwrap()).unwrap();
                fs::write(&definition, "fixture service definition").unwrap();
            }
            let fixture = Self {
                root,
                home,
                bin,
                definition,
                config,
            };
            fixture.write_manager(false);
            fixture
        }

        fn write_manager(&self, fail: bool) {
            #[cfg(target_os = "linux")]
            let (name, body) = (
                "systemctl",
                r#"
case "$*" in
  '--user is-active codexify.service')
    if [ "$FIXTURE_RUNNING" = true ]; then printf 'active\n'; else printf 'inactive\n'; exit 3; fi ;;
  '--user is-enabled codexify.service')
    if [ "$FIXTURE_ENABLED" = true ]; then printf 'enabled\n'; else printf 'disabled\n'; exit 1; fi ;;
  '--user start codexify.service'|'--user stop codexify.service'|'--user restart codexify.service') ;;
  *) printf 'unexpected service command\n' >&2; exit 91 ;;
esac
"#,
            );
            #[cfg(target_os = "macos")]
            let (name, body) = (
                "launchctl",
                r#"
case "$1" in
  print)
    running="$FIXTURE_RUNNING"
    if [ -n "$FIXTURE_STATE" ] && [ -f "$FIXTURE_STATE" ]; then IFS= read -r running < "$FIXTURE_STATE"; fi
    if [ "$running" = true ]; then printf 'state = running\npid = 4242\n'; else exit 3; fi ;;
  print-disabled)
    if [ "$FIXTURE_ENABLED" = true ]; then printf '"dev.codexify.service" => false\n'; else printf '"dev.codexify.service" => true\n'; fi ;;
  bootout)
    printf 'false' > "$FIXTURE_STATE" ;;
  bootstrap|kickstart)
    printf 'true' > "$FIXTURE_STATE" ;;
  *) printf 'unexpected service command\n' >&2; exit 91 ;;
esac
"#,
            );
            let body = if fail {
                "printf 'fixture query failure\\n' >&2\nexit 1\n"
            } else {
                body
            };
            let path = self.bin.join(name);
            fs::write(
                &path,
                format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$FIXTURE_CALLS\"\n{body}"),
            )
            .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn command(&self) -> Command {
            let mut command = Command::new(binary());
            command
                .current_dir(&self.home)
                .env("HOME", &self.home)
                .env("USERPROFILE", &self.home)
                .env("XDG_CONFIG_HOME", self.home.join("xdg"))
                .env("CODEXIFY_CONFIG", &self.config)
                .env("PATH", &self.bin)
                .env("FIXTURE_CALLS", self.root.path().join("calls"))
                .env("FIXTURE_STATE", self.root.path().join("state"))
                .env("FIXTURE_RUNNING", "true")
                .env("FIXTURE_ENABLED", "true");
            command
        }

        fn assert_unchanged(&self, installed: bool) {
            assert!(!self.home.join(".codexify").exists());
            assert_eq!(
                fs::read_to_string(&self.config).unwrap(),
                "deliberately invalid configuration"
            );
            if installed {
                assert_eq!(
                    fs::read_to_string(&self.definition).unwrap(),
                    "fixture service definition"
                );
            } else {
                assert!(!self.definition.exists());
                assert!(!self.root.path().join("calls").exists());
            }
        }
    }

    #[test]
    fn service_status_absent_reports_state_without_loading_config_or_creating_files() {
        let fixture = Fixture::new(false);
        for json_output in [false, true] {
            let mut command = fixture.command();
            command.args(["service", "status"]);
            if json_output {
                command.arg("--json");
            }
            let output = command.output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(4),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stderr.is_empty());
            if json_output {
                let report: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(report["installed"], false);
                assert_eq!(report["running"], false);
                assert_eq!(report["enabled"], Value::Null);
                assert_eq!(report["definitionPath"], Value::Null);
            } else {
                let text = String::from_utf8(output.stdout).unwrap();
                assert!(text.contains("Codexify service: not installed"), "{text}");
                assert!(text.contains("Enabled: unknown"), "{text}");
            }
            fixture.assert_unchanged(false);
        }
    }

    #[test]
    fn service_status_json_reports_running_and_enabled_independently() {
        let fixture = Fixture::new(true);
        for (running, enabled, exit) in [
            (true, true, 0),
            (true, false, 0),
            (false, true, 3),
            (false, false, 3),
        ] {
            let output = fixture
                .command()
                .env("FIXTURE_RUNNING", running.to_string())
                .env("FIXTURE_ENABLED", enabled.to_string())
                .args(["service", "status", "--json", "--config"])
                .arg(&fixture.config)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(exit),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stderr.is_empty());
            let report: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(report.as_object().unwrap().len(), 5);
            assert_eq!(report["installed"], true);
            assert_eq!(report["running"], running);
            assert_eq!(report["enabled"], enabled);
            assert_eq!(report["definitionPath"], json!(fixture.definition));
            assert!(
                report["detail"]
                    .as_str()
                    .is_some_and(|detail| !detail.is_empty())
            );
            fixture.assert_unchanged(true);
        }
        let calls = fs::read_to_string(fixture.root.path().join("calls")).unwrap();
        assert_eq!(calls.lines().count(), 8);
    }

    #[test]
    fn service_status_human_output_does_not_claim_application_health() {
        let fixture = Fixture::new(true);
        for (running, state, exit) in [(true, "running", 0), (false, "stopped", 3)] {
            let output = fixture
                .command()
                .env("FIXTURE_RUNNING", running.to_string())
                .args(["service", "status"])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(exit));
            assert!(output.stderr.is_empty());
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(
                text.contains(&format!("Codexify service: {state}")),
                "{text}"
            );
            assert!(text.contains("Installed: yes"), "{text}");
            assert!(text.contains("Enabled: yes"), "{text}");
            assert!(
                text.contains(fixture.definition.to_str().unwrap()),
                "{text}"
            );
            assert!(text.contains("Details:"), "{text}");
            assert!(!text.contains("healthy"), "{text}");
            fixture.assert_unchanged(true);
        }
    }

    #[test]
    fn service_status_query_failure_is_not_reported_as_absent() {
        let fixture = Fixture::new(true);
        fixture.write_manager(true);
        let output = fixture
            .command()
            .args(["service", "status", "--json"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("fixture query failure"), "{error}");
        fixture.assert_unchanged(true);
    }

    #[test]
    fn service_start_stop_and_restart_preserve_enablement() {
        let expectations = [("start", "start"), ("stop", "stop"), ("restart", "restart")];
        for (operation, expected) in expectations {
            let fixture = Fixture::new(true);
            fs::write(
                fixture.root.path().join("state"),
                if operation == "start" {
                    "false"
                } else {
                    "true"
                },
            )
            .unwrap();
            let output = fixture
                .command()
                .env("NO_COLOR", "1")
                .args(["service", operation])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{operation}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8(output.stdout)
                    .unwrap()
                    .to_ascii_lowercase()
                    .contains(expected)
            );
            let calls = fs::read_to_string(fixture.root.path().join("calls")).unwrap();
            assert!(
                !calls.lines().any(|line| {
                    let line = line.to_ascii_lowercase();
                    line.contains(" enable") || line.contains(" disable")
                }),
                "{operation}: {calls}"
            );
            #[cfg(target_os = "linux")]
            assert!(
                calls.contains(&format!("--user {operation} codexify.service")),
                "{calls}"
            );
            #[cfg(target_os = "macos")]
            match operation {
                "start" | "restart" => assert!(
                    calls.contains("kickstart") || calls.contains("bootstrap"),
                    "{calls}"
                ),
                "stop" => assert!(calls.contains("bootout --wait"), "{operation}: {calls}"),
                _ => unreachable!(),
            }
            fixture.assert_unchanged(true);
        }
    }

    #[test]
    fn service_status_missing_manager_is_a_query_error() {
        let fixture = Fixture::new(true);
        let empty_path = fixture.root.path().join("empty-bin");
        fs::create_dir(&empty_path).unwrap();
        let output = fixture
            .command()
            .env("PATH", empty_path)
            .args(["service", "status", "--json"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
        fixture.assert_unchanged(true);
    }
}
