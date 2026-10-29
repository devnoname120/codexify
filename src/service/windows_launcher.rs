//! Upgrade the known legacy task action before start/enable, including when an
//! updater from an older release invokes the newly installed executable.

#[cfg(windows)]
use super::{
    ServiceSpec, WINDOWS_TASK, command_output, powershell_command, powershell_quote,
    windows_action_arguments,
};

fn legacy_task(output: &str) -> anyhow::Result<Option<(String, String, String)>> {
    use anyhow::{Context, bail};
    if output.len() > 64 * 1024 {
        bail!("scheduled task launcher exceeds the command-line limit");
    }
    // Diagnostic text truncation must never be applied to structured output:
    // current encoded launchers legitimately exceed the 4 KiB diagnostic tail.
    let arguments: Option<String> =
        serde_json::from_str(output.trim()).context("parse the scheduled task launcher")?;
    let Some((executable, config)) = arguments.as_deref().and_then(legacy_paths) else {
        return Ok(None);
    };
    Ok(Some((arguments.unwrap(), executable, config)))
}

fn legacy_paths(arguments: &str) -> Option<(String, String)> {
    use base64::Engine;

    let encoded = arguments.trim().strip_prefix(
        "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -EncodedCommand ",
    )?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    if bytes.len() % 2 != 0 || bytes.len() > 32 * 1024 {
        return None;
    }
    let units: Vec<_> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes(*b))
        .collect();
    let script = String::from_utf16(&units).ok()?.replace("\r\n", "\n");
    if script.starts_with("# Codexify supervisor launcher v2\n") {
        let literal = |name: &str| {
            let pattern =
                regex::Regex::new(&format!(r"(?m)^\${name} = ('(?:[^'\r\n]|'')+')$")).ok()?;
            let value = pattern.captures(&script)?.get(1)?.as_str();
            Some(value)
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
            .unwrap();
    let paths = pattern.captures(invocation)?;
    Some((paths[1].replace("''", "'"), paths[2].replace("''", "'")))
}

#[cfg(windows)]
pub(super) fn refresh() -> anyhow::Result<()> {
    use anyhow::Context;

    let task_name = powershell_quote(WINDOWS_TASK);
    let query = format!(
        "$ErrorActionPreference='Stop'\n$ProgressPreference='SilentlyContinue'\n$task=Get-ScheduledTask -TaskName {task_name} -ErrorAction SilentlyContinue\nif (-not $task -or @($task.Actions).Count -ne 1) {{ 'null'; exit 0 }}\nConvertTo-Json -InputObject $task.Actions[0].Arguments -Compress\n"
    );
    let output = command_output(
        powershell_command(&query),
        "inspect the scheduled task launcher",
    )?;
    let Some((arguments, executable, config)) = legacy_task(&output)? else {
        return Ok(()); // Current launchers and custom task actions are preserved.
    };
    let current = std::env::current_exe().context("locate the service executable")?;
    if !same_file::is_same_file(&executable, &current).unwrap_or(false) {
        return Ok(());
    }
    let spec = ServiceSpec::resolve(std::path::Path::new(&config))?;
    super::write_windows_launcher(&spec)?;
    let expected = powershell_quote(&arguments);
    let replacement = powershell_quote(&windows_action_arguments(&spec));
    let update = format!(
        "$ErrorActionPreference='Stop'\n$ProgressPreference='SilentlyContinue'\n$task=Get-ScheduledTask -TaskName {task_name} -ErrorAction Stop\nif (@($task.Actions).Count -ne 1 -or $task.Actions[0].Arguments -cne {expected}) {{ throw 'The scheduled task changed while updating its launcher; retry the service command' }}\n$task.Actions[0].Arguments={replacement}\nSet-ScheduledTask -TaskName {task_name} -Action $task.Actions | Out-Null\n"
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

    fn arguments(script: &str) -> String {
        format!(
            "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -EncodedCommand {}",
            super::super::encode_powershell_script(script)
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
            legacy_paths(&arguments(&script)),
            Some((executable.into(), config.into()))
        );
        assert_eq!(
            legacy_paths(&arguments(&script.replace('\n', "\r\n"))),
            Some((executable.into(), config.into()))
        );
    }

    #[test]
    fn current_custom_and_malformed_launchers_are_not_rewritten() {
        for script in [
            "# Codexify supervisor launcher v2\n$serviceConfig='custom'",
            "$ErrorActionPreference = 'Stop'\n& 'app.exe' service run --config $env:PRIVATE_CONFIG\nif ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }\n",
            "$ErrorActionPreference = 'Stop'\n& 'app.exe' service run --config 'config'\nRemove-Item 'unrelated'\nif ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }\n",
        ] {
            assert!(legacy_paths(&arguments(script)).is_none());
        }
        assert!(legacy_paths("-EncodedCommand not-base64").is_none());
    }

    #[test]
    fn recognized_v2_payloads_upgrade_but_modified_scripts_are_preserved() {
        let executable = r"C:\Users\O'Brien\app.exe";
        let config = r"C:\Users\O'Brien\custom.json";
        let script = super::super::windows_launcher_source(
            &super::super::powershell_quote(executable),
            &super::super::powershell_quote(config),
            "'C:\\Users\\test\\service.log'",
        );
        assert_eq!(
            legacy_paths(&arguments(&script)),
            Some((executable.into(), config.into()))
        );
        assert!(legacy_paths(&arguments(&(script + "\nWrite-Output 'custom'\n"))).is_none());
        assert!(
            legacy_paths("-NoLogo -NoProfile -File \"C:\\Users\\test\\supervisor.ps1\"").is_none()
        );
    }

    #[test]
    fn current_launcher_queries_larger_than_the_diagnostic_tail_are_preserved() {
        let script = format!("# Codexify supervisor launcher v2\n{}", "x".repeat(3000));
        let query = serde_json::to_string(&arguments(&script)).unwrap();
        assert!(query.len() > 4096);
        assert!(legacy_task(&query).unwrap().is_none());
        assert!(legacy_task("null").unwrap().is_none());
        assert!(legacy_task("truncated-query").is_err());
    }
}
