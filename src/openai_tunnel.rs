//! Lifecycle and verified installation for OpenAI's outbound Secure MCP Tunnel.

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep, timeout};
use zip::ZipArchive;

use crate::types::{AppConfig, OpenAiTunnelConfig};
use crate::util::home_dir;

pub const TUNNEL_CLIENT_VERSION: &str = "0.0.12";
const RELEASE_BASE: &str = "https://github.com/openai/tunnel-client/releases/download/v0.0.12";
const MAX_DOWNLOAD_BYTES: usize = 100 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_KEY_BYTES: u64 = 64 * 1024;
const READY_TIMEOUT: Duration = Duration::from_secs(120);
const READY_POLL_INTERVAL: Duration = Duration::from_millis(500);
const CHILD_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const LOG_TAIL_BYTES: u64 = 32 * 1024;
const DETAIL_MAX_CHARS: usize = 2_000;
const POLL_SUCCESS_METRIC: &str = "commands_poll_last_successful_timestamp_seconds";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallManifest {
    version: u8,
    tunnel_client_version: String,
    asset: String,
    archive_sha256: String,
    binary_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseAsset {
    archive_name: String,
    binary_name: String,
}

enum ControlPlaneReadiness {
    Ready,
    Pending(String),
    Failed(String),
}

pub struct RunningOpenAiTunnel {
    child: Child,
    _runtime_dir: TempDir,
    log_path: PathBuf,
    health_url: String,
}

impl RunningOpenAiTunnel {
    pub fn health_url(&self) -> &str {
        &self.health_url
    }

    pub async fn wait_for_exit(&mut self) -> anyhow::Error {
        match self.child.wait().await {
            Ok(status) => anyhow!(
                "OpenAI tunnel runtime exited unexpectedly with {status}: {}",
                log_tail(&self.log_path)
            ),
            Err(error) => anyhow!("failed to wait for OpenAI tunnel runtime: {error}"),
        }
    }

    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }

        terminate_child(&mut self.child).await?;
        Ok(())
    }
}

pub async fn start(config: &AppConfig) -> anyhow::Result<RunningOpenAiTunnel> {
    let settings = config
        .openai_tunnel
        .as_ref()
        .context("OpenAI tunnel configuration is missing")?;
    validate_key_reference(&settings.api_key_ref)?;
    let client_path = resolve_client(settings).await?;

    let runtime_dir = tempfile::Builder::new()
        .prefix("codexify-openai-tunnel-")
        .tempdir()
        .context("create private OpenAI tunnel runtime directory")?;
    make_private_dir(runtime_dir.path())?;

    let health_url_path = runtime_dir.path().join("health.url");
    let log_path = runtime_dir.path().join("tunnel.log");
    let log = private_log_file(&log_path)?;
    let log_stderr = log.try_clone().context("clone OpenAI tunnel log handle")?;
    let target_url = format!("http://127.0.0.1:{}/mcp", config.port);

    let mut command = Command::new(&client_path);
    command
        .args(runtime_args(settings, &health_url_path, &target_url))
        .env_remove("TUNNEL_CLIENT_CONFIG")
        .env_remove("TUNNEL_CLIENT_PROFILE")
        .env_remove("TUNNEL_CLIENT_PROFILE_FILE")
        .env_remove("TUNNEL_CLIENT_PROFILE_DIR")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_stderr))
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .with_context(|| format!("start OpenAI tunnel runtime at {}", client_path.display()))?;

    let health_url = match wait_until_ready(&mut child, &health_url_path, &log_path).await {
        Ok(url) => url,
        Err(error) => {
            let _ = terminate_child(&mut child).await;
            return Err(error);
        }
    };

    Ok(RunningOpenAiTunnel {
        child,
        _runtime_dir: runtime_dir,
        log_path,
        health_url,
    })
}

