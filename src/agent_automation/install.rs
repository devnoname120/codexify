use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use flate2::read::GzDecoder;
use reqwest::redirect::Policy;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::Archive;
use zip::ZipArchive;

use crate::markdown_chat::AgentAutomationConfig;
use crate::util::home_dir;

use super::process::{
    executable as process_executable, managed_descendants, orphaned_browser, same_process,
};
use super::{
    BrowserCompanionClient, CompanionSessionStatus, PROTOCOL_BUILD_SUFFIX, PROTOCOL_SCHEMA_HASH,
    PROTOCOL_VERSION,
};

const MANIFEST_VERSION: u32 = 1;
const MANIFEST_FILE: &str = "manifest.json";
const ACCOUNT_FILE: &str = "account.json";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 100_000;
const MAX_ARCHIVE_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ARCHIVE_EXPANDED_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: usize = 768 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: usize = 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const START_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const LIFECYCLE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DEFAULT_RELEASE_ROOT: &str = "https://github.com/devnoname120/codexify/releases/download";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserArchiveKind {
    TarGz,
    Zip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserReleaseAsset {
    pub tag: String,
    pub version: String,
    pub platform: String,
    pub archive_name: String,
    pub kind: BrowserArchiveKind,
    pub release_url: String,
    pub checksums_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRuntimeLayout {
    pub root: PathBuf,
    pub runtime_root: PathBuf,
    pub runtime_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub descriptor_path: PathBuf,
    pub launch_config_path: PathBuf,
    pub log_path: PathBuf,
    pub account_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrowserRuntimeManifest {
    pub version: u32,
    pub codexify_version: String,
    pub protocol_version: u32,
    pub protocol_schema_hash: String,
    pub build: String,
    pub platform: String,
    pub entrypoint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserInstallReceipt {
    pub version: String,
    pub build: String,
    pub platform: String,
    pub runtime_dir: PathBuf,
    pub entrypoint: PathBuf,
    pub archive_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserLifecycleState {
    Disabled,
    NotInstalled,
    Stopped,
    Incompatible,
    AuthenticationRequired,
    AccountChanged,
    Busy,
    Ready,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserCompanionStatus {
    pub state: BrowserLifecycleState,
    pub enabled: bool,
    pub installed: bool,
    pub running: bool,
    pub authenticated: bool,
    pub active_action_id: Option<String>,
    pub version: String,
    pub build: Option<String>,
    pub platform: String,
    pub runtime_dir: Option<PathBuf>,
    pub descriptor_path: PathBuf,
    pub log_path: PathBuf,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
struct InstalledBrowserRuntime {
    manifest: BrowserRuntimeManifest,
    entrypoint: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BrowserAccountRecord {
    version: u32,
    fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct BrowserCompanionManager {
    config: AgentAutomationConfig,
    layout: BrowserRuntimeLayout,
    asset: BrowserReleaseAsset,
    expected_build: String,
}

impl BrowserCompanionManager {
    pub fn from_config(config: &AgentAutomationConfig) -> Result<Self, String> {
        let root = match std::env::var_os("CODEXIFY_BROWSER_HOME") {
            Some(path) => PathBuf::from(path),
            None => expand_descriptor_path(&config.descriptor_path)?
                .parent()
                .ok_or("Browser descriptor has no parent directory")?
                .to_path_buf(),
        };
        let release_root = std::env::var("CODEXIFY_BROWSER_RELEASE_ROOT")
            .unwrap_or_else(|_| DEFAULT_RELEASE_ROOT.to_string());
        Self::new(
            root,
            config.clone(),
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            &release_root,
        )
    }

    #[doc(hidden)]
    pub fn for_test(
        root: PathBuf,
        config: AgentAutomationConfig,
        codexify_version: &str,
        os: &str,
        arch: &str,
        release_root: &str,
    ) -> Result<Self, String> {
        Self::new(root, config, codexify_version, os, arch, release_root)
    }

    fn new(
        root: PathBuf,
        config: AgentAutomationConfig,
        codexify_version: &str,
        os: &str,
        arch: &str,
        release_root: &str,
    ) -> Result<Self, String> {
        if !root.is_absolute() {
            return Err("Browser companion root must be absolute".into());
        }
        let version = Version::parse(codexify_version)
            .map_err(|_| "Browser companion Codexify version is invalid".to_string())?;
        let version_text = version.to_string();
        let expected_build = format!("{version_text}{PROTOCOL_BUILD_SUFFIX}");
        let descriptor_path = expand_descriptor_path(&config.descriptor_path)?;
        let asset = release_asset(&version_text, os, arch, release_root)?;
        let runtime_root = root.join("runtime");
        let runtime_dir = runtime_root.join(&version_text);
        let layout = BrowserRuntimeLayout {
            root: root.clone(),
            runtime_root,
            manifest_path: runtime_dir.join(MANIFEST_FILE),
            runtime_dir,
            descriptor_path,
            launch_config_path: root.join("launch.json"),
            log_path: root.join("companion.log"),
            account_path: root.join(ACCOUNT_FILE),
        };
        Ok(Self {
            config,
            layout,
            asset,
            expected_build,
        })
    }

    pub fn layout(&self) -> &BrowserRuntimeLayout {
        &self.layout
    }

    pub fn release_asset(&self) -> &BrowserReleaseAsset {
        &self.asset
    }

    pub fn expected_build(&self) -> &str {
        &self.expected_build
    }

    pub async fn install(&self) -> Result<BrowserInstallReceipt, String> {
        if !self.config.enabled {
            return Err("Browser automation is disabled; enable agentChat.automation before installing the companion".into());
        }
        crate::tls::ensure_crypto_provider();
        let client = crate::tls::client_builder()
            .redirect(Policy::limited(5))
            .timeout(DOWNLOAD_TIMEOUT)
            .build()
            .map_err(|_| {
                "Could not initialize the browser companion download client".to_string()
            })?;
        let checksums = fetch_bounded(
            &client,
            &self.asset.checksums_url,
            MAX_CHECKSUM_BYTES,
            "browser companion checksums",
        )
        .await?;
        let archive = fetch_bounded(
            &client,
            &self.asset.release_url,
            MAX_ARCHIVE_BYTES,
            "browser companion archive",
        )
        .await?;
        self.install_verified_archive(&archive, &checksums)
    }

    pub async fn start(&self) -> Result<BrowserCompanionStatus, String> {
        self.start_checked(|| Ok(())).await
    }
    pub(super) async fn start_checked(
        &self,
        allowed: impl Fn() -> Result<(), String>,
    ) -> Result<BrowserCompanionStatus, String> {
        allowed()?;
        if !self.config.enabled {
            return Err("Browser automation is disabled".into());
        }
        let status = self.status().await;
        match status.state {
            BrowserLifecycleState::Ready
            | BrowserLifecycleState::Busy
            | BrowserLifecycleState::AuthenticationRequired
            | BrowserLifecycleState::AccountChanged => return Ok(status),
            BrowserLifecycleState::Stopped => {}
            BrowserLifecycleState::NotInstalled => {
                return Err(
                    "Browser companion is not installed; run `codexify browser install`".into(),
                );
            }
            BrowserLifecycleState::Disabled => return Err("Browser automation is disabled".into()),
            BrowserLifecycleState::Incompatible | BrowserLifecycleState::Unavailable => {
                return Err(status
                    .detail
                    .unwrap_or_else(|| "Browser companion is not safe to start".into()));
            }
        }
        let runtime = self
            .installed_runtime()?
            .ok_or_else(|| "Browser companion is not installed".to_string())?;
        self.remove_stale_descriptor()?;
        ensure_private_directory(&self.layout.root).map_err(|error| format!("{error:#}"))?;
        let log =
            private_append_file(&self.layout.log_path).map_err(|error| format!("{error:#}"))?;
        let stderr = log
            .try_clone()
            .map_err(|_| "Could not duplicate the browser companion log handle".to_string())?;
        let mut command = StdCommand::new(&runtime.entrypoint);
        command
            .current_dir(
                self.layout
                    .runtime_root
                    .join(&runtime.manifest.codexify_version),
            )
            .env("CODEXIFY_BROWSER_HOME", &self.layout.root)
            .env("CODEXIFY_BROWSER_DESCRIPTOR", &self.layout.descriptor_path)
            .env("CODEXIFY_BROWSER_BUILD", &runtime.manifest.build)
            .env("CODEXIFY_BROWSER_ACTION_LANES", self.config.browser_action_lanes.to_string())
            .env("CODEXIFY_BROWSER_MINIMAL_UI", if self.config.minimal_ui { "1" } else { "0" })
            .env("CODEXIFY_BROWSER_SIMPLICITY", self.config.effective_browser_simplicity().as_str())
            .env("CODEXIFY_BROWSER_RECOVERY_ENHANCEMENTS", if self.config.recovery_enhancements { "1" } else { "0" })
            .env(
                "CODEXIFY_BROWSER_NAVIGATION_TIMINGS",
                serde_json::json!({
                    "conversationLoadIntervalSeconds": self.config.retry_timings.conversation_load_interval_seconds,
                    "conversationUnavailableCooldownSeconds": self.config.retry_timings.conversation_unavailable_cooldown_seconds,
                    "rateLimitCooldownSeconds": self.config.retry_timings.rate_limit_cooldown_seconds,
                }).to_string(),
            )
            .env(
                "CODEXIFY_BROWSER_REQUEST_BLOCK_LIST",
                serde_json::to_string(self.config.effective_request_block_list())
                    .map_err(|_| "Could not encode browser request block list")?,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        configure_detached_process(&mut command);
        let mut child = command
            .spawn()
            .map_err(|_| "Could not start the installed browser companion".to_string())?;
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Err(error) = allowed() {
                if let Ok(executable) =
                    process_executable(child.id()).and_then(|p| p.canonicalize())
                {
                    let _ =
                        stop_process_tree(child.id(), executable, &self.layout.runtime_root).await;
                } else {
                    cleanup_started_child(child.id(), &self.layout.runtime_root).await;
                }
                return Err(error);
            }
            if let Some(exit) = child
                .try_wait()
                .map_err(|_| "Could not inspect the browser companion process".to_string())?
            {
                return Err(format!(
                    "Browser companion exited before becoming ready ({exit})"
                ));
            }
            let status = self.status().await;
            if matches!(
                status.state,
                BrowserLifecycleState::Ready
                    | BrowserLifecycleState::Busy
                    | BrowserLifecycleState::AuthenticationRequired
                    | BrowserLifecycleState::AccountChanged
            ) {
                return Ok(status);
            }
            if status.state == BrowserLifecycleState::Unavailable
                && let Ok(client) = BrowserCompanionClient::from_descriptor_path_with_timeout(
                    &self.layout.descriptor_path,
                    Duration::from_secs(2),
                )
                && client
                    .liveness(&tokio_util::sync::CancellationToken::new())
                    .await
                    == Ok(Some(true))
            {
                return Ok(status);
            }
            if status.state == BrowserLifecycleState::Incompatible
                && self.layout.descriptor_path.exists()
            {
                cleanup_started_child(child.id(), &self.layout.runtime_root).await;
                return Err(status.detail.unwrap_or_else(|| {
                    "Browser companion failed its startup verification".into()
                }));
            }
            if Instant::now() >= deadline {
                cleanup_started_child(child.id(), &self.layout.runtime_root).await;
                return Err(
                    "Browser companion did not become ready before the startup deadline".into(),
                );
            }
            tokio::time::sleep(LIFECYCLE_POLL_INTERVAL).await;
        }
    }

    pub async fn stop(&self) -> Result<BrowserCompanionStatus, String> {
        let client = match BrowserCompanionClient::from_descriptor_path_with_timeout(
            &self.layout.descriptor_path,
            Duration::from_secs(2),
        ) {
            Ok(client) => client,
            Err(message) if descriptor_means_stopped(&message) => {
                if let Some((pid, executable)) = orphaned_browser(
                    &self.layout.root.join("chrome-profile"),
                    &self.layout.runtime_root,
                )? {
                    self.verify_runtime_path(&executable)?;
                    stop_process_tree(pid, executable, &self.layout.runtime_root).await?;
                }
                self.remove_stale_descriptor()?;
                return Ok(self.status().await);
            }
            Err(message) => return Err(message),
        };
        let health = client
            .health(&tokio_util::sync::CancellationToken::new())
            .await;
        let pid = client.pid();
        let executable = match process_executable(pid) {
            Ok(path) => path
                .canonicalize()
                .map_err(|_| "Could not verify the browser companion executable")?,
            Err(error) if health.is_err() && matches!(error.raw_os_error(), Some(2 | 3)) => {
                // A dead/zombie launcher may leave a valid-looking PID descriptor.
                if let Some((pid, executable)) = orphaned_browser(
                    &self.layout.root.join("chrome-profile"),
                    &self.layout.runtime_root,
                )? {
                    self.verify_runtime_path(&executable)?;
                    stop_process_tree(pid, executable, &self.layout.runtime_root).await?;
                }
                self.remove_stale_descriptor()?;
                return Ok(self.status().await);
            }
            Err(_) => return Err("Could not verify the browser companion executable".into()),
        };
        if let Err(error) = health {
            // A frozen HTTP server cannot authenticate a stop. Its OS executable
            // must instead match the manifest of this private managed runtime.
            if !self
                .managed_executable(client.build())
                .is_ok_and(|expected| expected == executable)
            {
                return Err(format!(
                    "{error}; could not verify a managed browser companion process"
                ));
            }
        }
        stop_process_tree(pid, executable, &self.layout.runtime_root).await?;
        if BrowserCompanionClient::from_descriptor_path(&self.layout.descriptor_path)
            .is_ok_and(|current| current.pid() != pid)
        {
            return Err("Browser companion changed while stopping".into());
        }
        self.remove_stale_descriptor()?;
        Ok(self.status().await)
    }

    pub async fn login(&self) -> Result<BrowserCompanionStatus, String> {
        self.start().await?;
        let client = BrowserCompanionClient::from_descriptor_path(&self.layout.descriptor_path)?;
        let health = client
            .login(&tokio_util::sync::CancellationToken::new())
            .await?;
        if let Some(fingerprint) = health.account_fingerprint.as_deref() {
            self.record_account_fingerprint(fingerprint)?;
        }
        Ok(self.status().await)
    }

    pub async fn reveal(&self) -> Result<BrowserCompanionStatus, String> {
        self.start().await?;
        let client = BrowserCompanionClient::from_descriptor_path(&self.layout.descriptor_path)?;
        client
            .reveal(
                &super::RevealWindowRequest { url: None },
                &tokio_util::sync::CancellationToken::new(),
            )
            .await?;
        Ok(self.status().await)
    }

    pub async fn status(&self) -> BrowserCompanionStatus {
        if !self.config.enabled {
            return self.status_value(
                BrowserLifecycleState::Disabled,
                false,
                false,
                false,
                None,
                None,
                Some("Agent-chat browser automation is disabled".into()),
            );
        }
        let runtime = match self.installed_runtime() {
            Ok(Some(runtime)) => runtime,
            Ok(None) => {
                return self.status_value(
                    BrowserLifecycleState::NotInstalled,
                    false,
                    false,
                    false,
                    None,
                    None,
                    Some("The browser companion runtime is not installed".into()),
                );
            }
            Err(message) => {
                return self.status_value(
                    BrowserLifecycleState::Incompatible,
                    true,
                    false,
                    false,
                    None,
                    None,
                    Some(message),
                );
            }
        };

        let client = match BrowserCompanionClient::from_descriptor_path_with_timeout(
            &self.layout.descriptor_path,
            Duration::from_secs(2),
        ) {
            Ok(client) => client,
            Err(message) if descriptor_means_stopped(&message) => {
                return self.status_value(
                    BrowserLifecycleState::Stopped,
                    true,
                    false,
                    false,
                    Some(runtime.manifest.build),
                    Some(runtime.entrypoint),
                    Some("The installed browser companion is not running".into()),
                );
            }
            Err(message) if descriptor_means_incompatible(&message) => {
                return self.status_value(
                    BrowserLifecycleState::Incompatible,
                    true,
                    false,
                    false,
                    Some(runtime.manifest.build),
                    Some(runtime.entrypoint),
                    Some(message),
                );
            }
            Err(message) => {
                return self.status_value(
                    BrowserLifecycleState::Unavailable,
                    true,
                    false,
                    false,
                    Some(runtime.manifest.build),
                    Some(runtime.entrypoint),
                    Some(message),
                );
            }
        };
        let health = match client
            .health(&tokio_util::sync::CancellationToken::new())
            .await
        {
            Ok(health) => health,
            Err(message) if descriptor_means_incompatible(&message) => {
                return self.status_value(
                    BrowserLifecycleState::Incompatible,
                    true,
                    true,
                    false,
                    Some(runtime.manifest.build),
                    Some(runtime.entrypoint),
                    Some(message),
                );
            }
            Err(message) => {
                return self.status_value(
                    BrowserLifecycleState::Unavailable,
                    true,
                    true,
                    false,
                    Some(runtime.manifest.build),
                    Some(runtime.entrypoint),
                    Some(message),
                );
            }
        };
        match self.account_fingerprint() {
            Some(Ok(expected))
                if health
                    .account_fingerprint
                    .as_deref()
                    .is_some_and(|current| current != expected) =>
            {
                return self.status_value(
                    BrowserLifecycleState::AccountChanged,
                    true,
                    true,
                    health.authenticated,
                    Some(runtime.manifest.build),
                    Some(runtime.entrypoint),
                    Some("The authenticated ChatGPT account changed; refresh capabilities and verify pairings again".into()),
                );
            }
            Some(Ok(_)) | None => {}
            Some(Err(message)) => {
                return self.status_value(
                    BrowserLifecycleState::Unavailable,
                    true,
                    true,
                    health.authenticated,
                    Some(runtime.manifest.build),
                    Some(runtime.entrypoint),
                    Some(message),
                );
            }
        }
        let state =
            if health.active_action_id.is_some() || health.status == CompanionSessionStatus::Busy {
                BrowserLifecycleState::Busy
            } else if health.status == CompanionSessionStatus::Unavailable {
                BrowserLifecycleState::Unavailable
            } else if !health.authenticated
                || health.status == CompanionSessionStatus::AuthenticationRequired
            {
                BrowserLifecycleState::AuthenticationRequired
            } else if health.status == CompanionSessionStatus::Idle {
                BrowserLifecycleState::Ready
            } else {
                BrowserLifecycleState::Unavailable
            };
        self.status_value(
            state,
            true,
            true,
            health.authenticated,
            Some(runtime.manifest.build),
            Some(runtime.entrypoint),
            None,
        )
        .with_active_action(health.active_action_id)
    }

    pub fn record_account_fingerprint(&self, fingerprint: &str) -> Result<(), String> {
        if !valid_opaque(fingerprint) {
            return Err("Browser companion account fingerprint is invalid".into());
        }
        if !self.config.enabled {
            return Err("Browser automation is disabled".into());
        }
        ensure_private_directory(&self.layout.root).map_err(|error| format!("{error:#}"))?;
        let record = BrowserAccountRecord {
            version: 1,
            fingerprint: fingerprint.to_string(),
        };
        let bytes = serde_json::to_vec(&record)
            .map_err(|_| "Could not encode browser companion account state".to_string())?;
        atomic_private_write(&self.layout.account_path, &bytes)
            .map_err(|error| format!("{error:#}"))
    }

    #[doc(hidden)]
    pub fn install_verified_archive(
        &self,
        archive: &[u8],
        checksums: &[u8],
    ) -> Result<BrowserInstallReceipt, String> {
        self.install_verified_archive_inner(archive, checksums)
            .map_err(|error| format!("{error:#}"))
    }

    fn install_verified_archive_inner(
        &self,
        archive: &[u8],
        checksums: &[u8],
    ) -> anyhow::Result<BrowserInstallReceipt> {
        if !self.config.enabled {
            bail!(
                "Browser automation is disabled; enable agentChat.automation before installing the companion"
            );
        }
        let expected = checksum_for(checksums, &self.asset.archive_name)?;
        let actual = sha256_hex(archive);
        if actual != expected {
            bail!("Browser companion archive failed SHA-256 verification");
        }

        ensure_private_directory(&self.layout.root)?;
        ensure_private_directory(&self.layout.runtime_root)?;
        let nonce = random_id()?;
        let staging = self.layout.runtime_root.join(format!(".install-{nonce}"));
        create_private_directory(&staging)?;
        let package_root_name = package_root_name(&self.asset);
        let extraction = match self.asset.kind {
            BrowserArchiveKind::TarGz => extract_tar_archive(archive, &staging, &package_root_name),
            BrowserArchiveKind::Zip => extract_zip_archive(archive, &staging, &package_root_name),
        };
        if let Err(error) = extraction {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }

        let package_root = staging.join(&package_root_name);
        let publication = (|| {
            validate_single_package_root(&staging, &package_root_name)?;
            let manifest = read_manifest(&package_root.join(MANIFEST_FILE))?;
            validate_manifest(
                &manifest,
                &package_root,
                &self.asset.version,
                &self.asset.platform,
                &self.expected_build,
            )?;
            publish_runtime(
                &self.layout.runtime_root,
                &package_root,
                &self.layout.runtime_dir,
                &nonce,
            )?;
            let installed_entrypoint = self.layout.runtime_dir.join(&manifest.entrypoint);
            Ok(BrowserInstallReceipt {
                version: manifest.codexify_version,
                build: manifest.build,
                platform: manifest.platform,
                runtime_dir: self.layout.runtime_dir.clone(),
                entrypoint: installed_entrypoint,
                archive_sha256: actual,
            })
        })();
        let _ = fs::remove_dir_all(&staging);
        publication
    }

    fn installed_runtime(&self) -> Result<Option<InstalledBrowserRuntime>, String> {
        if let Some(runtime) = self.current_runtime()? {
            return Ok(Some(runtime));
        }
        let mut versions = fs::read_dir(&self.layout.runtime_root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| Version::parse(name).ok())
            })
            .collect::<Vec<_>>();
        versions.sort();
        for version in versions.into_iter().rev() {
            let build = format!("{version}{PROTOCOL_BUILD_SUFFIX}");
            let root = self.layout.runtime_root.join(version.to_string());
            if let Ok(entrypoint) = self.managed_executable(&build) {
                let manifest =
                    read_manifest(&root.join(MANIFEST_FILE)).map_err(|e| e.to_string())?;
                return Ok(Some(InstalledBrowserRuntime {
                    manifest,
                    entrypoint,
                }));
            }
        }
        Ok(None)
    }
    fn verify_runtime_path(&self, path: &Path) -> Result<(), String> {
        let root = self
            .layout
            .runtime_root
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let version = path
            .strip_prefix(root)
            .ok()
            .and_then(|p| p.components().next())
            .and_then(|p| p.as_os_str().to_str())
            .ok_or("Process is outside its managed runtime")?;
        self.managed_executable(&format!("{version}{PROTOCOL_BUILD_SUFFIX}"))
            .map(|_| ())
    }
    fn managed_executable(&self, build: &str) -> Result<PathBuf, String> {
        let version = build
            .strip_suffix(PROTOCOL_BUILD_SUFFIX)
            .and_then(|version| Version::parse(version).ok())
            .ok_or("Browser companion build is incompatible")?;
        let root = self.layout.runtime_root.join(version.to_string());
        if !fs::symlink_metadata(&root).is_ok_and(|m| m.file_type().is_dir()) {
            return Err("Browser runtime is not a real directory".into());
        }
        let manifest = read_manifest(&root.join(MANIFEST_FILE)).map_err(|e| e.to_string())?;
        let entrypoint = validate_manifest(
            &manifest,
            &root,
            &version.to_string(),
            &self.asset.platform,
            build,
        )
        .map_err(|e| e.to_string())?
        .canonicalize()
        .map_err(|e| e.to_string())?;
        let runtime_root = self
            .layout
            .runtime_root
            .canonicalize()
            .map_err(|e| e.to_string())?;
        if !entrypoint.starts_with(runtime_root) {
            return Err("Browser executable is outside its managed runtime".into());
        }
        Ok(entrypoint)
    }
    fn current_runtime(&self) -> Result<Option<InstalledBrowserRuntime>, String> {
        let metadata = match fs::symlink_metadata(&self.layout.runtime_dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("Browser companion runtime is unavailable".into()),
        };
        if !metadata.file_type().is_dir() {
            return Err("Browser companion runtime is not a real directory".into());
        }
        let manifest = read_manifest(&self.layout.manifest_path)
            .and_then(|manifest| {
                validate_manifest(
                    &manifest,
                    &self.layout.runtime_dir,
                    &self.asset.version,
                    &self.asset.platform,
                    &self.expected_build,
                )
                .map(|entrypoint| InstalledBrowserRuntime {
                    manifest,
                    entrypoint,
                })
            })
            .map_err(|error| format!("{error:#}"))?;
        Ok(Some(manifest))
    }

    fn account_fingerprint(&self) -> Option<Result<String, String>> {
        let metadata = match fs::symlink_metadata(&self.layout.account_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => return Some(Err("Browser companion account state is unavailable".into())),
        };
        if !metadata.file_type().is_file() || metadata.len() > MAX_MANIFEST_BYTES {
            return Some(Err("Browser companion account state is invalid".into()));
        }
        let bytes = match fs::read(&self.layout.account_path) {
            Ok(bytes) if bytes.len() as u64 <= MAX_MANIFEST_BYTES => bytes,
            _ => return Some(Err("Browser companion account state is invalid".into())),
        };
        let record: BrowserAccountRecord = match serde_json::from_slice(&bytes) {
            Ok(record) => record,
            Err(_) => return Some(Err("Browser companion account state is invalid".into())),
        };
        if record.version != 1 || !valid_opaque(&record.fingerprint) {
            return Some(Err("Browser companion account state is invalid".into()));
        }
        Some(Ok(record.fingerprint))
    }

    fn remove_stale_descriptor(&self) -> Result<(), String> {
        let metadata = match fs::symlink_metadata(&self.layout.descriptor_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err("Browser companion descriptor is unavailable".into()),
        };
        if !metadata.file_type().is_file() {
            return Err("Browser companion descriptor must be a regular file".into());
        }
        fs::remove_file(&self.layout.descriptor_path)
            .map_err(|_| "Could not remove the stale browser companion descriptor".to_string())
    }

    #[allow(clippy::too_many_arguments)]
    fn status_value(
        &self,
        state: BrowserLifecycleState,
        installed: bool,
        running: bool,
        authenticated: bool,
        build: Option<String>,
        entrypoint: Option<PathBuf>,
        detail: Option<String>,
    ) -> BrowserCompanionStatus {
        BrowserCompanionStatus {
            state,
            enabled: self.config.enabled,
            installed,
            running,
            authenticated,
            active_action_id: None,
            version: build
                .as_deref()
                .and_then(|b| b.strip_suffix(PROTOCOL_BUILD_SUFFIX))
                .unwrap_or(&self.asset.version)
                .to_owned(),
            build: build.clone(),
            platform: self.asset.platform.clone(),
            runtime_dir: entrypoint.map(|_| self.layout.runtime_dir.clone()),
            descriptor_path: self.layout.descriptor_path.clone(),
            log_path: self.layout.log_path.clone(),
            detail,
        }
    }
}

impl BrowserCompanionStatus {
    fn with_active_action(mut self, action_id: Option<String>) -> Self {
        self.active_action_id = action_id;
        self
    }
}

fn release_asset(
    version: &str,
    os: &str,
    arch: &str,
    release_root: &str,
) -> Result<BrowserReleaseAsset, String> {
    let (platform, kind, extension) = match (os, arch) {
        ("linux", "x86_64") => ("linux-x64", BrowserArchiveKind::TarGz, "tar.gz"),
        ("linux", "aarch64") => ("linux-arm64", BrowserArchiveKind::TarGz, "tar.gz"),
        ("macos", "x86_64") => ("darwin-x64", BrowserArchiveKind::TarGz, "tar.gz"),
        ("macos", "aarch64") => ("darwin-arm64", BrowserArchiveKind::TarGz, "tar.gz"),
        ("windows", "x86_64" | "aarch64") => ("windows-x64", BrowserArchiveKind::Zip, "zip"),
        _ => {
            return Err(format!(
                "Codexify has no browser companion artifact for {os}-{arch}"
            ));
        }
    };
    let root = release_root.trim_end_matches('/');
    if root.is_empty() || !root.contains("://") {
        return Err("Browser companion release root is invalid".into());
    }
    let tag = format!("v{version}");
    let archive_name = format!("codexify-browser-{tag}-{platform}.{extension}");
    Ok(BrowserReleaseAsset {
        tag: tag.clone(),
        version: version.to_string(),
        platform: platform.to_string(),
        archive_name: archive_name.clone(),
        kind,
        release_url: format!("{root}/{tag}/{archive_name}"),
        checksums_url: format!("{root}/{tag}/checksums.txt"),
    })
}

fn package_root_name(asset: &BrowserReleaseAsset) -> String {
    asset
        .archive_name
        .strip_suffix(".tar.gz")
        .or_else(|| asset.archive_name.strip_suffix(".zip"))
        .unwrap_or(&asset.archive_name)
        .to_string()
}

fn checksum_for(checksums: &[u8], archive_name: &str) -> anyhow::Result<String> {
    let text =
        std::str::from_utf8(checksums).context("Browser companion checksums are not UTF-8")?;
    let mut matches = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(hash) = fields.next() else {
            continue;
        };
        let Some(name) = fields.next() else {
            continue;
        };
        if fields.next().is_some() || name.trim_start_matches('*') != archive_name {
            continue;
        }
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("checksums.txt contains an invalid SHA-256 for {archive_name}");
        }
        matches.push(hash.to_ascii_lowercase());
    }
    match matches.as_slice() {
        [hash] => Ok(hash.clone()),
        [] => bail!("checksums.txt does not contain {archive_name}"),
        _ => bail!("checksums.txt contains duplicate entries for {archive_name}"),
    }
}

fn extract_tar_archive(bytes: &[u8], destination: &Path, package_root: &str) -> anyhow::Result<()> {
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = Archive::new(decoder);
    let mut entries = 0usize;
    let mut expanded = 0u64;
    for entry in archive
        .entries()
        .context("read browser companion tar archive")?
    {
        let mut entry = entry.context("read browser companion tar entry")?;
        entries += 1;
        if entries > MAX_ARCHIVE_ENTRIES {
            bail!("Browser companion archive contains too many entries");
        }
        let relative = safe_archive_path(
            &entry
                .path()
                .context("read browser companion tar entry path")?,
            package_root,
        )?;
        let target = destination.join(&relative);
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            create_private_directory(&target)?;
            continue;
        }
        if entry_type.is_symlink() {
            let link_target = entry
                .link_name()
                .context("read browser companion tar symlink target")?
                .context("Browser companion tar symlink target is missing")?;
            validate_archive_symlink_target(&relative, &link_target, package_root)?;
            write_archive_symlink(&target, &link_target)?;
            continue;
        }
        if !entry_type.is_file() {
            bail!("Browser companion archive contains an unsupported entry type");
        }
        let size = entry.size();
        expanded = expanded
            .checked_add(size)
            .context("Browser companion archive expanded size overflow")?;
        if size > MAX_ARCHIVE_FILE_BYTES || expanded > MAX_ARCHIVE_EXPANDED_BYTES {
            bail!("Browser companion archive exceeds its expanded size limit");
        }
        let executable = entry.header().mode().unwrap_or(0) & 0o111 != 0;
        write_archive_file(&target, &mut entry, size, executable)?;
    }
    Ok(())
}

fn extract_zip_archive(bytes: &[u8], destination: &Path, package_root: &str) -> anyhow::Result<()> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).context("open browser companion ZIP")?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!("Browser companion archive contains too many entries");
    }
    let mut expanded = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("read browser companion ZIP entry {index}"))?;
        let enclosed = entry
            .enclosed_name()
            .context("Browser companion ZIP contains an unsafe path")?
            .to_path_buf();
        let relative = safe_archive_path(&enclosed, package_root)?;
        let target = destination.join(&relative);
        if entry.is_dir() {
            create_private_directory(&target)?;
            continue;
        }
        if !entry.is_file()
            || entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            bail!("Browser companion ZIP contains an unsupported entry type");
        }
        let size = entry.size();
        expanded = expanded
            .checked_add(size)
            .context("Browser companion archive expanded size overflow")?;
        if size > MAX_ARCHIVE_FILE_BYTES || expanded > MAX_ARCHIVE_EXPANDED_BYTES {
            bail!("Browser companion archive exceeds its expanded size limit");
        }
        let executable = entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0);
        write_archive_file(&target, &mut entry, size, executable)?;
    }
    Ok(())
}

