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
use zeroize::Zeroizing;
use zip::ZipArchive;

use crate::process_env::{
    CHILD_CONTROL_PLANE_API_KEY_ENV, CHILD_MCP_AUTHORIZATION_ENV, isolate_tunnel_child_env,
};
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

pub(crate) fn validate_tunnel_id(value: &str) -> anyhow::Result<()> {
    if value.strip_prefix("tunnel_").is_some_and(|suffix| {
        suffix.len() == 32
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    }) {
        return Ok(());
    }

    bail!("OpenAI tunnel id must be tunnel_ followed by 32 lowercase letters or digits")
}

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
    archive_sha256: &'static str,
}

/// Result of probing the tunnel-client's local health endpoints. Used both to
/// gate startup readiness and to monitor a running tunnel. `Unreachable` means
/// the probe itself failed (health endpoint down, connection refused/timeout);
/// `Unhealthy` means the endpoint answered but reported the tunnel not ready
/// (non-200, or the control-plane poll metric is missing/zero). The distinction
/// is diagnostic — both count as a health failure for the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelHealth {
    Healthy,
    Unhealthy(String),
    Unreachable(String),
}

/// Builds the loopback-only HTTP client used to probe the tunnel-client's health
/// endpoints. Shared by startup readiness and the running-tunnel health monitor.
pub fn build_health_client() -> anyhow::Result<Client> {
    Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .context("build loopback health client")
}

pub struct RunningOpenAiTunnel {
    child: Child,
    _runtime_dir: TempDir,
    log_path: PathBuf,
    health_url: String,
    base_url: Url,
}

impl RunningOpenAiTunnel {
    pub fn health_url(&self) -> &str {
        &self.health_url
    }