fn runtime_args(
    settings: &OpenAiTunnelConfig,
    health_url_path: &Path,
    target_url: &str,
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("run"),
        OsString::from("--control-plane.tunnel-id"),
        OsString::from(&settings.tunnel_id),
        OsString::from("--control-plane.api-key"),
        OsString::from(&settings.api_key_ref),
        OsString::from("--mcp.server-url"),
        OsString::from(target_url),
        OsString::from("--mcp.startup-wait-timeout"),
        OsString::from("15s"),
        OsString::from("--health.listen-addr"),
        OsString::from("127.0.0.1:0"),
        OsString::from("--health.url-file"),
        health_url_path.as_os_str().to_os_string(),
        OsString::from("--log.format"),
        OsString::from("json"),
    ];
    if let Some(organization_id) = &settings.organization_id {
        args.push(OsString::from("--control-plane.organization-id"));
        args.push(OsString::from(organization_id));
    }
    args
}

async fn wait_until_ready(
    child: &mut Child,
    health_url_path: &Path,
    log_path: &Path,
) -> anyhow::Result<String> {
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .context("build loopback health client")?;
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut last_detail = "waiting for tunnel-client to publish its health URL".to_string();

    loop {
        if let Some(status) = child.try_wait()? {
            bail!(
                "OpenAI tunnel runtime exited during startup with {status}: {}",
                log_tail(log_path)
            );
        }

        if let Ok(raw) = std::fs::read_to_string(health_url_path) {
            match parse_loopback_health_url(&raw) {
                Ok(base_url) => {
                    let ready_url = base_url.join("readyz").context("build /readyz URL")?;
                    match client.get(ready_url).send().await {
                        Ok(response) if response.status() == StatusCode::OK => {
                            match probe_control_plane(&client, &base_url).await {
                                ControlPlaneReadiness::Ready => {
                                    return Ok(base_url.as_str().trim_end_matches('/').to_string());
                                }
                                ControlPlaneReadiness::Pending(detail) => {
                                    last_detail = detail;
                                }
                                ControlPlaneReadiness::Failed(detail) => {
                                    bail!(
                                        "OpenAI tunnel control-plane validation failed: {detail}"
                                    );
                                }
                            }
                        }
                        Ok(response) => {
                            let status = response.status();
                            let body = response
                                .bytes()
                                .await
                                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                                .unwrap_or_default();
                            last_detail = sanitize_detail(format!(
                                "/readyz returned {status}: {}",
                                body.trim()
                            ));
                        }
                        Err(error) => {
                            last_detail = sanitize_detail(format!(
                                "could not query tunnel-client /readyz: {error}"
                            ));
                        }
                    }
                }
                Err(error) => last_detail = error.to_string(),
            }
        }

        if Instant::now() >= deadline {
            bail!(
                "OpenAI tunnel runtime did not become ready within {} seconds ({last_detail}): {}",
                READY_TIMEOUT.as_secs(),
                log_tail(log_path)
            );
        }
        sleep(READY_POLL_INTERVAL).await;
    }
}

async fn probe_control_plane(client: &Client, base_url: &Url) -> ControlPlaneReadiness {
    let status_url = match base_url.join("api/status") {
        Ok(url) => url,
        Err(error) => {
            return ControlPlaneReadiness::Pending(sanitize_detail(format!(
                "could not build tunnel-client status URL: {error}"
            )));
        }
    };
    let status = match client.get(status_url).send().await {
        Ok(response) if response.status() == StatusCode::OK => match response.text().await {
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(value) => value,
                Err(error) => {
                    return ControlPlaneReadiness::Pending(sanitize_detail(format!(
                        "could not parse tunnel-client status: {error}"
                    )));
                }
            },
            Err(error) => {
                return ControlPlaneReadiness::Pending(sanitize_detail(format!(
                    "could not read tunnel-client status: {error}"
                )));
            }
        },
        Ok(response) => {
            return ControlPlaneReadiness::Pending(format!(
                "tunnel-client status returned {}",
                response.status()
            ));
        }
        Err(error) => {
            return ControlPlaneReadiness::Pending(sanitize_detail(format!(
                "could not query tunnel-client status: {error}"
            )));
        }
    };

    if let Some(error) = status
        .get("tunnel_metadata_error")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        return ControlPlaneReadiness::Failed(sanitize_detail(error));
    }
    if !status
        .get("tunnel_metadata")
        .is_some_and(serde_json::Value::is_object)
    {
        return ControlPlaneReadiness::Pending(
            "waiting for OpenAI tunnel metadata and Tunnels Read permission".into(),
        );
    }

    let metrics_url = match base_url.join("metrics") {
        Ok(url) => url,
        Err(error) => {
            return ControlPlaneReadiness::Pending(sanitize_detail(format!(
                "could not build tunnel-client metrics URL: {error}"
            )));
        }
    };
    let metrics = match client.get(metrics_url).send().await {
        Ok(response) if response.status() == StatusCode::OK => match response.text().await {
            Ok(text) => text,
            Err(error) => {
                return ControlPlaneReadiness::Pending(sanitize_detail(format!(
                    "could not read tunnel-client metrics: {error}"
                )));
            }
        },
        Ok(response) => {
            return ControlPlaneReadiness::Pending(format!(
                "tunnel-client metrics returned {}",
                response.status()
            ));
        }
        Err(error) => {
            return ControlPlaneReadiness::Pending(sanitize_detail(format!(
                "could not query tunnel-client metrics: {error}"
            )));
        }
    };

    match parse_metric_value(&metrics, POLL_SUCCESS_METRIC) {
        Some(value) if value > 0.0 => ControlPlaneReadiness::Ready,
        Some(_) => ControlPlaneReadiness::Pending(
            "waiting for the first successful OpenAI control-plane poll and Tunnels Use permission"
                .into(),
        ),
        None => ControlPlaneReadiness::Pending(format!(
            "tunnel-client metrics did not expose {POLL_SUCCESS_METRIC}"
        )),
    }
}