fn safe_archive_path(path: &Path, package_root: &str) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        bail!("Browser companion archive contains an absolute path");
    }
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => output.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("Browser companion archive contains an unsafe path")
            }
        }
    }
    if output.as_os_str().is_empty()
        || output
            .components()
            .next()
            .and_then(|component| match component {
                Component::Normal(value) => value.to_str(),
                _ => None,
            })
            != Some(package_root)
    {
        bail!("Browser companion archive has an unexpected package root");
    }
    Ok(output)
}

fn validate_archive_symlink_target(
    link_path: &Path,
    target: &Path,
    package_root: &str,
) -> anyhow::Result<()> {
    let text = target
        .to_str()
        .context("Browser companion archive symlink target is not UTF-8")?;
    if text.is_empty() || text.len() > 4096 || target.is_absolute() {
        bail!("Browser companion archive symlink target is invalid");
    }
    let mut resolved = link_path
        .parent()
        .context("Browser companion archive symlink has no parent")?
        .to_path_buf();
    for component in target.components() {
        match component {
            Component::Normal(value) => resolved.push(value),
            Component::CurDir => {}
            Component::ParentDir => {
                if resolved.components().count() <= 1 {
                    bail!("Browser companion archive symlink escapes the package root");
                }
                resolved.pop();
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("Browser companion archive symlink target is invalid")
            }
        }
    }
    if resolved
        .components()
        .next()
        .and_then(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        != Some(package_root)
    {
        bail!("Browser companion archive symlink escapes the package root");
    }
    Ok(())
}

fn write_archive_symlink(path: &Path, target: &Path) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .context("Browser companion archive symlink has no parent")?;
    ensure_private_directory(parent)?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, path).with_context(|| {
            format!(
                "create browser companion archive symlink {}",
                path.display()
            )
        })?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = target;
        bail!("Browser companion tar symlinks are unsupported on this platform")
    }
}