    /// Probes the running tunnel's local health endpoints. Cheap and side-effect
    /// free, so the supervisor can call it on a fixed interval.
    pub async fn check_health(&self, client: &Client) -> TunnelHealth {
        probe_tunnel_health(client, &self.base_url).await
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
    let control_plane_api_key = resolve_key_reference(&settings.api_key_ref)?;
    let internal_mcp_bearer = config
        .api_key
        .as_deref()
        .context("native tunnel mode requires an internal MCP bearer token")?;
    let internal_mcp_authorization = bearer_authorization_value(internal_mcp_bearer);
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
    isolate_tunnel_child_env(&mut command);
    command
        .args(runtime_args(settings, &health_url_path, &target_url))
        .env(
            CHILD_CONTROL_PLANE_API_KEY_ENV,
            control_plane_api_key.as_str(),
        )
        .env(
            CHILD_MCP_AUTHORIZATION_ENV,
            internal_mcp_authorization.as_str(),
        )
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_stderr))
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .with_context(|| format!("start OpenAI tunnel runtime at {}", client_path.display()))?;
    drop(command);
    drop(control_plane_api_key);
    drop(internal_mcp_authorization);

    let base_url = match wait_until_ready(&mut child, &health_url_path, &log_path).await {
        Ok(url) => url,
        Err(error) => {
            let _ = terminate_child(&mut child).await;
            return Err(error);
        }
    };
    let health_url = base_url.as_str().trim_end_matches('/').to_string();

    Ok(RunningOpenAiTunnel {
        child,
        _runtime_dir: runtime_dir,
        log_path,
        health_url,
        base_url,
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
        OsString::from(format!("env:{CHILD_CONTROL_PLANE_API_KEY_ENV}")),
        OsString::from("--mcp.server-url"),
        OsString::from(target_url),
        OsString::from("--mcp.extra-headers"),
        OsString::from(format!("Authorization: env:{CHILD_MCP_AUTHORIZATION_ENV}")),
        OsString::from("--mcp.discovery-extra-headers"),
        OsString::from(format!("Authorization: env:{CHILD_MCP_AUTHORIZATION_ENV}")),
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

fn bearer_authorization_value(token: &str) -> Zeroizing<String> {
    Zeroizing::new(format!("Bearer {token}"))
}

async fn wait_until_ready(
    child: &mut Child,
    health_url_path: &Path,
    log_path: &Path,
) -> anyhow::Result<Url> {
    let client = build_health_client()?;
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
                Ok(base_url) => match probe_tunnel_health(&client, &base_url).await {
                    TunnelHealth::Healthy => return Ok(base_url),
                    TunnelHealth::Unhealthy(detail) | TunnelHealth::Unreachable(detail) => {
                        last_detail = detail
                    }
                },
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

async fn probe_tunnel_health(client: &Client, base_url: &Url) -> TunnelHealth {
    let ready_url = match base_url.join("readyz") {
        Ok(url) => url,
        Err(error) => {
            return TunnelHealth::Unhealthy(sanitize_detail(format!(
                "could not build tunnel-client readiness URL: {error}"
            )));
        }
    };
    match client.get(ready_url).send().await {
        Ok(response) if response.status() == StatusCode::OK => {}
        Ok(response) => {
            let status = response.status();
            let body = response
                .bytes()
                .await
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            return TunnelHealth::Unhealthy(sanitize_detail(format!(
                "/readyz returned {status}: {}",
                body.trim()
            )));
        }
        Err(error) => {
            return TunnelHealth::Unreachable(sanitize_detail(format!(
                "could not query tunnel-client /readyz: {error}"
            )));
        }
    }

    let metrics_url = match base_url.join("metrics") {
        Ok(url) => url,
        Err(error) => {
            return TunnelHealth::Unhealthy(sanitize_detail(format!(
                "could not build tunnel-client metrics URL: {error}"
            )));
        }
    };
    let metrics = match client.get(metrics_url).send().await {
        Ok(response) if response.status() == StatusCode::OK => match response.text().await {
            Ok(text) => text,
            Err(error) => {
                return TunnelHealth::Unreachable(sanitize_detail(format!(
                    "could not read tunnel-client metrics: {error}"
                )));
            }
        },
        Ok(response) => {
            return TunnelHealth::Unhealthy(format!(
                "tunnel-client metrics returned {}",
                response.status()
            ));
        }
        Err(error) => {
            return TunnelHealth::Unreachable(sanitize_detail(format!(
                "could not query tunnel-client metrics: {error}"
            )));
        }
    };

    match parse_metric_value(&metrics, POLL_SUCCESS_METRIC) {
        Some(value) if value > 0.0 => TunnelHealth::Healthy,
        Some(_) => {
            TunnelHealth::Unhealthy("waiting for a successful OpenAI control-plane poll".into())
        }
        None => TunnelHealth::Unhealthy(format!(
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
        let split_at = line
            .char_indices()
            .find_map(|(index, ch)| (ch == '{' || ch.is_ascii_whitespace()).then_some(index))?;
        let metric_name = &line[..split_at];
        if metric_name != name {
            return None;
        }
        let remainder = &line[split_at..];
        let sample = if remainder.starts_with('{') {
            &remainder[remainder.rfind('}')? + 1..]
        } else {
            remainder
        };
        sample.split_whitespace().next()?.parse().ok()
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
        || manifest.archive_sha256 != asset.archive_sha256
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
    let archive = fetch_bytes(&archive_url).await?;
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
    let archive_sha256 = match (os, arch) {
        ("darwin", "amd64") => "ca05df2ab5397065fcf4b1e2e8ec330d9ad0d7a880a1f08265b36fc69eddd391",
        ("darwin", "arm64") => "924c7a1e0a2ea2c10f4f72b9c2e2382e7d55443831cdf8d84e394a54e83ccc30",
        ("linux", "amd64") => "31e9ece3f54f87126813fb206d465fd86b23462cc71734a787927b818f60d931",
        ("linux", "arm64") => "f02bc770367e328f21614841eb27393d7f023256224a6dde31c8aa4d6dc763f5",
        ("windows", "amd64") => "0721098f9edda72cc36f938adcb12cd6a0c49c6c0be7ad6ab6e412f966585f2e",
        ("windows", "arm64") => "952a30d469df749c88722e70441e72c541aa9ad878ab082678533f64bd31b2a9",
        _ => unreachable!("OS and architecture were validated above"),
    };
    Ok(ReleaseAsset {
        archive_name: format!("tunnel-client-runtime-v{TUNNEL_CLIENT_VERSION}-{os}-{arch}.zip"),
        binary_name: if cfg!(windows) {
            "tunnel-client-runtime.exe".to_string()
        } else {
            "tunnel-client-runtime".to_string()
        },
        archive_sha256,
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

    let version = client_probe_output(path, &["--version"], "version check").await?;
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

    let help = client_probe_output(path, &["run", "--help"], "compatibility check").await?;
    let help_output = command_output(&help.stdout, &help.stderr);
    if !help.status.success()
        || !help_output.contains("--control-plane.tunnel-id")
        || !help_output.contains("--mcp.server-url")
        || !help_output.contains("--mcp.extra-headers")
        || !help_output.contains("--mcp.discovery-extra-headers")
        || !help_output.contains("--health.url-file")
    {
        bail!(
            "OpenAI tunnel client is incompatible with Codexify's supervised runtime mode: {}",
            sanitize_detail(help_output)
        );
    }
    Ok(())
}

async fn client_probe_output(
    path: &Path,
    args: &[&str],
    operation: &str,
) -> anyhow::Result<std::process::Output> {
    let mut command = Command::new(path);
    isolate_tunnel_child_env(&mut command);
    command.args(args);
    timeout(Duration::from_secs(10), command.output())
        .await
        .with_context(|| format!("OpenAI tunnel client {operation} timed out"))?
        .with_context(|| format!("run OpenAI tunnel client {operation}"))
}

fn resolve_key_reference(reference: &str) -> anyhow::Result<Zeroizing<String>> {
    if let Some(name) = reference.strip_prefix("env:") {
        let value = std::env::var(name).map_err(|error| match error {
            std::env::VarError::NotPresent => {
                anyhow!("OpenAI tunnel API-key environment variable {name} is not set")
            }
            std::env::VarError::NotUnicode(_) => {
                anyhow!("OpenAI tunnel API-key environment variable {name} is not valid Unicode")
            }
        })?;
        validate_runtime_api_key(&value)?;
        return Ok(Zeroizing::new(value));
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
    let contents = Zeroizing::new(
        std::fs::read_to_string(path)
            .with_context(|| format!("read OpenAI tunnel API-key file {}", path.display()))?,
    );
    let value = Zeroizing::new(contents.trim().to_string());
    validate_runtime_api_key(&value)?;
    Ok(value)
}

pub(crate) fn validate_runtime_api_key(value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        bail!("OpenAI tunnel API key is empty");
    }
    if value.len() as u64 > MAX_KEY_BYTES {
        bail!("OpenAI tunnel API key exceeds 64 KiB");
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        bail!("OpenAI tunnel API key is malformed");
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
        assert!(args.windows(2).any(|pair| pair
            == [
                "--control-plane.api-key",
                "env:CODEXIFY_OPENAI_TUNNEL_API_KEY"
            ]));
        assert!(args.windows(2).any(|pair| pair
            == [
                "--mcp.extra-headers",
                "Authorization: env:CODEXIFY_INTERNAL_MCP_AUTHORIZATION"
            ]));
        assert!(args.windows(2).any(|pair| pair
            == [
                "--mcp.discovery-extra-headers",
                "Authorization: env:CODEXIFY_INTERNAL_MCP_AUTHORIZATION"
            ]));
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--mcp.server-url", "http://127.0.0.1:3000/mcp"] })
        );
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--control-plane.organization-id", "org_test"] })
        );
        assert!(!args.iter().any(|arg| arg == "env:CONTROL_PLANE_API_KEY"));
    }

    #[test]
    fn internal_mcp_header_uses_the_bearer_scheme() {
        assert_eq!(
            bearer_authorization_value("internal-token").as_str(),
            "Bearer internal-token"
        );
    }

    #[test]
    fn runtime_api_key_validation_matches_the_official_character_set() {
        assert!(validate_runtime_api_key("sk-valid_key-123").is_ok());
        assert!(validate_runtime_api_key("").is_err());
        assert!(validate_runtime_api_key("key with spaces").is_err());
        assert!(validate_runtime_api_key("key\nvalue").is_err());
        assert!(validate_runtime_api_key(&"a".repeat(MAX_KEY_BYTES as usize + 1)).is_err());
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
                       commands_poll_last_successful_timestamp_seconds{otel_scope_name=\"controlplane\",otel_scope_schema_url=\"\",otel_scope_version=\"\"} 1787429375\n";
        assert_eq!(
            parse_metric_value(metrics, POLL_SUCCESS_METRIC),
            Some(1_787_429_375.0)
        );
        assert_eq!(parse_metric_value(metrics, "missing"), None);
    }

    #[tokio::test]
    async fn runtime_only_health_contract_needs_no_admin_api() {
        use axum::{Router, routing::get};

        let app = Router::new()
            .route("/readyz", get(|| async { "ready" }))
            .route(
                "/metrics",
                get(|| async {
                    "commands_poll_last_successful_timestamp_seconds{otel_scope_name=\"controlplane\"} 1787429375\n"
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base_url = Url::parse(&format!("http://{address}/")).unwrap();
        let client = Client::builder().no_proxy().build().unwrap();

        assert_eq!(
            client
                .get(base_url.join("api/status").unwrap())
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            probe_tunnel_health(&client, &base_url).await,
            TunnelHealth::Healthy
        );

        server.abort();
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
            assert_eq!(asset.archive_sha256.len(), 64);
            assert!(
                asset
                    .archive_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            );
        } else {
            assert!(asset.is_err());
        }
    }
}