fn parse_metric_value(metrics: &str, name: &str) -> Option<f64> {
    metrics.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let mut fields = line.split_whitespace();
        let metric_name = fields.next()?;
        if metric_name != name {
            return None;
        }
        fields.next()?.parse().ok()
    })
}

fn parse_loopback_health_url(raw: &str) -> anyhow::Result<Url> {
    let url = Url::parse(raw.trim()).context("tunnel-client wrote an invalid health URL")?;
    if url.scheme() != "http" {
        bail!("tunnel-client health URL must use loopback HTTP");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("tunnel-client health URL must not contain credentials");
    }
    let loopback = match url.host_str() {
        Some("localhost" | "::1") => true,
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback()),
        None => false,
    };
    if !loopback {
        bail!("tunnel-client health URL is not loopback-only");
    }
    Ok(url)
}

async fn resolve_client(settings: &OpenAiTunnelConfig) -> anyhow::Result<PathBuf> {
    if let Some(path) = &settings.client_path {
        validate_client(path, None).await?;
        return Ok(path.clone());
    }

    let asset = release_asset()?;
    let binary_path = managed_binary_path(&asset.binary_name)?;
    let manifest_path = binary_path
        .parent()
        .context("managed tunnel-client path has no parent")?
        .join("manifest.json");

    match (binary_path.exists(), manifest_path.exists()) {
        (true, true) => {
            validate_managed_install(&binary_path, &manifest_path, &asset).await?;
            Ok(binary_path)
        }
        (false, false) => {
            println!("Installing verified OpenAI tunnel runtime v{TUNNEL_CLIENT_VERSION}...");
            install_managed_client(&binary_path, &manifest_path, &asset).await?;
            Ok(binary_path)
        }
        _ => bail!(
            "incomplete managed OpenAI tunnel installation under {}; remove that version directory and restart",
            binary_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .display()
        ),
    }
}

async fn validate_managed_install(
    binary_path: &Path,
    manifest_path: &Path,
    asset: &ReleaseAsset,
) -> anyhow::Result<()> {
    let manifest: InstallManifest = serde_json::from_slice(
        &std::fs::read(manifest_path).context("read OpenAI tunnel install manifest")?,
    )
    .context("parse OpenAI tunnel install manifest")?;
    if manifest.version != 1
        || manifest.tunnel_client_version != TUNNEL_CLIENT_VERSION
        || manifest.asset != asset.archive_name
    {
        bail!("managed OpenAI tunnel installation manifest does not match this Codexify build");
    }
    let actual_hash =
        sha256_hex(&std::fs::read(binary_path).context("read managed OpenAI tunnel runtime")?);
    if actual_hash != manifest.binary_sha256 {
        bail!("managed OpenAI tunnel runtime failed its integrity check");
    }
    validate_client(binary_path, Some(TUNNEL_CLIENT_VERSION)).await
}