fn write_archive_file(
    path: &Path,
    input: &mut impl Read,
    declared_size: u64,
    executable: bool,
) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .context("Browser companion archive entry has no parent")?;
    ensure_private_directory(parent)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("create browser companion archive entry {}", path.display()))?;
    let copied = std::io::copy(&mut input.take(MAX_ARCHIVE_FILE_BYTES + 1), &mut file)
        .with_context(|| format!("extract browser companion archive entry {}", path.display()))?;
    if copied != declared_size || copied > MAX_ARCHIVE_FILE_BYTES {
        bail!("Browser companion archive entry size is invalid");
    }
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }),
        )?;
    }
    Ok(())
}

fn validate_single_package_root(staging: &Path, expected: &str) -> anyhow::Result<()> {
    let mut entries = fs::read_dir(staging).context("list extracted browser companion package")?;
    let entry = entries
        .next()
        .transpose()?
        .context("Browser companion archive is empty")?;
    if entries.next().transpose()?.is_some()
        || entry.file_name() != OsStr::new(expected)
        || !entry.file_type()?.is_dir()
    {
        bail!("Browser companion archive has an unexpected package root");
    }
    Ok(())
}

fn read_manifest(path: &Path) -> anyhow::Result<BrowserRuntimeManifest> {
    let metadata = fs::symlink_metadata(path).context("locate browser companion manifest")?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_MANIFEST_BYTES {
        bail!("Browser companion manifest is unavailable or oversized");
    }
    let mut file = File::open(path).context("open browser companion manifest")?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read browser companion manifest")?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        bail!("Browser companion manifest is oversized");
    }
    serde_json::from_slice(&bytes).context("parse browser companion manifest")
}

