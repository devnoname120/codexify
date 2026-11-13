//! Shared lifecycle control; recovery never submits or replays browser actions.
use super::{BrowserCompanionClient, BrowserCompanionManager, CompanionSessionStatus};
use crate::markdown_chat::AgentAutomationConfig;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions, TryLockError},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex, OnceLock, Weak,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
const STOPPED: &str = "Browser companion was stopped by you. Use Start browser to resume it.";

#[derive(Debug, Default)]
struct Runtime {
    lock: Mutex<()>,
    phase: AtomicU8,
    failures: StdMutex<RecoveryProbeGate>,
}
#[derive(Debug, Default)]
struct RecoveryProbeGate {
    pid: u32,
    first: Option<Instant>,
    last: Option<Instant>,
    count: u32,
}
impl RecoveryProbeGate {
    fn confirmed(&mut self, pid: u32, now: Instant) -> bool {
        if self.pid != pid {
            *self = Self {
                pid,
                ..Self::default()
            };
        }
        let first = *self.first.get_or_insert(now);
        if self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(5))
        {
            self.count = self.count.saturating_add(1);
            self.last = Some(now);
        }
        self.count >= 3 && now.saturating_duration_since(first) >= Duration::from_secs(60)
    }
}

static RUNTIMES: OnceLock<StdMutex<HashMap<PathBuf, Weak<Runtime>>>> = OnceLock::new();

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Desired {
    version: u8,
    automatic_start: bool,
    restart_from_pid: Option<u32>,
    changed_at_ms: u64,
}
impl Default for Desired {
    fn default() -> Self {
        Self {
            version: 1,
            automatic_start: true,
            restart_from_pid: None,
            changed_at_ms: 0,
        }
    }
}
impl Desired {
    fn waiting_for_restart(&self) -> bool {
        self.restart_from_pid == Some(std::process::id())
            && crate::markdown_chat::now_ms().saturating_sub(self.changed_at_ms) < 60_000
    }
}

#[derive(Clone)]
pub struct BrowserCompanionControl {
    config: AgentAutomationConfig,
    manager: BrowserCompanionManager,
    runtime: Arc<Runtime>,
}