async fn install_managed_client(
    binary_path: &Path,
    manifest_path: &Path,
    asset: &ReleaseAsset,
) -> anyhow::Result<()> {
    let archive_url = format!("{RELEASE_BASE}/{}", asset.archive_name);
    let sums_url = format!("{RELEASE_BASE}/SHA256SUMS.txt");
    let (archive, sums) = tokio::try_join!(fetch_bytes(&archive_url), fetch_bytes(&sums_url))?;
    let expected_hash =
        parse_expected_checksum(&String::from_utf8_lossy(&sums), &asset.archive_name)?;
    let archive_hash = sha256_hex(&archive);
    if archive_hash != asset.archive_sha256 {
        bail!(
            "OpenAI tunnel runtime archive does not match the hash pinned by this Codexify build"
        );
    }

    let binary = extract_binary(&archive, &asset.binary_name)?;
    let parent = binary_path
        .parent()
        .context("managed tunnel-client path has no parent")?;
    make_private_dir(parent)?;
    atomic_write(binary_path, &binary, true)?;
    if let Err(error) = validate_client(binary_path, Some(TUNNEL_CLIENT_VERSION)).await {
        let _ = std::fs::remove_file(binary_path);
        return Err(error);
    }

    let manifest = InstallManifest {
        version: 1,
        tunnel_client_version: TUNNEL_CLIENT_VERSION.to_string(),
        asset: asset.archive_name.clone(),
        archive_sha256: archive_hash,
        binary_sha256: sha256_hex(&binary),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    atomic_write(manifest_path, &manifest_bytes, false)?;
    Ok(())
}

async fn fetch_bytes(url: &str) -> anyhow::Result<Vec<u8>> {
    let client = Client::builder()
        .redirect(Policy::limited(5))
        .timeout(Duration::from_secs(120))
        .build()
        .context("build OpenAI tunnel download client")?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("download {url}"))?
        .error_for_status()
        .with_context(|| format!("download {url}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DOWNLOAD_BYTES as u64)
    {
        bail!("OpenAI tunnel download exceeds {MAX_DOWNLOAD_BYTES} bytes");
    }
    let bytes = response
        .bytes()
        .await
        .context("read OpenAI tunnel download")?;
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        bail!("OpenAI tunnel download exceeds {MAX_DOWNLOAD_BYTES} bytes");
    }
    Ok(bytes.to_vec())
}

fn release_asset() -> anyhow::Result<ReleaseAsset> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        other => bail!("OpenAI tunnel runtime has no pinned build for OS {other}"),
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => bail!("OpenAI tunnel runtime has no pinned build for architecture {other}"),
    };
    Ok(ReleaseAsset {
        archive_name: format!("tunnel-client-runtime-v{TUNNEL_CLIENT_VERSION}-{os}-{arch}.zip"),
        binary_name: if cfg!(windows) {
            "tunnel-client-runtime.exe".to_string()
        } else {
            "tunnel-client-runtime".to_string()
        },
    })
}

fn managed_binary_path(binary_name: &str) -> anyhow::Result<PathBuf> {
    let home =
        home_dir().context("cannot install OpenAI tunnel runtime: home directory is unknown")?;
    Ok(home
        .join(".codexify")
        .join("openai-tunnel")
        .join(format!("v{TUNNEL_CLIENT_VERSION}"))
        .join(binary_name))
}

fn parse_expected_checksum(text: &str, asset: &str) -> anyhow::Result<String> {
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(hash) = fields.next() else {
            continue;
        };
        let Some(name) = fields.next() else {
            continue;
        };
        if name.trim_start_matches('*') == asset
            && hash.len() == 64
            && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Ok(hash.to_ascii_lowercase());
        }
    }
    bail!("official SHA256SUMS.txt has no valid entry for {asset}")
}

