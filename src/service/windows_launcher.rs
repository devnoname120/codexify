//! Upgrade recognized PowerShell task launchers to the native launcher before
//! start/enable, including when an updater invokes the newly installed binary.

#[cfg(windows)]
use super::{
    ServiceSpec, WINDOWS_TASK, command_output, powershell_command, powershell_quote,
    windows_action_arguments,
};

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskAction {
    execute: String,
    #[serde(default)]
    arguments: String,
    #[serde(default)]
    working_directory: String,
}

#[derive(Debug)]
struct LegacyTask {
    action: TaskAction,
    executable: String,
    config: String,
}

fn powershell_host(path: &str) -> bool {
    let base = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let base = if base.len() >= 4 && base[base.len() - 4..].eq_ignore_ascii_case(".exe") {
        &base[..base.len() - 4]
    } else {
        base
    };
    base.eq_ignore_ascii_case("powershell") || base.eq_ignore_ascii_case("pwsh")
}

fn task_action(output: &str) -> anyhow::Result<Option<TaskAction>> {
    use anyhow::{Context, bail};

    if output.len() > 64 * 1024 {
        bail!("scheduled task launcher exceeds the command-line limit");
    }
    serde_json::from_str(output.trim()).context("parse the scheduled task launcher")
}

fn legacy_task(output: &str) -> anyhow::Result<Option<LegacyTask>> {
    let Some(action) = task_action(output)? else {
        return Ok(None);
    };
    if !powershell_host(&action.execute) {
        return Ok(None);
    }
    let Some((executable, config)) = legacy_paths(&action.arguments) else {
        return Ok(None);
    };
    Ok(Some(LegacyTask {
        action,
        executable,
        config,
    }))
}

fn legacy_paths(arguments: &str) -> Option<(String, String)> {
    legacy_paths_with_reader(arguments, |path| {
        let expected = super::service_root()
            .ok()?
            .join("service")
            .join("supervisor.ps1");
        if !same_file::is_same_file(path, expected).unwrap_or(false) {
            return None;
        }
        read_bounded_launcher(path)
    })
}

fn read_bounded_launcher(path: impl AsRef<std::path::Path>) -> Option<String> {
    use std::io::Read;

    const MAX_LAUNCHER_BYTES: u64 = 64 * 1024;
    let path = path.as_ref();
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_LAUNCHER_BYTES {
        return None;
    }
    let mut contents = String::with_capacity(metadata.len() as usize);
    std::fs::File::open(path)
        .ok()?
        .take(MAX_LAUNCHER_BYTES + 1)
        .read_to_string(&mut contents)
        .ok()?;
    (contents.len() as u64 <= MAX_LAUNCHER_BYTES).then_some(contents)
}

fn legacy_paths_with_reader(
    arguments: &str,
    read_launcher: impl Fn(&str) -> Option<String>,
) -> Option<(String, String)> {
    use base64::Engine;

    const ENCODED_PREFIX: &str = "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -EncodedCommand ";
    const FILE_PREFIX: &str =
        "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File ";

    let arguments = arguments.trim();
    if let Some(encoded) = arguments.strip_prefix(ENCODED_PREFIX) {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()?;
        if bytes.len() % 2 != 0 || bytes.len() > 32 * 1024 {
            return None;
        }
        let units = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| u16::from_le_bytes(*bytes))
            .collect::<Vec<_>>();
        return launcher_script_paths(&String::from_utf16(&units).ok()?);
    }

    let path = arguments.strip_prefix(FILE_PREFIX)?;
    let path = path.strip_prefix('"')?.strip_suffix('"')?;
    launcher_script_paths(&read_launcher(path)?)
}