fn validate_manifest(
    manifest: &BrowserRuntimeManifest,
    package_root: &Path,
    version: &str,
    platform: &str,
    expected_build: &str,
) -> anyhow::Result<PathBuf> {
    if manifest.version != MANIFEST_VERSION {
        bail!("Browser companion manifest version is incompatible");
    }
    if manifest.codexify_version != version {
        bail!("Browser companion manifest targets a different Codexify version");
    }
    if manifest.protocol_version != PROTOCOL_VERSION
        || manifest.protocol_schema_hash != PROTOCOL_SCHEMA_HASH
    {
        bail!("Browser companion manifest protocol is incompatible");
    }
    if manifest.build != expected_build || !manifest.build.ends_with(PROTOCOL_BUILD_SUFFIX) {
        bail!("Browser companion manifest build is incompatible");
    }
    if manifest.platform != platform {
        bail!("Browser companion manifest targets a different platform");
    }
    let relative = safe_relative_path(&manifest.entrypoint, "Browser companion entrypoint")?;
    if relative == Path::new(MANIFEST_FILE) {
        bail!("Browser companion entrypoint is invalid");
    }
    let entrypoint = package_root.join(relative);
    let metadata =
        fs::symlink_metadata(&entrypoint).context("locate browser companion entrypoint")?;
    if !metadata.file_type().is_file() {
        bail!("Browser companion entrypoint is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("Browser companion entrypoint is not executable");
        }
    }
    Ok(entrypoint)
}