fn extract_binary(archive: &[u8], binary_name: &str) -> anyhow::Result<Vec<u8>> {
    let mut zip =
        ZipArchive::new(Cursor::new(archive)).context("open tunnel-client release ZIP")?;
    let mut entry = zip
        .by_name(binary_name)
        .with_context(|| format!("release ZIP does not contain {binary_name}"))?;
    if !entry.is_file() || entry.size() > MAX_BINARY_BYTES {
        bail!("tunnel-client binary in release ZIP is not a bounded regular file");
    }
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry
        .by_ref()
        .take(MAX_BINARY_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read tunnel-client binary from release ZIP")?;
    if bytes.len() as u64 > MAX_BINARY_BYTES {
        bail!("tunnel-client binary in release ZIP exceeds {MAX_BINARY_BYTES} bytes");
    }
    Ok(bytes)
}

async fn validate_client(path: &Path, exact_version: Option<&str>) -> anyhow::Result<()> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("OpenAI tunnel client does not exist: {}", path.display()))?;
    if !metadata.is_file() {
        bail!("OpenAI tunnel client is not a file: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("OpenAI tunnel client is not executable: {}", path.display());
        }
    }

    let version = timeout(
        Duration::from_secs(10),
        Command::new(path).arg("--version").output(),
    )
    .await
    .context("OpenAI tunnel client version check timed out")??;
    if !version.status.success() {
        bail!(
            "OpenAI tunnel client version check failed: {}",
            command_output(&version.stdout, &version.stderr)
        );
    }
    let version_output = command_output(&version.stdout, &version.stderr);
    if let Some(expected) = exact_version
        && !version_output.contains(expected)
    {
        bail!(
            "managed OpenAI tunnel client reported an unexpected version: {}",
            sanitize_detail(version_output)
        );
    }

    let help = timeout(
        Duration::from_secs(10),
        Command::new(path).args(["run", "--help"]).output(),
    )
    .await
    .context("OpenAI tunnel client compatibility check timed out")??;
    let help_output = command_output(&help.stdout, &help.stderr);
    if !help.status.success()
        || !help_output.contains("--control-plane.tunnel-id")
        || !help_output.contains("--mcp.server-url")
        || !help_output.contains("--health.url-file")
    {
        bail!(
            "OpenAI tunnel client is incompatible with Codexify's supervised runtime mode: {}",
            sanitize_detail(help_output)
        );
    }
    Ok(())
}

fn validate_key_reference(reference: &str) -> anyhow::Result<()> {
    if let Some(name) = reference.strip_prefix("env:") {
        let value = std::env::var_os(name).ok_or_else(|| {
            anyhow!("OpenAI tunnel API-key environment variable {name} is not set")
        })?;
        if value.is_empty() {
            bail!("OpenAI tunnel API-key environment variable {name} is empty");
        }
        return Ok(());
    }

    let path = reference
        .strip_prefix("file:")
        .map(Path::new)
        .context("OpenAI tunnel API key must use an env:NAME or file:/path reference")?;
    let metadata = std::fs::metadata(path).with_context(|| {
        format!(
            "OpenAI tunnel API-key file does not exist: {}",
            path.display()
        )
    })?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_KEY_BYTES {
        bail!("OpenAI tunnel API-key file must be a non-empty regular file under 64 KiB");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "OpenAI tunnel API-key file is readable by other users; run chmod 600 {}",
                path.display()
            );
        }
    }
    Ok(())
}

async fn terminate_child(child: &mut Child) -> anyhow::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }

    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if result != 0 {
            child.start_kill().context("kill OpenAI tunnel runtime")?;
        }
    }
    #[cfg(windows)]
    child.start_kill().context("kill OpenAI tunnel runtime")?;

    if timeout(CHILD_STOP_TIMEOUT, child.wait()).await.is_err() {
        child
            .start_kill()
            .context("force-kill OpenAI tunnel runtime")?;
        timeout(CHILD_STOP_TIMEOUT, child.wait())
            .await
            .context("OpenAI tunnel runtime did not exit after force-kill")??;
    }
    Ok(())
}

fn private_log_file(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("create private OpenAI tunnel log at {}", path.display()))
}