fn launcher_script_paths(script: &str) -> Option<(String, String)> {
    let script = script
        .strip_prefix('\u{feff}')
        .unwrap_or(script)
        .replace("\r\n", "\n");
    if script.starts_with("# Codexify supervisor launcher v2\n") {
        let literal = |name: &str| {
            let pattern =
                regex::Regex::new(&format!(r"(?m)^\${name} = ('(?:[^'\r\n]|'')+')$")).ok()?;
            Some(pattern.captures(&script)?.get(1)?.as_str())
        };
        let executable = literal("serviceExecutable")?;
        let config = literal("serviceConfig")?;
        let log = literal("serviceLog")?;
        if script.trim_end() != super::windows_launcher_source(executable, config, log).trim_end() {
            return None;
        }
        return Some((
            executable[1..executable.len() - 1].replace("''", "'"),
            config[1..config.len() - 1].replace("''", "'"),
        ));
    }

    let invocation = script
        .trim_end()
        .strip_prefix("$ErrorActionPreference = 'Stop'\n")?
        .strip_suffix("\nif ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }")?;
    let pattern =
        regex::Regex::new(r"^& '((?:[^'\r\n]|'')+)' service run --config '((?:[^'\r\n]|'')+)'$")
            .ok()?;
    let paths = pattern.captures(invocation)?;
    Some((paths[1].replace("''", "'"), paths[2].replace("''", "'")))
}