fn publish_runtime(
    runtime_root: &Path,
    package_root: &Path,
    destination: &Path,
    nonce: &str,
) -> anyhow::Result<()> {
    let backup = runtime_root.join(format!(".previous-{nonce}"));
    let had_previous = match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() {
                bail!("Existing browser companion runtime is not a directory");
            }
            fs::rename(destination, &backup).context("stage existing browser companion runtime")?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("inspect existing browser companion runtime"),
    };
    if let Err(error) = fs::rename(package_root, destination) {
        if had_previous {
            let _ = fs::rename(&backup, destination);
        }
        return Err(error).context("publish browser companion runtime");
    }
    if had_previous {
        fs::remove_dir_all(&backup).context("remove previous browser companion runtime")?;
    }
    #[cfg(unix)]
    File::open(runtime_root)?.sync_all()?;
    Ok(())
}

fn safe_relative_path(value: &str, label: &str) -> anyhow::Result<PathBuf> {
    if value.is_empty() || value.contains('\0') {
        bail!("{label} is invalid");
    }
    let path = Path::new(value);
    if path.is_absolute() {
        bail!("{label} must be relative");
    }
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => output.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("{label} contains an unsafe path component")
            }
        }
    }
    if output.as_os_str().is_empty() {
        bail!("{label} is invalid");
    }
    Ok(output)
}