fn make_private_dir(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("create private directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg_attr(not(unix), allow(unused_variables))]
fn atomic_write(path: &Path, bytes: &[u8], executable: bool) -> anyhow::Result<()> {
    let parent = path.parent().context("atomic-write path has no parent")?;
    make_private_dir(parent)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = path.file_name().unwrap_or_else(|| OsStr::new("file"));
    let temp_path = parent.join(format!(
        ".{}.{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        nonce
    ));

    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    let result = (|| -> anyhow::Result<()> {
        let mut file = options.open(&temp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                &temp_path,
                std::fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }),
            )?;
        }
        std::fs::rename(&temp_path, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result.with_context(|| format!("write {}", path.display()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn command_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    [stderr, stdout]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn log_tail(path: &Path) -> String {
    let result = (|| -> std::io::Result<String> {
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(LOG_TAIL_BYTES)))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    })();
    match result {
        Ok(text) if !text.trim().is_empty() => sanitize_detail(text),
        Ok(_) => "no tunnel-client diagnostics were written".to_string(),
        Err(error) => format!("could not read tunnel-client diagnostics: {error}"),
    }
}

fn sanitize_detail(value: impl AsRef<str>) -> String {
    let tunnel_id = regex::Regex::new(r"tunnel_[a-z0-9]{32}").expect("valid tunnel-id regex");
    let api_key = regex::Regex::new(r"sk-[A-Za-z0-9_-]{12,}").expect("valid API-key regex");
    let redacted = tunnel_id.replace_all(value.as_ref(), "[tunnel-id]");
    api_key
        .replace_all(&redacted, "[redacted-key]")
        .chars()
        .take(DETAIL_MAX_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> OpenAiTunnelConfig {
        OpenAiTunnelConfig {
            tunnel_id: "tunnel_0123456789abcdef0123456789abcdef".into(),
            api_key_ref: "env:CONTROL_PLANE_API_KEY".into(),
            organization_id: Some("org_test".into()),
            client_path: None,
        }
    }

    #[test]
    fn builds_runtime_arguments_without_literal_secret_material() {
        let args = runtime_args(
            &settings(),
            Path::new("/tmp/health.url"),
            "http://127.0.0.1:3000/mcp",
        )
        .into_iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
        assert_eq!(args[0], "run");
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--control-plane.api-key", "env:CONTROL_PLANE_API_KEY"] })
        );
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--mcp.server-url", "http://127.0.0.1:3000/mcp"] })
        );
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--control-plane.organization-id", "org_test"] })
        );
    }

    #[test]
    fn parses_only_the_requested_release_checksum() {
        let sums = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  other.zip\n\
                    bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb *wanted.zip\n";
        assert_eq!(
            parse_expected_checksum(sums, "wanted.zip").unwrap(),
            "b".repeat(64)
        );
        assert!(parse_expected_checksum(sums, "missing.zip").is_err());
    }

    #[test]
    fn health_url_must_be_plain_loopback_http() {
        assert!(parse_loopback_health_url("http://127.0.0.1:49152/").is_ok());
        assert!(parse_loopback_health_url("http://[::1]:49152/").is_ok());
        assert!(parse_loopback_health_url("https://127.0.0.1:49152/").is_err());
        assert!(parse_loopback_health_url("http://example.com:49152/").is_err());
        assert!(parse_loopback_health_url("http://user@127.0.0.1:49152/").is_err());
    }

    #[test]
    fn diagnostics_redact_tunnel_ids_and_api_keys() {
        let detail = sanitize_detail(
            "tunnel=tunnel_0123456789abcdef0123456789abcdef key=sk-example_secret_123456",
        );
        assert_eq!(detail, "tunnel=[tunnel-id] key=[redacted-key]");
    }

    #[test]
    fn reads_the_first_successful_control_plane_poll_metric() {
        let metrics = "# HELP commands_poll_last_successful_timestamp_seconds last success\n\
                       commands_poll_cycles_total 2\n\
                       commands_poll_last_successful_timestamp_seconds 1787429375\n";
        assert_eq!(
            parse_metric_value(metrics, POLL_SUCCESS_METRIC),
            Some(1_787_429_375.0)
        );
        assert_eq!(parse_metric_value(metrics, "missing"), None);
    }

    #[test]
    fn current_platform_has_a_release_asset_when_supported() {
        let asset = release_asset();
        if matches!(std::env::consts::OS, "macos" | "linux" | "windows")
            && matches!(std::env::consts::ARCH, "aarch64" | "x86_64")
        {
            let asset = asset.unwrap();
            assert!(asset.archive_name.contains(TUNNEL_CLIENT_VERSION));
            assert!(asset.binary_name.starts_with("tunnel-client-runtime"));
        } else {
            assert!(asset.is_err());
        }
    }
}
