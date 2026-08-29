use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::{SecondsFormat, Utc};
use tokio::io::AsyncReadExt;
use tokio::process::Command as TokioCommand;
use tokio::time::Instant;

use crate::util::home_dir;

#[cfg(any(target_os = "macos", test))]
const SERVICE_LABEL: &str = "dev.codexify.service";
#[cfg(target_os = "linux")]
const SYSTEMD_UNIT: &str = "codexify.service";
#[cfg(any(target_os = "windows", test))]
const WINDOWS_TASK: &str = "Codexify";
const LOG_FILE: &str = "codexify.log";
const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
const LOG_GENERATIONS: usize = 5;
const LOG_TAIL_BYTES: u64 = 256 * 1024;
const LOG_TAIL_LINES: usize = 200;
const DRAIN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const RESTART_MIN_DELAY: Duration = Duration::from_secs(2);
const RESTART_MAX_DELAY: Duration = Duration::from_secs(60);
const STABLE_RUNTIME: Duration = Duration::from_secs(60);
const CONFIG_RETRY_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
struct SupervisorPolicy {
    restart_min_delay: Duration,
    restart_max_delay: Duration,
    stable_runtime: Duration,
    config_retry_delay: Duration,
}

const SUPERVISOR_POLICY: SupervisorPolicy = SupervisorPolicy {
    restart_min_delay: RESTART_MIN_DELAY,
    restart_max_delay: RESTART_MAX_DELAY,
    stable_runtime: STABLE_RUNTIME,
    config_retry_delay: CONFIG_RETRY_DELAY,
};

#[derive(Debug, Clone)]
struct ServiceSpec {
    executable: PathBuf,
    config: PathBuf,
    working_dir: PathBuf,
    home: PathBuf,
    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    path: String,
}

impl ServiceSpec {
    fn resolve(config: &Path) -> anyhow::Result<Self> {
        if !config.is_absolute() {
            bail!("service config path must be absolute: {}", config.display());
        }
        let config = normalize_native_path(config.to_path_buf());
        validate_native_path(&config)?;
        match fs::metadata(&config) {
            Ok(metadata) if !metadata.is_file() => {
                bail!("service config path is not a file: {}", config.display())
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect service config path {}", config.display()));
            }
        }

        let executable = std::env::current_exe()
            .context("locate the running codexify executable")?
            .canonicalize()
            .context("canonicalize the running codexify executable")?;
        let executable = normalize_native_path(executable);
        validate_native_path(&executable)?;

        let home = home_dir().context("locate the user's home directory")?;
        let home = absolutize(&home)?;
        validate_native_path(&home)?;

        let working_dir = config
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.clone());
        fs::create_dir_all(&working_dir).with_context(|| {
            format!(
                "create service configuration directory {}",
                working_dir.display()
            )
        })?;

        #[cfg(any(target_os = "linux", target_os = "macos", test))]
        let path = service_path(&executable)?;
        Ok(Self {
            executable,
            config,
            working_dir,
            home,
            #[cfg(any(target_os = "linux", target_os = "macos", test))]
            path,
        })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn service_path(executable: &Path) -> anyhow::Result<String> {
    let mut entries = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    entries.retain(|entry| entry.is_absolute());
    let mut unique = Vec::with_capacity(entries.len());
    for entry in entries {
        if !unique.contains(&entry) {
            unique.push(entry);
        }
    }
    let mut entries = unique;
    if let Some(parent) = executable.parent()
        && !entries.iter().any(|entry| entry == parent)
    {
        entries.insert(0, parent.to_path_buf());
    }
    if entries.is_empty() {
        entries.extend(std::env::split_paths(std::ffi::OsStr::new(default_path())));
    }
    let joined = std::env::join_paths(entries).context("construct the service PATH")?;
    let value = joined
        .into_string()
        .map_err(|_| anyhow::anyhow!("service PATH must be valid UTF-8"))?;
    validate_service_value("PATH", &value)?;
    Ok(value)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn default_path() -> &'static str {
    #[cfg(windows)]
    {
        r"C:\Windows\System32;C:\Windows"
    }
    #[cfg(not(windows))]
    {
        "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    }
}

fn absolutize(path: &Path) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .context("determine the current directory")?
            .join(path))
    }
}

fn validate_native_path(path: &Path) -> anyhow::Result<()> {
    let value = path
        .to_str()
        .with_context(|| format!("service paths must be valid UTF-8: {}", path.display()))?;
    validate_service_value("service path", value)
        .with_context(|| format!("invalid service path {}", path.display()))
}

fn validate_service_value(name: &str, value: &str) -> anyhow::Result<()> {
    if value.contains(['\0', '\n', '\r']) {
        bail!("{name} cannot contain NUL or newline characters");
    }
    Ok(())
}

fn normalize_native_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::path::{Component, Prefix};

        let mut components = path.components();
        if let Some(Component::Prefix(prefix)) = components.next() {
            match prefix.kind() {
                Prefix::VerbatimDisk(drive) => {
                    let mut rebuilt = OsString::from(format!("{}:", drive as char));
                    rebuilt.push(components.as_path().as_os_str());
                    return PathBuf::from(rebuilt);
                }
                Prefix::VerbatimUNC(server, share) => {
                    let mut rebuilt = OsString::from(r"\\");
                    rebuilt.push(server);
                    rebuilt.push(r"\");
                    rebuilt.push(share);
                    rebuilt.push(components.as_path().as_os_str());
                    return PathBuf::from(rebuilt);
                }
                _ => {}
            }
        }
    }
    path
}