struct Phase(Arc<Runtime>, u8);
impl Phase {
    fn new(runtime: &Arc<Runtime>, phase: u8) -> Self {
        runtime.phase.store(phase, Ordering::SeqCst);
        Self(runtime.clone(), phase)
    }
}
impl Drop for Phase {
    fn drop(&mut self) {
        let _ = self
            .0
            .phase
            .compare_exchange(self.1, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

impl BrowserCompanionControl {
    pub fn new(config: &AgentAutomationConfig) -> Result<Self, String> {
        Ok(Self::with_manager(
            config.clone(),
            BrowserCompanionManager::from_config(config)?,
        ))
    }
    #[doc(hidden)]
    pub fn with_manager(config: AgentAutomationConfig, manager: BrowserCompanionManager) -> Self {
        let mut runtimes = RUNTIMES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let key = std::fs::canonicalize(&manager.layout().root)
            .unwrap_or_else(|_| manager.layout().root.clone());
        let runtime = runtimes
            .get(&key)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let runtime = Arc::new(Runtime::default());
                runtimes.insert(key, Arc::downgrade(&runtime));
                runtime
            });
        Self {
            config,
            manager,
            runtime,
        }
    }
    fn state_path(&self) -> PathBuf {
        self.manager.layout().root.join("lifecycle.json")
    }
    fn desired(&self) -> Result<Desired, String> {
        let path = self.state_path();
        if !path.try_exists().map_err(|e| e.to_string())? {
            return Ok(Desired::default());
        }
        let mut file = private_file(&path, false)?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(4097)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > 4096 {
            return Err("Browser lifecycle state is too large".into());
        }
        let desired: Desired =
            serde_json::from_slice(&bytes).map_err(|_| "Browser lifecycle state is invalid")?;
        if desired.version != 1 {
            return Err("Browser lifecycle state is incompatible".into());
        }
        Ok(desired)
    }
    fn save_desired(
        &self,
        automatic_start: bool,
        restart_from_pid: Option<u32>,
    ) -> Result<(), String> {
        super::install::ensure_private_directory(&self.manager.layout().root)
            .map_err(|e| e.to_string())?;
        let value = Desired {
            version: 1,
            automatic_start,
            restart_from_pid,
            changed_at_ms: crate::markdown_chat::now_ms(),
        };
        super::install::atomic_private_write(
            &self.state_path(),
            &serde_json::to_vec(&value).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())
    }
    fn allowed(&self) -> Result<(), String> {
        if !self.config.enabled {
            return Err("Browser automation is disabled".into());
        }
        let desired = self.desired()?;
        if !desired.automatic_start {
            return Err(STOPPED.into());
        }
        if desired.waiting_for_restart() {
            return Err("Browser companion is waiting for Codexify to restart".into());
        }
        Ok(())
    }
    pub fn public_status(&self) -> serde_json::Value {
        let desired = self.desired();
        let phase = self.runtime.phase.load(Ordering::SeqCst);
        serde_json::json!({"manuallyStopped":desired.as_ref().is_ok_and(|s|!s.automatic_start),
            "waitingForServerRestart":desired.as_ref().is_ok_and(Desired::waiting_for_restart),
            "recovering":phase==1,"stopping":phase==2})
    }
    async fn file_lock(&self, cancellation: &CancellationToken) -> Result<File, String> {
        super::install::ensure_private_directory(&self.manager.layout().root)
            .map_err(|e| e.to_string())?;
        let file = private_file(&self.manager.layout().root.join("lifecycle.lock"), true)?;
        let until = Instant::now() + Duration::from_secs(60);
        loop {
            if cancellation.is_cancelled() {
                return Err("Browser lifecycle operation was cancelled".into());
            }
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => return Err(error.to_string()),
            }
            if Instant::now() >= until {
                return Err("Another browser lifecycle operation is still running".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    async fn repair(&self, cancellation: &CancellationToken) -> Result<(), String> {
        self.allowed()?;
        if healthy_client(&self.config, cancellation).await?.is_some() {
            return Ok(());
        }
        // Descriptor validation includes the live PID. A failed HTTP observation
        // is never permission to terminate that process or an uncertain Send.
        if let Ok(client) = BrowserCompanionClient::from_config(&self.config) {
            let unavailable =
                match tokio::time::timeout(HEALTH_TIMEOUT, client.liveness(cancellation)).await {
                    Ok(Ok(Some(false))) => true,
                    Ok(Ok(Some(true))) => false,
                    Ok(Ok(None)) => match tokio::time::timeout(
                        HEALTH_TIMEOUT,
                        client.health(cancellation),
                    )
                    .await
                    {
                        Ok(Ok(health)) => {
                            health.status == CompanionSessionStatus::Unavailable
                                && health.active_action_id.is_none()
                        }
                        _ => return Err(
                            "Browser observation did not complete; the live browser was preserved"
                                .into(),
                        ),
                    },
                    _ => {
                        return Err(
                            "Browser observation did not complete; the live browser was preserved"
                                .into(),
                        );
                    }
                };
            if !unavailable {
                return Ok(());
            }
            let confirmed = self
                .runtime
                .failures
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .confirmed(client.pid(), Instant::now());
            if !confirmed {
                return Err("Browser reported unavailable; retaining its live process while repeated probes settle".into());
            }
        }
        *self
            .runtime
            .failures
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = RecoveryProbeGate::default();
        let _phase = Phase::new(&self.runtime, 1);
        // Stop also verifies executable identity after confirmed recovery evidence.
        self.manager.stop().await?;
        self.allowed()?;
        if cancellation.is_cancelled() {
            return Err("Browser lifecycle operation was cancelled".into());
        }
        self.manager.start_checked(|| self.allowed()).await?;
        Ok(())
    }
    pub async fn ensure_running(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<BrowserCompanionClient, String> {
        self.allowed()?;
        let result =
            companion_client_or_start(&self.config, &self.runtime.lock, cancellation, || async {
                if !self.manager.status().await.installed {
                    return Err(
                        "Browser companion is not installed; run codexify browser install".into(),
                    );
                }
                let _file = self.file_lock(cancellation).await?;
                self.repair(cancellation).await
            })
            .await?;
        self.allowed()?;
        *self
            .runtime
            .failures
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = RecoveryProbeGate::default();
        Ok(result)
    }
    pub async fn start(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<BrowserCompanionClient, String> {
        if !self.config.enabled {
            return Err("Browser automation is disabled".into());
        }
        self.save_desired(true, None)?;
        let _phase = Phase::new(&self.runtime, 1);
        let _guard = self.runtime.lock.lock().await;
        let _file = self.file_lock(cancellation).await?;
        self.repair(cancellation).await?;
        self.allowed()?;
        healthy_client(&self.config, cancellation)
            .await?
            .ok_or_else(|| "Browser companion did not become available".into())
    }
    pub async fn stop(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<super::BrowserCompanionStatus, String> {
        self.save_desired(false, None)?;
        let _phase = Phase::new(&self.runtime, 2);
        let _guard = self.runtime.lock.lock().await;
        let _file = self.file_lock(cancellation).await?;
        self.manager.stop().await
    }
    /// A service maintenance stop must preserve an explicit manual-stop choice.
    #[doc(hidden)]
    pub async fn stop_for_restart(&self) -> Result<(), String> {
        self.save_desired(self.desired()?.automatic_start, Some(std::process::id()))?;
        let _guard = self.runtime.lock.lock().await;
        let _file = self.file_lock(&CancellationToken::new()).await?;
        self.manager.stop().await.map(|_| ())
    }
}

async fn healthy_client(
    config: &AgentAutomationConfig,
    cancellation: &CancellationToken,
) -> Result<Option<BrowserCompanionClient>, String> {
    if cancellation.is_cancelled() {
        return Err("Browser action was cancelled before startup".into());
    }
    let Ok(client) = BrowserCompanionClient::from_config(config) else {
        return Ok(None);
    };
    // New runtimes distinguish a frozen renderer from a slow ChatGPT session API.
    match tokio::time::timeout(HEALTH_TIMEOUT, client.liveness(cancellation)).await {
        Ok(Ok(Some(true))) => return Ok(Some(client)),
        Ok(Ok(None)) => {} // Compatible older runtime, use its existing health route.
        _ => return Ok(None),
    }
    match tokio::time::timeout(HEALTH_TIMEOUT, client.health(cancellation)).await {
        Ok(Ok(health))
            if health.status != CompanionSessionStatus::Unavailable || health.authenticated =>
        {
            Ok(Some(client))
        }
        _ => Ok(None),
    }
}

pub(super) async fn companion_client_or_start<F, Fut>(
    config: &AgentAutomationConfig,
    startup_lock: &Mutex<()>,
    cancellation: &CancellationToken,
    start: F,
) -> Result<BrowserCompanionClient, String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    if let Some(client) = healthy_client(config, cancellation).await? {
        return Ok(client);
    }
    let _guard = tokio::select! {
        guard=startup_lock.lock()=>guard,
        _=cancellation.cancelled()=>return Err("Browser action was cancelled before startup".into()),
    };
    // Let a renderer navigation or short scheduling pause settle before recovery.
    tokio::time::sleep(Duration::from_millis(500)).await;
    if let Some(client) = healthy_client(config, cancellation).await? {
        return Ok(client);
    }
    if cancellation.is_cancelled() {
        return Err("Browser action was cancelled before startup".into());
    }
    start().await?;
    healthy_client(config, cancellation)
        .await?
        .ok_or_else(|| "Browser companion did not become available".into())
}

fn private_file(path: &Path, create: bool) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(create)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err("Browser lifecycle state must be a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err("Browser lifecycle state must be private to the current user".into());
        }
    }
    Ok(file)
}

pub struct BrowserRecoveryWorker(tokio::task::JoinHandle<()>);
impl Drop for BrowserRecoveryWorker {
    fn drop(&mut self) {
        self.0.abort();
    }
}
impl BrowserCompanionControl {
    #[doc(hidden)]
    pub fn watch(&self) -> BrowserRecoveryWorker {
        let control = self.clone();
        BrowserRecoveryWorker(tokio::spawn(async move {
            let mut previous_error = None;
            loop {
                let result = match control.allowed() {
                    Ok(()) => control
                        .ensure_running(&CancellationToken::new())
                        .await
                        .map(|_| ()),
                    Err(error)
                        if error == STOPPED
                            || error.contains("waiting for Codexify to restart") =>
                    {
                        Ok(())
                    }
                    Err(error) => Err(error),
                };
                // Stop can arrive while recovery is running. It is a deliberate state,
                // not a recovery failure that should be logged or retried with backoff.
                let result = match result {
                    Err(error)
                        if error == STOPPED
                            || error.contains("waiting for Codexify to restart") =>
                    {
                        Ok(())
                    }
                    result => result,
                };
                let delay = if let Err(error) = result {
                    if previous_error.as_ref() != Some(&error) {
                        tracing::warn!(%error,"browser companion recovery failed");
                        previous_error = Some(error);
                    }
                    Duration::from_secs(30)
                } else {
                    previous_error = None;
                    Duration::from_secs(5)
                };
                tokio::time::sleep(delay).await;
            }
        }))
    }
}

pub fn watch(config: &AgentAutomationConfig) -> Option<BrowserRecoveryWorker> {
    if !config.enabled {
        return None;
    }
    match BrowserCompanionControl::new(config) {
        Ok(control) => Some(control.watch()),
        Err(error) => {
            tracing::warn!(%error,"browser recovery unavailable");
            None
        }
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    #[test]
    fn slow_or_single_probes_do_not_restart_a_live_browser() {
        let start = Instant::now();
        let mut gate = RecoveryProbeGate::default();
        assert!(!gate.confirmed(10, start));
        for _ in 0..100 {
            assert!(!gate.confirmed(10, start + Duration::from_secs(1)));
        }
        assert!(!gate.confirmed(10, start + Duration::from_secs(30)));
        assert!(!gate.confirmed(10, start + Duration::from_secs(59)));
        assert!(gate.confirmed(10, start + Duration::from_secs(61)));
        assert!(
            !gate.confirmed(11, start + Duration::from_secs(62)),
            "A replacement process needs its own fresh probes"
        );
        gate = RecoveryProbeGate::default();
        assert!(!gate.confirmed(10, start + Duration::from_secs(120)));
    }
}