#[cfg(windows)]
pub(super) fn refresh() -> anyhow::Result<()> {
    use anyhow::Context;

    let task_name = powershell_quote(WINDOWS_TASK);
    let query = format!(
        "$ErrorActionPreference='Stop'\n$ProgressPreference='SilentlyContinue'\n$task=Get-ScheduledTask -TaskName {task_name} -ErrorAction SilentlyContinue\nif (-not $task -or @($task.Actions).Count -ne 1) {{ 'null'; exit 0 }}\n$action=$task.Actions[0]\n[ordered]@{{ execute=[string]$action.Execute; arguments=[string]$action.Arguments; workingDirectory=[string]$action.WorkingDirectory }} | ConvertTo-Json -Compress\n"
    );
    let output = command_output(
        powershell_command(&query),
        "inspect the scheduled task launcher",
    )?;
    let Some(legacy) = legacy_task(&output)? else {
        return Ok(());
    };
    let current = std::env::current_exe().context("locate the service executable")?;
    if !same_file::is_same_file(&legacy.executable, &current).unwrap_or(false) {
        return Ok(());
    }

    let spec = ServiceSpec::resolve(std::path::Path::new(&legacy.config))?;
    let expected_execute = powershell_quote(&legacy.action.execute);
    let expected_arguments = powershell_quote(&legacy.action.arguments);
    let expected_working_directory = powershell_quote(&legacy.action.working_directory);
    let replacement_execute = powershell_quote(&spec.executable.to_string_lossy());
    let replacement_arguments = powershell_quote(&windows_action_arguments(&spec));
    let replacement_working_directory = powershell_quote(&spec.working_dir.to_string_lossy());
    let update = format!(
        "$ErrorActionPreference='Stop'\n$ProgressPreference='SilentlyContinue'\n$task=Get-ScheduledTask -TaskName {task_name} -ErrorAction Stop\nif (@($task.Actions).Count -ne 1) {{ throw 'The scheduled task changed while updating its launcher; retry the service command' }}\n$current=$task.Actions[0]\nif ([string]$current.Execute -cne {expected_execute} -or [string]$current.Arguments -cne {expected_arguments} -or [string]$current.WorkingDirectory -cne {expected_working_directory}) {{ throw 'The scheduled task changed while updating its launcher; retry the service command' }}\n$action=New-ScheduledTaskAction -Execute {replacement_execute} -Argument {replacement_arguments} -WorkingDirectory {replacement_working_directory}\nSet-ScheduledTask -TaskName {task_name} -Action $action | Out-Null\n"
    );
    command_output(
        powershell_command(&update),
        "update the scheduled task launcher",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded_arguments(script: &str) -> String {
        format!(
            "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -EncodedCommand {}",
            super::super::encode_powershell_script(script)
        )
    }

    fn file_arguments(path: &str) -> String {
        format!(
            "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File \"{path}\""
        )
    }

    #[test]
    fn legacy_task_paths_are_decoded_without_executing_powershell() {
        let executable = r"C:\Users\O'Brien\Program & Files\app.exe";
        let config = r"C:\Users\O'Brien\configuration.json";
        let script = format!(
            "$ErrorActionPreference = 'Stop'\n& {} service run --config {}\nif ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}\n",
            super::super::powershell_quote(executable),
            super::super::powershell_quote(config)
        );
        assert_eq!(
            legacy_paths_with_reader(&encoded_arguments(&script), |_| None),
            Some((executable.into(), config.into()))
        );
        assert_eq!(
            legacy_paths_with_reader(&encoded_arguments(&script.replace('\n', "\r\n")), |_| None),
            Some((executable.into(), config.into()))
        );
    }

    #[test]
    fn recognized_v2_file_launcher_upgrades_but_modified_scripts_are_preserved() {
        let executable = r"C:\Users\O'Brien\app.exe";
        let config = r"C:\Users\O'Brien\custom.json";
        let script = super::super::windows_launcher_source(
            &super::super::powershell_quote(executable),
            &super::super::powershell_quote(config),
            "'C:\\Users\\test\\service.log'",
        );
        let arguments = file_arguments(r"C:\Users\test\supervisor.ps1");
        assert_eq!(
            legacy_paths_with_reader(&arguments, |_| Some(format!("\u{feff}{script}"))),
            Some((executable.into(), config.into()))
        );
        assert!(
            legacy_paths_with_reader(&arguments, |_| {
                Some(format!("{script}\nWrite-Output 'custom'\n"))
            })
            .is_none()
        );
    }

    #[test]
    fn current_custom_and_malformed_launchers_are_not_rewritten() {
        for script in [
            "# Codexify supervisor launcher v2\n$serviceConfig='custom'",
            "$ErrorActionPreference = 'Stop'\n& 'app.exe' service run --config $env:PRIVATE_CONFIG\nif ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }\n",
            "$ErrorActionPreference = 'Stop'\n& 'app.exe' service run --config 'config'\nRemove-Item 'unrelated'\nif ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }\n",
        ] {
            assert!(legacy_paths_with_reader(&encoded_arguments(script), |_| None).is_none());
        }
        assert!(legacy_paths_with_reader("-EncodedCommand not-base64", |_| None).is_none());
        assert!(
            legacy_paths_with_reader(
                "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File unquoted.ps1",
                |_| Some("ignored".into())
            )
            .is_none()
        );
    }

    #[test]
    fn only_exact_powershell_actions_are_migrated() {
        let script = "$ErrorActionPreference = 'Stop'\n& 'app.exe' service run --config 'config.json'\nif ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }\n";
        let arguments = encoded_arguments(script);
        for execute in [
            "powershell.exe",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            "pwsh.exe",
        ] {
            let action = serde_json::json!({
                "execute": execute,
                "arguments": arguments,
                "workingDirectory": r"C:\work"
            });
            let legacy = legacy_task(&action.to_string()).unwrap().unwrap();
            assert_eq!(legacy.executable, "app.exe");
            assert_eq!(legacy.config, "config.json");
            assert_eq!(legacy.action.working_directory, r"C:\work");
        }
        let custom = serde_json::json!({
            "execute": "custom.exe",
            "arguments": arguments,
            "workingDirectory": r"C:\work"
        });
        assert!(legacy_task(&custom.to_string()).unwrap().is_none());
    }

    #[test]
    fn launcher_queries_are_bounded_and_nullable() {
        let action = serde_json::json!({
            "execute": "powershell.exe",
            "arguments": "x".repeat(3000),
            "workingDirectory": r"C:\work"
        });
        assert!(legacy_task(&action.to_string()).unwrap().is_none());
        assert!(legacy_task("null").unwrap().is_none());
        assert!(legacy_task("truncated-query").is_err());
        assert!(legacy_task(&format!("\"{}\"", "x".repeat(70 * 1024))).is_err());
    }

    #[test]
    fn file_launcher_reads_are_bounded() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("supervisor.ps1");
        std::fs::write(&path, "x".repeat(64 * 1024)).unwrap();
        assert_eq!(read_bounded_launcher(&path).unwrap().len(), 64 * 1024);
        std::fs::write(&path, "x".repeat(64 * 1024 + 1)).unwrap();
        assert!(read_bounded_launcher(&path).is_none());
    }
}