fn service_root() -> anyhow::Result<PathBuf> {
    let home = home_dir().context("locate the user's home directory")?;
    Ok(absolutize(&home)?.join(".codexify"))
}

pub fn log_path() -> anyhow::Result<PathBuf> {
    Ok(service_root()?.join("logs").join(LOG_FILE))
}

pub fn install(config: &Path) -> anyhow::Result<()> {
    let spec = ServiceSpec::resolve(config)?;
    let config_exists = spec.config.is_file();
    ensure_log_directory(&spec.home)?;
    platform_install(&spec)?;
    println!(
        "Installed and enabled the Codexify service with config {}",
        spec.config.display()
    );
    println!("Service logs: {}", log_path()?.display());
    if !config_exists {
        println!(
            "The service is waiting for that config file; run `codexify quickstart` or create it with an absolute workDir."
        );
    }
    Ok(())
}

pub fn enable() -> anyhow::Result<()> {
    platform_enable()?;
    println!("Enabled and started the Codexify service");
    Ok(())
}

pub fn disable() -> anyhow::Result<()> {
    platform_disable()?;
    println!("Stopped and disabled the Codexify service");
    Ok(())
}

pub fn remove() -> anyhow::Result<()> {
    platform_remove()?;
    println!("Removed the Codexify service");
    Ok(())
}

pub fn is_installed() -> anyhow::Result<bool> {
    platform_is_installed()
}

fn ensure_log_directory(home: &Path) -> anyhow::Result<PathBuf> {
    let directory = home.join(".codexify").join("logs");
    fs::create_dir_all(&directory)
        .with_context(|| format!("create service log directory {}", directory.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory)
}

fn command_output(mut command: StdCommand, action: &str) -> anyhow::Result<String> {
    let output = command.output().with_context(|| {
        format!(
            "{action}: start {}",
            command.get_program().to_string_lossy()
        )
    })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }

    let stderr = bounded_command_text(&output.stderr);
    let stdout = bounded_command_text(&output.stdout);
    let detail = if !stderr.is_empty() { stderr } else { stdout };
    bail!("{action} failed with {}: {detail}", output.status)
}

fn bounded_command_text(bytes: &[u8]) -> String {
    const MAX: usize = 4096;
    let bytes = if bytes.len() > MAX {
        &bytes[bytes.len() - MAX..]
    } else {
        bytes
    };
    String::from_utf8_lossy(bytes).trim().to_string()
}

fn best_effort(mut command: StdCommand) {
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let _ = command.status();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_definition(path: &Path, contents: &str, mode: u32) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .context("service definition has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create service definition directory {}", parent.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "create temporary service definition in {}",
            parent.display()
        )
    })?;
    temp.write_all(contents.as_bytes())?;
    temp.as_file().sync_all()?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(mode))?;

    match temp.persist(path) {
        Ok(_) => Ok(()),
        Err(error) => {
            #[cfg(windows)]
            {
                let temp = error.file;
                if path.exists() {
                    fs::remove_file(path)?;
                    return temp
                        .persist(path)
                        .map(|_| ())
                        .map_err(|error| error.error.into());
                }
                Err(error.error.into())
            }
            #[cfg(not(windows))]
            {
                Err(error.error.into())
            }
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn systemd_escape(value: &str) -> String {
    let escaped = value
        .replace('%', "%%")
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(any(target_os = "linux", test))]
fn systemd_exec_escape(value: &str) -> String {
    systemd_escape(&value.replace('$', "$$"))
}

#[cfg(any(target_os = "linux", test))]
fn systemd_unit(spec: &ServiceSpec) -> String {
    let executable = systemd_exec_escape(&spec.executable.to_string_lossy());
    let config = systemd_exec_escape(&spec.config.to_string_lossy());
    let working_dir = systemd_escape(&spec.working_dir.to_string_lossy());
    let home = systemd_escape(&format!("HOME={}", spec.home.display()));
    let path = systemd_escape(&format!("PATH={}", spec.path));
    format!(
        "[Unit]\nDescription=Codexify MCP bridge\nStartLimitIntervalSec=0\n\n[Service]\nType=simple\nExecStart={executable} service run --config {config}\nWorkingDirectory={working_dir}\nEnvironment={home}\nEnvironment={path}\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec=15\nKillMode=control-group\n\n[Install]\nWantedBy=default.target\n"
    )
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(any(target_os = "macos", test))]
fn launchd_plist(spec: &ServiceSpec) -> String {
    let executable = xml_escape(&spec.executable.to_string_lossy());
    let config = xml_escape(&spec.config.to_string_lossy());
    let working_dir = xml_escape(&spec.working_dir.to_string_lossy());
    let home = xml_escape(&spec.home.to_string_lossy());
    let path = xml_escape(&spec.path);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{SERVICE_LABEL}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{executable}</string>\n    <string>service</string>\n    <string>run</string>\n    <string>--config</string>\n    <string>{config}</string>\n  </array>\n  <key>WorkingDirectory</key>\n  <string>{working_dir}</string>\n  <key>EnvironmentVariables</key>\n  <dict>\n    <key>HOME</key>\n    <string>{home}</string>\n    <key>PATH</key>\n    <string>{path}</string>\n  </dict>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n  <key>ThrottleInterval</key>\n  <integer>5</integer>\n  <key>ExitTimeOut</key>\n  <integer>15</integer>\n  <key>AbandonProcessGroup</key>\n  <false/>\n  <key>ProcessType</key>\n  <string>Background</string>\n</dict>\n</plist>\n"
    )
}

#[cfg(any(target_os = "windows", test))]
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(any(target_os = "windows", test))]
fn windows_action_arguments(config: &Path) -> String {
    format!("service run --config \"{}\"", config.display())
}