fn expand_descriptor_path(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    let path = if let Some(relative) = trimmed.strip_prefix("~/") {
        home_dir()
            .ok_or("Browser companion descriptor path requires a home directory")?
            .join(relative)
    } else {
        PathBuf::from(trimmed)
    };
    if !path.is_absolute()
        || trimmed == "~"
        || trimmed.starts_with('~') && !trimmed.starts_with("~/")
    {
        return Err("Browser companion descriptor path must be absolute or start with ~/".into());
    }
    Ok(path)
}

fn create_private_directory(path: &Path) -> anyhow::Result<()> {
    fs::create_dir(path).with_context(|| format!("create private directory {}", path.display()))?;
    set_private_directory_permissions(path)
}

pub(super) fn ensure_private_directory(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)
            .with_context(|| format!("create private directory {}", path.display()))?;
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect private directory {}", path.display()))?;
    if !metadata.file_type().is_dir() {
        bail!(
            "Browser companion path is not a real directory: {}",
            path.display()
        );
    }
    set_private_directory_permissions(path)
}

fn set_private_directory_permissions(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(super) fn atomic_private_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .context("Browser companion private file has no parent directory")?;
    ensure_private_directory(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && !metadata.file_type().is_file()
    {
        bail!("Browser companion private state must be a regular file");
    }
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("browser-state"),
        random_id()?
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("create private file {}", temporary.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        }
        replace_file(&temporary, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> anyhow::Result<()> {
    fs::rename(source, destination).context("publish browser companion private state")
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .context("publish browser companion private state");
    }
    Ok(())
}

fn descriptor_means_stopped(message: &str) -> bool {
    message.contains("descriptor is unavailable")
        || message.contains("descriptor directory is unavailable")
        || message.contains("descriptor process is not running")
}

fn descriptor_means_incompatible(message: &str) -> bool {
    [
        "protocol",
        "build",
        "kind",
        "endpoint",
        "bearer secret",
        "invalid JSON",
        "timestamp",
        "private to the current user",
        "regular non-symlink",
        "real directory",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn valid_opaque(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

async fn fetch_bounded(
    client: &reqwest::Client,
    url: &str,
    maximum: usize,
    label: &str,
) -> Result<Vec<u8>, String> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| format!("Could not download {label}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Could not download {label}: HTTP {}",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(format!("Downloaded {label} exceeds its size limit"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| format!("Could not read downloaded {label}"))?
    {
        if bytes.len().saturating_add(chunk.len()) > maximum {
            return Err(format!("Downloaded {label} exceeds its size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn private_append_file(path: &Path) -> anyhow::Result<File> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && !metadata.file_type().is_file()
    {
        bail!("Browser companion log must be a regular file");
    }
    let parent = path
        .parent()
        .context("Browser companion log has no parent directory")?;
    ensure_private_directory(parent)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("open browser companion log {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(unix)]
fn configure_detached_process(command: &mut StdCommand) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn configure_detached_process(command: &mut StdCommand) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn configure_detached_process(_command: &mut StdCommand) {}

#[cfg(unix)]
fn terminate_process(pid: u32, force: bool) -> Result<(), String> {
    if pid > i32::MAX as u32 {
        return Err("Browser companion PID is invalid".into());
    }
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    if unsafe { libc::kill(pid as i32, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err("Could not stop the browser companion process".into())
}

#[cfg(windows)]
fn terminate_process(pid: u32, _force: bool) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        TerminateProcess,
    };

    let handle = unsafe {
        OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if handle.is_null() {
        if std::io::Error::last_os_error().raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Ok(());
        }
        return Err("Could not open the browser companion process".into());
    }
    let terminated = unsafe { TerminateProcess(handle, 0) };
    // Chrome or Electron may exit between identity verification and Stop.
    // Windows reports access denied for TerminateProcess on an exited process;
    // that already achieved the desired state and must not block recovery.
    let mut exit_code = STILL_ACTIVE as u32;
    let already_exited = terminated == 0
        && unsafe { GetExitCodeProcess(handle, &mut exit_code) } != 0
        && exit_code != STILL_ACTIVE as u32;
    unsafe { CloseHandle(handle) };
    if terminated == 0 && !already_exited {
        return Err("Could not stop the browser companion process".into());
    }
    Ok(())
}

#[cfg(all(test, windows))]
#[test]
fn stopping_an_already_exited_browser_process_succeeds() {
    let mut command = StdCommand::new("cmd.exe");
    command.args(["/C", "exit", "0"]);
    configure_detached_process(&mut command);
    let mut child = command.spawn().unwrap();
    child.wait().unwrap();
    terminate_process(child.id(), true).unwrap();
    terminate_process(child.id(), true).unwrap();
}

#[cfg(not(any(unix, windows)))]
fn terminate_process(_pid: u32, _force: bool) -> Result<(), String> {
    Err("Browser companion process control is unsupported on this platform".into())
}

async fn stop_process_tree(
    pid: u32,
    executable: PathBuf,
    runtime_root: &Path,
) -> Result<(), String> {
    let mut owned = managed_descendants(pid, runtime_root)?;
    owned.push((pid, executable));
    terminate_process(pid, false)?;
    let deadline = Instant::now() + STOP_TIMEOUT;
    while owned
        .iter()
        .any(|(pid, executable)| same_process(*pid, executable))
        && Instant::now() < deadline
    {
        tokio::time::sleep(LIFECYCLE_POLL_INTERVAL).await;
    }
    // Chrome can outlive a frozen launcher. Stop only previously verified
    // descendants in this managed runtime, never a system Chrome instance.
    for (pid, executable) in &owned {
        if same_process(*pid, executable) {
            terminate_process(*pid, true)?;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while owned
        .iter()
        .any(|(pid, executable)| same_process(*pid, executable))
        && Instant::now() < deadline
    {
        tokio::time::sleep(LIFECYCLE_POLL_INTERVAL).await;
    }
    if owned
        .iter()
        .any(|(pid, executable)| same_process(*pid, executable))
    {
        return Err("Browser companion process did not stop".into());
    }
    Ok(())
}

fn random_id() -> anyhow::Result<String> {
    let mut bytes = [0u8; 12];
    getrandom::getrandom(&mut bytes).map_err(|error| {
        anyhow::anyhow!("generate browser companion install identifier: {error}")
    })?;
    Ok(hex(&bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

async fn cleanup_started_child(pid: u32, root: &Path) {
    if let Ok(executable) = process_executable(pid).and_then(|p| p.canonicalize()) {
        let _ = stop_process_tree(pid, executable, root).await;
    } else {
        let _ = terminate_process(pid, true);
    }
}