#[cfg(any(target_os = "windows", test))]
fn windows_install_script(spec: &ServiceSpec) -> String {
    let executable = powershell_quote(&spec.executable.to_string_lossy());
    let arguments = powershell_quote(&windows_action_arguments(&spec.config));
    let working_dir = powershell_quote(&spec.working_dir.to_string_lossy());
    let task = powershell_quote(WINDOWS_TASK);
    format!(
        "$ErrorActionPreference = 'Stop'\n$taskName = {task}\n$existing = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue\nif ($existing) {{ Stop-ScheduledTask -InputObject $existing -ErrorAction SilentlyContinue }}\n$identity = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name\n$action = New-ScheduledTaskAction -Execute {executable} -Argument {arguments} -WorkingDirectory {working_dir}\n$trigger = New-ScheduledTaskTrigger -AtLogOn -User $identity\n$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew\n$principal = New-ScheduledTaskPrincipal -UserId $identity -LogonType Interactive -RunLevel Limited\nRegister-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Force | Out-Null\nEnable-ScheduledTask -TaskName $taskName | Out-Null\nStart-ScheduledTask -TaskName $taskName\n"
    )
}

#[cfg(target_os = "linux")]
fn systemd_path(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
        .join("systemd")
        .join("user")
        .join(SYSTEMD_UNIT)
}

#[cfg(target_os = "macos")]
fn launchd_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{SERVICE_LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn launchd_domain() -> String {
    format!("gui/{}", unsafe { libc::geteuid() })
}

#[cfg(target_os = "macos")]
fn launchd_target() -> String {
    format!("{}/{}", launchd_domain(), SERVICE_LABEL)
}

#[cfg(target_os = "windows")]
fn powershell_command(script: &str) -> StdCommand {
    use base64::Engine;

    let mut bytes = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let program = if which_program("powershell.exe") {
        "powershell.exe"
    } else {
        "pwsh.exe"
    };
    let mut command = StdCommand::new(program);
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-EncodedCommand",
        &encoded,
    ]);
    command
}

#[cfg(target_os = "windows")]
fn which_program(program: &str) -> bool {
    StdCommand::new("where.exe")
        .arg(program)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "linux")]
fn platform_install(spec: &ServiceSpec) -> anyhow::Result<()> {
    let unit_path = systemd_path(&spec.home);
    write_definition(&unit_path, &systemd_unit(spec), 0o644)?;
    command_output(
        {
            let mut command = StdCommand::new("systemctl");
            command.args(["--user", "daemon-reload"]);
            command
        },
        "reload the user systemd manager",
    )?;
    command_output(
        {
            let mut command = StdCommand::new("systemctl");
            command.args(["--user", "enable", SYSTEMD_UNIT]);
            command
        },
        "enable the Codexify systemd unit",
    )?;
    command_output(
        {
            let mut command = StdCommand::new("systemctl");
            command.args(["--user", "restart", SYSTEMD_UNIT]);
            command
        },
        "start the Codexify systemd unit",
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn platform_enable() -> anyhow::Result<()> {
    let home = service_root()?
        .parent()
        .context("service root has no home directory")?
        .to_path_buf();
    let unit_path = systemd_path(&home);
    if !unit_path.exists() {
        bail!("Codexify service is not installed; run `codexify service install`");
    }
    command_output(
        {
            let mut command = StdCommand::new("systemctl");
            command.args(["--user", "enable", "--now", SYSTEMD_UNIT]);
            command
        },
        "enable the Codexify systemd unit",
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn platform_disable() -> anyhow::Result<()> {
    let home = service_root()?
        .parent()
        .context("service root has no home directory")?
        .to_path_buf();
    let unit_path = systemd_path(&home);
    if !unit_path.exists() {
        bail!("Codexify service is not installed");
    }
    command_output(
        {
            let mut command = StdCommand::new("systemctl");
            command.args(["--user", "disable", "--now", SYSTEMD_UNIT]);
            command
        },
        "disable the Codexify systemd unit",
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn platform_remove() -> anyhow::Result<()> {
    let home = service_root()?
        .parent()
        .context("service root has no home directory")?
        .to_path_buf();
    let unit_path = systemd_path(&home);
    if unit_path.exists() {
        command_output(
            {
                let mut command = StdCommand::new("systemctl");
                command.args(["--user", "disable", "--now", SYSTEMD_UNIT]);
                command
            },
            "stop and disable the Codexify systemd unit",
        )?;
        fs::remove_file(&unit_path)
            .with_context(|| format!("remove systemd unit {}", unit_path.display()))?;
    } else {
        let mut stop = StdCommand::new("systemctl");
        stop.args(["--user", "stop", SYSTEMD_UNIT]);
        best_effort(stop);
    }
    command_output(
        {
            let mut command = StdCommand::new("systemctl");
            command.args(["--user", "daemon-reload"]);
            command
        },
        "reload the user systemd manager",
    )?;
    let mut reset = StdCommand::new("systemctl");
    reset.args(["--user", "reset-failed", SYSTEMD_UNIT]);
    best_effort(reset);
    Ok(())
}

#[cfg(target_os = "linux")]
fn platform_is_installed() -> anyhow::Result<bool> {
    let home = service_root()?
        .parent()
        .context("service root has no home directory")?
        .to_path_buf();
    Ok(systemd_path(&home).exists())
}

#[cfg(target_os = "macos")]
fn launchd_is_loaded(target: &str) -> anyhow::Result<bool> {
    let status = StdCommand::new("launchctl")
        .args(["print", target])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("query the Codexify launch agent")?;
    Ok(status.success())
}

#[cfg(target_os = "macos")]
fn launchd_bootout_if_loaded(target: &str) -> anyhow::Result<()> {
    if !launchd_is_loaded(target)? {
        return Ok(());
    }
    command_output(
        {
            let mut command = StdCommand::new("launchctl");
            command.args(["bootout", target]);
            command
        },
        "stop the Codexify launch agent",
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_install(spec: &ServiceSpec) -> anyhow::Result<()> {
    let plist_path = launchd_path(&spec.home);
    let target = launchd_target();
    launchd_bootout_if_loaded(&target)?;
    write_definition(&plist_path, &launchd_plist(spec), 0o600)?;

    command_output(
        {
            let mut command = StdCommand::new("launchctl");
            command.args(["enable", &target]);
            command
        },
        "enable the Codexify launch agent",
    )?;
    command_output(
        {
            let mut command = StdCommand::new("launchctl");
            command
                .arg("bootstrap")
                .arg(launchd_domain())
                .arg(&plist_path);
            command
        },
        "load the Codexify launch agent",
    )?;
    command_output(
        {
            let mut command = StdCommand::new("launchctl");
            command.args(["kickstart", "-k", &target]);
            command
        },
        "start the Codexify launch agent",
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_enable() -> anyhow::Result<()> {
    let home = service_root()?
        .parent()
        .context("service root has no home directory")?
        .to_path_buf();
    let plist_path = launchd_path(&home);
    if !plist_path.exists() {
        bail!("Codexify service is not installed; run `codexify service install`");
    }
    let target = launchd_target();
    command_output(
        {
            let mut command = StdCommand::new("launchctl");
            command.args(["enable", &target]);
            command
        },
        "enable the Codexify launch agent",
    )?;

    if !launchd_is_loaded(&target)? {
        command_output(
            {
                let mut command = StdCommand::new("launchctl");
                command
                    .arg("bootstrap")
                    .arg(launchd_domain())
                    .arg(&plist_path);
                command
            },
            "load the Codexify launch agent",
        )?;
    }
    command_output(
        {
            let mut command = StdCommand::new("launchctl");
            command.args(["kickstart", "-k", &target]);
            command
        },
        "start the Codexify launch agent",
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_disable() -> anyhow::Result<()> {
    let home = service_root()?
        .parent()
        .context("service root has no home directory")?
        .to_path_buf();
    let plist_path = launchd_path(&home);
    if !plist_path.exists() {
        bail!("Codexify service is not installed");
    }
    let target = launchd_target();
    command_output(
        {
            let mut command = StdCommand::new("launchctl");
            command.args(["disable", &target]);
            command
        },
        "disable the Codexify launch agent",
    )?;
    launchd_bootout_if_loaded(&target)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_remove() -> anyhow::Result<()> {
    let home = service_root()?
        .parent()
        .context("service root has no home directory")?
        .to_path_buf();
    let plist_path = launchd_path(&home);
    let target = launchd_target();
    launchd_bootout_if_loaded(&target)?;
    let mut enable = StdCommand::new("launchctl");
    enable.args(["enable", &target]);
    best_effort(enable);
    if plist_path.exists() {
        fs::remove_file(&plist_path)
            .with_context(|| format!("remove launch agent {}", plist_path.display()))?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_is_installed() -> anyhow::Result<bool> {
    let home = service_root()?
        .parent()
        .context("service root has no home directory")?
        .to_path_buf();
    Ok(launchd_path(&home).exists())
}

#[cfg(target_os = "windows")]
fn platform_install(spec: &ServiceSpec) -> anyhow::Result<()> {
    command_output(
        powershell_command(&windows_install_script(spec)),
        "install the Codexify scheduled task",
    )?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn platform_enable() -> anyhow::Result<()> {
    let task = powershell_quote(WINDOWS_TASK);
    let script = format!(
        "$ErrorActionPreference = 'Stop'\n$task = Get-ScheduledTask -TaskName {task} -ErrorAction Stop\nEnable-ScheduledTask -InputObject $task | Out-Null\nStart-ScheduledTask -InputObject $task\n"
    );
    command_output(
        powershell_command(&script),
        "enable the Codexify scheduled task",
    )?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn platform_disable() -> anyhow::Result<()> {
    let task = powershell_quote(WINDOWS_TASK);
    let script = format!(
        "$ErrorActionPreference = 'Stop'\n$task = Get-ScheduledTask -TaskName {task} -ErrorAction Stop\nStop-ScheduledTask -InputObject $task -ErrorAction SilentlyContinue\nDisable-ScheduledTask -InputObject $task | Out-Null\n"
    );
    command_output(
        powershell_command(&script),
        "disable the Codexify scheduled task",
    )?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn platform_remove() -> anyhow::Result<()> {
    let task = powershell_quote(WINDOWS_TASK);
    let script = format!(
        "$ErrorActionPreference = 'Stop'\n$task = Get-ScheduledTask -TaskName {task} -ErrorAction SilentlyContinue\nif ($task) {{\n  Stop-ScheduledTask -InputObject $task -ErrorAction SilentlyContinue\n  Unregister-ScheduledTask -InputObject $task -Confirm:$false\n}}\n"
    );
    command_output(
        powershell_command(&script),
        "remove the Codexify scheduled task",
    )?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn platform_is_installed() -> anyhow::Result<bool> {
    let task = powershell_quote(WINDOWS_TASK);
    let script = format!(
        "$task = Get-ScheduledTask -TaskName {task} -ErrorAction SilentlyContinue\nif ($task) {{ exit 0 }} else {{ exit 3 }}\n"
    );
    let status = powershell_command(&script)
        .status()
        .context("query the Codexify scheduled task")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(3) => Ok(false),
        _ => bail!("query the Codexify scheduled task failed with {status}"),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn platform_install(_spec: &ServiceSpec) -> anyhow::Result<()> {
    bail!("Codexify services are supported on Linux, macOS, and Windows")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn platform_enable() -> anyhow::Result<()> {
    bail!("Codexify services are supported on Linux, macOS, and Windows")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn platform_disable() -> anyhow::Result<()> {
    bail!("Codexify services are supported on Linux, macOS, and Windows")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn platform_remove() -> anyhow::Result<()> {
    bail!("Codexify services are supported on Linux, macOS, and Windows")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn platform_is_installed() -> anyhow::Result<bool> {
    Ok(false)
}

struct RotatingLog {
    path: PathBuf,
    file: Option<File>,
    length: u64,
    max_bytes: u64,
    generations: usize,
}

impl RotatingLog {
    fn open(path: PathBuf, max_bytes: u64, generations: usize) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
        let mut log = Self {
            path,
            file: None,
            length: 0,
            max_bytes,
            generations,
        };
        if fs::metadata(&log.path).is_ok_and(|metadata| metadata.len() >= max_bytes) {
            log.rotate_files()?;
        }
        log.reopen()?;
        Ok(log)
    }

    fn reopen(&mut self) -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.create(true).append(true).read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        }
        self.length = file.metadata()?.len();
        self.file = Some(file);
        Ok(())
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.length > 0 && self.length.saturating_add(bytes.len() as u64) > self.max_bytes {
            self.rotate_files()?;
            self.reopen()?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("service log is not open"))?;
        file.write_all(bytes)?;
        file.flush()?;
        self.length = self.length.saturating_add(bytes.len() as u64);
        Ok(())
    }

    fn event(&mut self, message: &str) -> io::Result<()> {
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        self.append(format!("\n[{timestamp}] [service] {message}\n").as_bytes())
    }

    fn rotate_files(&mut self) -> io::Result<()> {
        self.file.take();
        if self.generations == 0 {
            let _ = fs::remove_file(&self.path);
            self.length = 0;
            return Ok(());
        }

        let oldest = rotated_path(&self.path, self.generations);
        if oldest.exists() {
            fs::remove_file(oldest)?;
        }
        for generation in (1..self.generations).rev() {
            let from = rotated_path(&self.path, generation);
            if from.exists() {
                fs::rename(from, rotated_path(&self.path, generation + 1))?;
            }
        }
        if self.path.exists() {
            fs::rename(&self.path, rotated_path(&self.path, 1))?;
        }
        self.length = 0;
        Ok(())
    }
}

fn rotated_path(path: &Path, generation: usize) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(format!(".{generation}"));
    PathBuf::from(value)
}

fn append_event(log: &Arc<StdMutex<RotatingLog>>, message: &str) {
    if let Ok(mut log) = log.lock() {
        let _ = log.event(message);
    }
}

async fn drain_to_log<R>(mut reader: R, log: Arc<StdMutex<RotatingLog>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(length) => {
                let Ok(mut log) = log.lock() else {
                    break;
                };
                if log.append(&buffer[..length]).is_err() {
                    break;
                }
            }
        }
    }
}

async fn await_drain(task: Option<tokio::task::JoinHandle<()>>) {
    let Some(mut task) = task else {
        return;
    };
    if tokio::time::timeout(DRAIN_SHUTDOWN_TIMEOUT, &mut task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: i32) {
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

#[cfg(windows)]
fn taskkill(pid: u32, force: bool) {
    let mut command = StdCommand::new("taskkill.exe");
    command.args(["/T", "/PID", &pid.to_string()]);
    if force {
        command.arg("/F");
    }
    best_effort(command);
}

#[cfg(windows)]
struct WindowsChildJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl WindowsChildJob {
    fn new() -> io::Result<Self> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            let error = io::Error::last_os_error();
            unsafe {
                CloseHandle(handle);
            }
            return Err(error);
        }
        Ok(Self(handle))
    }

    fn assign(&self, child: &tokio::process::Child) -> io::Result<()> {
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        let process = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("child process handle is unavailable"))?
            as HANDLE;
        if unsafe { AssignProcessToJobObject(self.0, process) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for WindowsChildJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

async fn terminate_child(
    pid: Option<u32>,
    wait: &mut (impl std::future::Future<Output = io::Result<std::process::ExitStatus>> + Unpin),
) {
    if let Some(pid) = pid {
        #[cfg(unix)]
        signal_process_group(pid, libc::SIGTERM);
        #[cfg(windows)]
        taskkill(pid, false);
    }

    if tokio::time::timeout(Duration::from_secs(10), &mut *wait)
        .await
        .is_err()
    {
        if let Some(pid) = pid {
            #[cfg(unix)]
            signal_process_group(pid, libc::SIGKILL);
            #[cfg(windows)]
            taskkill(pid, true);
        }
        let _ = wait.await;
    }
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut interrupt = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
}

#[cfg(windows)]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(not(any(unix, windows)))]
async fn shutdown_signal() {
    std::future::pending::<()>().await;
}

pub async fn run_supervisor(config: PathBuf) -> anyhow::Result<()> {
    if !config.is_absolute() {
        bail!("service supervisor requires an absolute --config path");
    }
    let config = normalize_native_path(config);
    validate_native_path(&config)?;
    let executable = std::env::current_exe()
        .context("locate the running codexify executable")?
        .canonicalize()
        .context("canonicalize the running codexify executable")?;
    let executable = normalize_native_path(executable);
    let log = Arc::new(StdMutex::new(RotatingLog::open(
        log_path()?,
        LOG_MAX_BYTES,
        LOG_GENERATIONS,
    )?));

    supervise(
        config,
        executable,
        log,
        SUPERVISOR_POLICY,
        shutdown_signal(),
    )
    .await
}

async fn supervise<F>(
    config: PathBuf,
    executable: PathBuf,
    log: Arc<StdMutex<RotatingLog>>,
    policy: SupervisorPolicy,
    shutdown: F,
) -> anyhow::Result<()>
where
    F: std::future::Future<Output = ()>,
{
    append_event(
        &log,
        &format!(
            "supervisor started with executable {} and config {}",
            executable.display(),
            config.display()
        ),
    );

    tokio::pin!(shutdown);
    let mut restart_delay = policy.restart_min_delay;
    let mut missing_reported = false;

    loop {
        if !config.is_file() {
            if !missing_reported {
                append_event(
                    &log,
                    &format!(
                        "config {} is not available; waiting for quickstart or manual configuration",
                        config.display()
                    ),
                );
                missing_reported = true;
            }
            tokio::select! {
                _ = &mut shutdown => {
                    append_event(&log, "supervisor stopped");
                    return Ok(());
                }
                _ = tokio::time::sleep(policy.config_retry_delay) => {}
            }
            continue;
        }
        missing_reported = false;

        let mut command = TokioCommand::new(&executable);
        command
            .arg("--config")
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);

        let started = Instant::now();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                append_event(&log, &format!("failed to start Codexify: {error}"));
                tokio::select! {
                    _ = &mut shutdown => {
                        append_event(&log, "supervisor stopped");
                        return Ok(());
                    }
                    _ = tokio::time::sleep(restart_delay) => {}
                }
                restart_delay = (restart_delay * 2).min(policy.restart_max_delay);
                continue;
            }
        };

        #[cfg(windows)]
        let mut child_job = Some(
            match WindowsChildJob::new().and_then(|job| {
                job.assign(&child)?;
                Ok(job)
            }) {
                Ok(job) => job,
                Err(error) => {
                    append_event(
                        &log,
                        &format!("failed to contain the Codexify process tree: {error}"),
                    );
                    if let Some(pid) = child.id() {
                        taskkill(pid, true);
                    }
                    let _ = child.wait().await;
                    tokio::select! {
                        _ = &mut shutdown => {
                            append_event(&log, "supervisor stopped");
                            return Ok(());
                        }
                        _ = tokio::time::sleep(restart_delay) => {}
                    }
                    restart_delay = (restart_delay * 2).min(policy.restart_max_delay);
                    continue;
                }
            },
        );

        let pid = child.id();
        append_event(
            &log,
            &format!(
                "started Codexify{}",
                pid.map(|pid| format!(" with pid {pid}"))
                    .unwrap_or_default()
            ),
        );
        let stdout_task = child
            .stdout
            .take()
            .map(|stdout| tokio::spawn(drain_to_log(stdout, log.clone())));
        let stderr_task = child
            .stderr
            .take()
            .map(|stderr| tokio::spawn(drain_to_log(stderr, log.clone())));
        let wait = child.wait();
        tokio::pin!(wait);

        let status = tokio::select! {
            status = &mut wait => Some(status),
            _ = &mut shutdown => {
                append_event(&log, "stopping Codexify after service shutdown request");
                terminate_child(pid, &mut wait).await;
                None
            }
        };

        #[cfg(unix)]
        if let Some(pid) = pid {
            signal_process_group(pid, libc::SIGKILL);
        }
        #[cfg(windows)]
        drop(child_job.take());

        await_drain(stdout_task).await;
        await_drain(stderr_task).await;

        let Some(status) = status else {
            append_event(&log, "supervisor stopped");
            return Ok(());
        };
        match status {
            Ok(status) => append_event(&log, &format!("Codexify exited with {status}")),
            Err(error) => append_event(&log, &format!("could not wait for Codexify: {error}")),
        }

        let delay = if started.elapsed() >= policy.stable_runtime {
            policy.restart_min_delay
        } else {
            restart_delay
        };
        restart_delay = (delay * 2).min(policy.restart_max_delay);
        append_event(
            &log,
            &format!("restarting Codexify in {} seconds", delay.as_secs()),
        );
        tokio::select! {
            _ = &mut shutdown => {
                append_event(&log, "supervisor stopped");
                return Ok(());
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

fn open_log_file(path: &Path) -> io::Result<(File, u64)> {
    use std::hash::{Hash, Hasher};

    let file = File::open(path)?;
    let handle = same_file::Handle::from_file(file.try_clone()?)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    handle.hash(&mut hasher);
    Ok((file, hasher.finish()))
}

fn tail_bytes(path: &Path) -> io::Result<(Vec<u8>, u64, u64)> {
    let (mut file, identity) = open_log_file(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(LOG_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if start > 0
        && let Some(index) = bytes.iter().position(|byte| *byte == b'\n')
    {
        bytes.drain(..=index);
    }

    let mut newlines = 0;
    let mut slice_start = 0;
    for (index, byte) in bytes.iter().enumerate().rev() {
        if *byte == b'\n' {
            newlines += 1;
            if newlines > LOG_TAIL_LINES {
                slice_start = index + 1;
                break;
            }
        }
    }
    Ok((bytes[slice_start..].to_vec(), length, identity))
}

fn append_from(path: &Path, position: &mut u64, identity: &mut Option<u64>) -> io::Result<Vec<u8>> {
    let (mut file, current_identity) = open_log_file(path)?;
    let length = file.metadata()?.len();
    if identity.as_ref() != Some(&current_identity) || length < *position {
        *position = 0;
    }
    *identity = Some(current_identity);
    if length == *position {
        return Ok(Vec::new());
    }
    file.seek(SeekFrom::Start(*position))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    *position = length;
    Ok(bytes)
}

pub async fn print_logs(follow: bool) -> anyhow::Result<()> {
    let path = log_path()?;
    let mut position = 0;
    let mut identity = None;
    match tail_bytes(&path) {
        Ok((bytes, length, current_identity)) => {
            io::stdout().write_all(&bytes)?;
            io::stdout().flush()?;
            position = length;
            identity = Some(current_identity);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && follow => {
            eprintln!("Waiting for service log {}", path.display());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            println!("No service log exists at {}", path.display());
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    }

    if !follow {
        return Ok(());
    }

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                match append_from(&path, &mut position, &mut identity) {
                    Ok(bytes) if !bytes.is_empty() => {
                        io::stdout().write_all(&bytes)?;
                        io::stdout().flush()?;
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        position = 0;
                        identity = None;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(root: &Path) -> ServiceSpec {
        ServiceSpec {
            executable: root.join("bin/codexify"),
            config: root.join("config/codexify.config.json"),
            working_dir: root.join("config"),
            home: root.join("home"),
            path: "/usr/local/bin:/usr/bin:/bin".to_string(),
        }
    }

    #[test]
    fn native_definitions_use_absolute_config_and_restart_policy() {
        let root = Path::new("/tmp/codexify-test");
        let spec = spec(root);
        let unit = systemd_unit(&spec);
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("KillMode=control-group"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains("ExecStart=\""));
        assert!(unit.contains(&format!(
            "service run --config {}",
            systemd_escape(&spec.config.to_string_lossy())
        )));

        let plist = launchd_plist(&spec);
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>AbandonProcessGroup</key>"));
        assert!(plist.contains(&xml_escape(&spec.config.to_string_lossy())));

        let script = windows_install_script(&spec);
        assert!(script.contains("New-ScheduledTaskTrigger -AtLogOn"));
        assert!(script.contains("-RestartCount 999"));
        assert!(script.contains("New-TimeSpan -Minutes 1"));
        assert!(script.contains(&windows_action_arguments(&spec.config)));
    }

    #[test]
    fn native_definition_escaping_is_lossless() {
        let root = Path::new("/tmp/codexify & test/$HOME");
        let spec = spec(root);
        assert!(launchd_plist(&spec).contains("codexify &amp; test"));
        let unit = systemd_unit(&spec);
        assert!(unit.contains("codexify & test"));
        assert!(unit.contains("$$HOME"));
        assert_eq!(powershell_quote("a'b"), "'a''b'");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launchd_definition_is_valid_plist() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("dev.codexify.service.plist");
        fs::write(&path, launchd_plist(&spec(root.path()))).unwrap();
        let output = StdCommand::new("plutil")
            .args(["-lint"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(windows)]
    #[test]
    fn scheduled_task_definition_is_valid_powershell() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("service-install.ps1");
        fs::write(&path, windows_install_script(&spec(root.path()))).unwrap();
        let path = powershell_quote(&path.to_string_lossy());
        let parser = format!(
            "$tokens = $null\n$errors = $null\n[System.Management.Automation.Language.Parser]::ParseFile({path}, [ref]$tokens, [ref]$errors) | Out-Null\nif ($errors.Count) {{ $errors | ForEach-Object {{ Write-Error $_ }}; exit 1 }}\n"
        );
        let output = powershell_command(&parser).output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn rotating_log_keeps_bounded_generations() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("codexify.log");
        let mut log = RotatingLog::open(path.clone(), 16, 2).unwrap();
        log.append(b"1234567890").unwrap();
        log.append(b"abcdefghij").unwrap();
        log.append(b"ABCDEFGHIJ").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"ABCDEFGHIJ");
        assert_eq!(fs::read(rotated_path(&path, 1)).unwrap(), b"abcdefghij");
        assert_eq!(fs::read(rotated_path(&path, 2)).unwrap(), b"1234567890");
        assert!(!rotated_path(&path, 3).exists());
    }

    #[test]
    fn log_tail_returns_only_recent_lines() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("codexify.log");
        let contents = (0..250)
            .map(|index| format!("line-{index}\n"))
            .collect::<String>();
        fs::write(&path, contents).unwrap();
        let (tail, length, _) = tail_bytes(&path).unwrap();
        let tail = String::from_utf8(tail).unwrap();
        assert!(!tail.contains("line-0\n"));
        assert!(tail.contains("line-249\n"));
        assert_eq!(length, fs::metadata(path).unwrap().len());
    }

    #[test]
    fn log_follow_detects_rotation_even_when_the_new_file_is_longer() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("codexify.log");
        fs::write(&path, b"old\n").unwrap();
        let (_, mut position, identity) = tail_bytes(&path).unwrap();
        let mut identity = Some(identity);

        fs::rename(&path, rotated_path(&path, 1)).unwrap();
        fs::write(&path, b"new-file-is-longer-than-old\n").unwrap();

        assert_eq!(
            append_from(&path, &mut position, &mut identity).unwrap(),
            b"new-file-is-longer-than-old\n"
        );
    }

    #[test]
    fn service_spec_rejects_an_existing_non_file_config() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config-directory");
        fs::create_dir(&config).unwrap();
        let error = ServiceSpec::resolve(&config).unwrap_err().to_string();
        assert!(error.contains("service config path is not a file"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_restarts_failed_server_and_captures_output() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("codexify.config.json");
        let executable = root.path().join("codexify-fake");
        let count = root.path().join("starts");
        let descendant = root.path().join("descendant");
        let log_path = root.path().join("codexify.log");
        fs::write(&config, "{}").unwrap();
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\ncount=0\n[ ! -f '{}' ] || count=$(cat '{}')\nprintf '%s\\n' $((count + 1)) > '{}'\nsleep 30 &\nprintf '%s\\n' $! > '{}'\nprintf 'fake server run %s\\n' $((count + 1))\nexit 7\n",
                count.display(),
                count.display(),
                count.display(),
                descendant.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let log = Arc::new(StdMutex::new(
            RotatingLog::open(log_path.clone(), 1024 * 1024, 2).unwrap(),
        ));
        let policy = SupervisorPolicy {
            restart_min_delay: Duration::from_millis(5),
            restart_max_delay: Duration::from_millis(20),
            stable_runtime: Duration::from_secs(1),
            config_retry_delay: Duration::from_millis(5),
        };

        let count_for_shutdown = count.clone();
        tokio::time::timeout(
            Duration::from_secs(5),
            supervise(config, executable, log, policy, async move {
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    if fs::read_to_string(&count_for_shutdown)
                        .ok()
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .is_some_and(|starts| starts >= 2)
                        || Instant::now() >= deadline
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }),
        )
        .await
        .expect("supervisor hung while a descendant held its output pipe")
        .unwrap();

        let output = fs::read_to_string(&log_path).unwrap();
        let starts: usize = fs::read_to_string(count)
            .unwrap_or_else(|error| panic!("{error}; service log:\n{output}"))
            .trim()
            .parse()
            .unwrap();
        assert!(
            starts >= 2,
            "supervisor started the server only {starts} time(s)"
        );
        assert!(output.contains("fake server run"));
        assert!(output.contains("Codexify exited with"));
        assert!(output.contains("restarting Codexify"));

        let descendant: i32 = fs::read_to_string(descendant)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while unsafe { libc::kill(descendant, 0) } == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_ne!(unsafe { libc::kill(descendant, 0) }, 0);
    }

    #[cfg(windows)]
    #[test]
    fn windows_service_paths_drop_verbatim_prefixes() {
        assert_eq!(
            normalize_native_path(PathBuf::from(r"\\?\C:\Users\Paul\codexify.exe")),
            PathBuf::from(r"C:\Users\Paul\codexify.exe")
        );
        assert_eq!(
            normalize_native_path(PathBuf::from(r"\\?\UNC\server\share\codexify.exe")),
            PathBuf::from(r"\\server\share\codexify.exe")
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_job_closes_the_supervised_process_tree() {
        let mut child = TokioCommand::new("cmd.exe")
            .args(["/C", "ping 127.0.0.1 -n 30 >NUL"])
            .spawn()
            .unwrap();
        let job = WindowsChildJob::new().unwrap();
        job.assign(&child).unwrap();
        drop(job);
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("job close did not terminate the child")
            .unwrap();
        assert!(!status.success());
    }
}
