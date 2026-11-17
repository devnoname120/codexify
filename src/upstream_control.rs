//! Per-connection admission and failure containment; never retries tool calls.
use crate::types::{McpRequestLimits, McpServerSpec};
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

// Capability is tied to a configured upstream, never inferred from a tab name.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum PlaywrightBackendMode {
    #[default]
    Other,
    Browser,
    Cdp,
    Extension,
    Unverified,
}

impl PlaywrightBackendMode {
    // Playwright accepts --config and PLAYWRIGHT_MCP_CONFIG as well as
    // command-line flags. A hidden extension configuration must not be
    // mistaken for a browser-level backend.
    fn config_document(spec: &McpServerSpec) -> Option<Option<Value>> {
        let path = spec
            .args
            .iter()
            .enumerate()
            .filter_map(|(index, arg)| {
                if arg == "--config" {
                    Some(spec.args.get(index + 1).map_or("", String::as_str))
                } else {
                    arg.strip_prefix("--config=")
                }
            })
            .last()
            .map(str::to_owned)
            .or_else(|| spec.env.get("PLAYWRIGHT_MCP_CONFIG").cloned())
            .or_else(|| std::env::var("PLAYWRIGHT_MCP_CONFIG").ok())?;

        let mut path = std::path::PathBuf::from(path);
        if path.is_relative() {
            if let Some(cwd) = spec.cwd.as_ref() {
                path = std::path::Path::new(cwd).join(path);
            }
        }
        let parsed = std::fs::read_to_string(path)
            .ok()
            .and_then(|source| serde_json::from_str::<Value>(&source).ok());
        Some(parsed)
    }

    fn from_spec(name: &str, spec: &McpServerSpec) -> Self {
        let playwright = name.to_ascii_lowercase().contains("playwright")
            || spec
                .args
                .iter()
                .any(|arg| arg.to_ascii_lowercase().contains("playwright"))
            || spec
                .command
                .as_deref()
                .is_some_and(|command| command.to_ascii_lowercase().contains("playwright"));
        if !playwright {
            return Self::Other;
        }

        let configured = Self::config_document(spec);
        let read_config = configured.as_ref().and_then(Option::as_ref);

        let cli_extension = spec.args.iter().rev().find_map(|arg| match arg.as_str() {
            "--extension" | "--extension=true" => Some(true),
            "--extension=false" => Some(false),
            _ => None,
        });
        let env_extension = spec
            .env
            .get("PLAYWRIGHT_MCP_EXTENSION")
            .cloned()
            .or_else(|| std::env::var("PLAYWRIGHT_MCP_EXTENSION").ok())
            .and_then(|value| match value.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" => Some(true),
                "0" | "false" | "no" => Some(false),
                _ => None,
            });
        let config_extension = read_config
            .and_then(|value| value.get("extension"))
            .and_then(Value::as_bool);
        if cli_extension.or(env_extension).or(config_extension) == Some(true)
            || name.eq_ignore_ascii_case("playwright-extension")
            || name.eq_ignore_ascii_case("playwright_extension")
        {
            return Self::Extension;
        }

        // Failure to read an explicitly configured backend leaves its
        // permissions unknown. Do not dispatch real isolation requests.
        if configured.as_ref().is_some_and(Option::is_none) {
            return Self::Unverified;
        }

        let cdp = spec
            .args
            .iter()
            .any(|arg| arg == "--cdp-endpoint" || arg.starts_with("--cdp-endpoint="))
            || spec.env.contains_key("PLAYWRIGHT_MCP_CDP_ENDPOINT")
            || std::env::var_os("PLAYWRIGHT_MCP_CDP_ENDPOINT").is_some()
            || read_config
                .and_then(|value| value.pointer("/browser/cdpEndpoint"))
                .and_then(Value::as_str)
                .is_some_and(|url| !url.is_empty());
        if cdp { Self::Cdp } else { Self::Browser }
    }
}

#[derive(Default)]
struct State {
    failures: u32,
    open_until: Option<Instant>,
    probe_active: bool,
    last_outcome: Option<&'static str>,
}

pub(crate) struct UpstreamControl {
    limits: McpRequestLimits,
    playwright_mode: PlaywrightBackendMode,
    active: Semaphore,
    pending: Semaphore,
    state: Mutex<State>,
    unavailable: tokio::sync::watch::Sender<bool>,
}

impl Default for UpstreamControl {
    fn default() -> Self {
        Self::new(McpRequestLimits::default())
    }
}

impl UpstreamControl {
    pub(crate) fn health_result(&self, healthy: bool) {
        let mut state = self.state.lock().unwrap();
        state.last_outcome = Some(if healthy {
            "health_response_received"
        } else {
            "health_probe_failed"
        });
        if healthy {
            state.failures = 0;
            state.open_until = None;
            self.unavailable.send_replace(false);
        } else {
            state.failures = state.failures.saturating_add(1);
            if state.failures >= self.limits.failure_threshold {
                state.open_until =
                    Some(Instant::now() + Duration::from_millis(self.limits.recovery_delay_ms));
                self.unavailable.send_replace(true);
            }
        }
    }
    pub(crate) fn new(limits: McpRequestLimits) -> Self {
        Self {
            active: Semaphore::new(limits.max_concurrent_requests),
            pending: Semaphore::new(limits.max_concurrent_requests + limits.max_pending_requests),
            limits,
            playwright_mode: PlaywrightBackendMode::Other,
            state: Mutex::new(State::default()),
            unavailable: tokio::sync::watch::channel(false).0,
        }
    }
    /// Keep capability stable for this upstream connection and all exposure modes.
    pub(crate) fn for_server(mut self, name: &str, spec: &McpServerSpec) -> Self {
        self.playwright_mode = PlaywrightBackendMode::from_spec(name, spec);
        self
    }

    /// Advertise only what the connection mode can establish, not what a
    /// similar tool on another upstream may be able to do.
    pub(crate) fn browser_context_capability(&self) -> Option<&'static str> {
        match self.playwright_mode {
            PlaywrightBackendMode::Other => None,
            PlaywrightBackendMode::Browser => Some("browser-level"),
            PlaywrightBackendMode::Cdp => Some("cdp-permission-dependent"),
            PlaywrightBackendMode::Extension => Some("existing-tabs-only"),
            PlaywrightBackendMode::Unverified => Some("unverified-config"),
        }
    }

    pub(crate) fn isolated_tab_requested(
        &self,
        tool: &str,
        params: &CallToolRequestParams,
    ) -> bool {
        self.playwright_mode != PlaywrightBackendMode::Other
            && tool == "browser_tabs"
            && params.arguments.as_ref().is_some_and(|args| {
                args.get("action").and_then(Value::as_str) == Some("new")
                    && args
                        .get("isolatedContext")
                        .and_then(Value::as_str)
                        .is_some_and(|name| !name.is_empty() && name != "default")
            })
    }

    /// An extension is attached to existing tabs; it cannot provide real
    /// BrowserContexts. No attempt or fallback is safe for an isolation request.
    pub(crate) fn check_browser_context_request(
        &self,
        tool: &str,
        params: &CallToolRequestParams,
    ) -> Result<(), String> {
        if self.isolated_tab_requested(tool, params)
            && self.playwright_mode == PlaywrightBackendMode::Unverified
        {
            return Err(
                "Playwright config file is missing, invalid or unreadable, so browser-context                  capabilities cannot be verified. No request was dispatched. Restore the                  configured MCP file before creating an isolated browser context.".into()
            );
        }
        if self.isolated_tab_requested(tool, params)
            && self.playwright_mode == PlaywrightBackendMode::Extension
        {
            return Err(
                "Playwright extension/existing-tab mode cannot create a genuinely isolated                  browser context (Target.createBrowserContext requires browser-level access).                  No request was dispatched. Use a connected browser-level Playwright MCP                  backend for browser_tabs(new, isolatedContext), then keep subsequent tab                  operations on that backend. If it is unavailable, repair its startup;                  do not fall back to shared extension tabs."
                    .into(),
            );
        }
        Ok(())
    }

    /// Some CDP endpoints are page-scoped; translate the exact refusal but
    /// do not assume a context was created or automatically retry any tool.
    pub(crate) fn browser_context_refusal(
        &self,
        isolated_requested: bool,
        message: &str,
    ) -> Option<String> {
        (isolated_requested
            && message.contains("Target.createBrowserContext")
            && message.contains("-32000")
            && message.contains("Not allowed"))
        .then(|| {
            "CDP -32000 Not allowed: this Playwright/CDP session lacks browser-level              Target.createBrowserContext permission. No isolated context was confirmed.              Do not reuse an extension tab or assume creation succeeded; connect a              browser-level Playwright backend, verify session identity and existing              resources before retrying."
                .into()
        })
    }

    pub(crate) async fn wait_unavailable(&self) {
        let mut receiver = self.unavailable.subscribe();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
    fn check(state: &State) -> Result<(), String> {
        if let Some(until) = state.open_until {
            let remaining = until.saturating_duration_since(Instant::now()).as_millis();
            if remaining > 0 {
                return Err(format!(
                    "upstream is unresponsive; retry admission is paused for {remaining} ms. Earlier operations may still be running."
                ));
            }
            if state.probe_active {
                return Err("upstream recovery probe is already in progress; earlier operations may still be running.".into());
            }
        }
        Ok(())
    }
    pub(crate) async fn admit(
        &self,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<Admission<'_>, String> {
        if cancellation.is_cancelled() {
            return Err("cancelled by downstream client before dispatch".into());
        }
        Self::check(&self.state.lock().unwrap())?;
        let pending = self
            .pending
            .try_acquire()
            .map_err(|_| "upstream queue is full; request was not dispatched".to_string())?;
        let active = crate::audit::stage("upstream_queue", async {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err("cancelled by downstream client before dispatch".to_string()),
                _ = tokio::time::sleep_until(deadline) => Err("upstream queue deadline exceeded; request was not dispatched".to_string()),
                permit = self.active.acquire() => Ok(permit.expect("upstream semaphore stays open")),
            }
        }).await?;
        let mut state = self.state.lock().unwrap();
        Self::check(&state)?;
        let probe = state.open_until.is_some();
        if probe {
            state.probe_active = true;
        }
        Ok(Admission {
            control: self,
            _active: active,
            _pending: pending,
            started: Instant::now(),
            probe,
            outcome: None,
        })
    }
    pub(crate) fn snapshot(&self) -> Value {
        let state = self.state.lock().unwrap();
        let active = self
            .limits
            .max_concurrent_requests
            .saturating_sub(self.active.available_permits());
        let admitted = (self.limits.max_concurrent_requests + self.limits.max_pending_requests)
            .saturating_sub(self.pending.available_permits());
        let mut snapshot = json!({"state": if state.probe_active {"probing"} else if state.open_until.is_some() {"unresponsive"} else if state.failures > 0 {"degraded"} else {"connected"},
            "activeRequests":active,"queuedRequests":admitted.saturating_sub(active),"consecutiveFailures":state.failures,
            "retryAfterMs":state.open_until.map(|until| until.saturating_duration_since(Instant::now()).as_millis() as u64).unwrap_or(0),
            "lastOutcome":state.last_outcome});
        if let Some(capability) = self.browser_context_capability() {
            snapshot["browserContextCapability"] = json!(capability);
        }
        snapshot
    }
}

pub(crate) struct Admission<'a> {
    control: &'a UpstreamControl,
    _active: SemaphorePermit<'a>,
    _pending: SemaphorePermit<'a>,
    started: Instant,
    probe: bool,
    outcome: Option<&'static str>,
}
impl Admission<'_> {
    pub(crate) fn is_probe(&self) -> bool {
        self.probe
    }
    pub(crate) fn complete(&mut self, outcome: &'static str) {
        self.outcome = Some(outcome);
    }
}
impl Drop for Admission<'_> {
    fn drop(&mut self) {
        let outcome = self.outcome.unwrap_or("completion_unconfirmed");
        let mut state = self.control.state.lock().unwrap();
        state.last_outcome = Some(outcome);
        if self.probe {
            state.probe_active = false;
        }
        if outcome == "response_received" {
            state.failures = 0;
            state.open_until = None;
            self.control.unavailable.send_replace(false);
        } else if matches!(outcome, "deadline_exceeded" | "transport_failed")
            || self.started.elapsed()
                >= Duration::from_millis(self.control.limits.slow_cancellation_ms)
        {
            state.failures = state.failures.saturating_add(1);
            if self.probe || state.failures >= self.control.limits.failure_threshold {
                state.open_until = Some(
                    Instant::now() + Duration::from_millis(self.control.limits.recovery_delay_ms),
                );
                self.control.unavailable.send_replace(true);
            }
        } else if self.probe {
            state.open_until =
                Some(Instant::now() + Duration::from_millis(self.control.limits.recovery_delay_ms));
        }
    }
}

#[cfg(test)]
mod browser_context_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exposed_capability_matches_configured_backend() {
        let full = UpstreamControl::default().for_server(
            "playwright",
            &McpServerSpec {
                command: Some("node".into()),
                args: vec!["@playwright/mcp".into()],
                ..Default::default()
            },
        );
        let extension = UpstreamControl::default().for_server(
            "playwright-extension",
            &McpServerSpec {
                command: Some("node".into()),
                args: vec!["@playwright/mcp".into(), "--extension".into()],
                ..Default::default()
            },
        );
        assert_eq!(full.snapshot()["browserContextCapability"], "browser-level");
        assert_eq!(
            extension.snapshot()["browserContextCapability"],
            "existing-tabs-only"
        );
        assert!(
            UpstreamControl::default()
                .snapshot()
                .get("browserContextCapability")
                .is_none()
        );
    }

    fn spec(args: &[&str]) -> McpServerSpec {
        McpServerSpec {
            command: Some("node".into()),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            ..Default::default()
        }
    }

    fn request(action: &str, context: Option<&str>) -> CallToolRequestParams {
        let mut arguments = json!({ "action": action, "url": "https://example.invalid/form" });
        if let Some(context) = context {
            arguments["isolatedContext"] = json!(context);
        }
        CallToolRequestParams::new("browser_tabs")
            .with_arguments(arguments.as_object().unwrap().clone())
    }

    #[test]
    fn extension_mode_fails_before_isolated_creation() {
        let control = UpstreamControl::default().for_server(
            "playwright-extension",
            &spec(&["playwright/mcp.js", "--extension"]),
        );
        let request = request("new", Some("lane06_grants_funding"));
        assert!(control.isolated_tab_requested("browser_tabs", &request));
        let error = control
            .check_browser_context_request("browser_tabs", &request)
            .unwrap_err();
        assert!(error.contains("No request was dispatched"));
        assert!(error.contains("cannot create a genuinely isolated"));
    }

    #[test]
    fn extension_env_and_renamed_server_also_fail_closed() {
        let mut settings = spec(&["playwright/mcp.js"]);
        settings
            .env
            .insert("PLAYWRIGHT_MCP_EXTENSION".into(), "true".into());
        let control = UpstreamControl::default().for_server("my_browser", &settings);
        assert!(
            control
                .check_browser_context_request("browser_tabs", &request("new", Some("lane01")))
                .is_err()
        );
        settings
            .env
            .insert("PLAYWRIGHT_MCP_EXTENSION".into(), "false".into());
        let control = UpstreamControl::default().for_server("my_browser", &settings);
        assert!(
            control
                .check_browser_context_request("browser_tabs", &request("new", Some("lane01")))
                .is_ok()
        );
    }

    #[test]
    fn normal_extension_tab_operations_remain_compatible() {
        let control = UpstreamControl::default().for_server(
            "playwright-extension",
            &spec(&["playwright/mcp.js", "--extension"]),
        );
        for request in [
            request("list", Some("lane06")),
            request("new", None),
            request("new", Some("default")),
            request("select", Some("lane06")),
        ] {
            assert!(
                control
                    .check_browser_context_request("browser_tabs", &request)
                    .is_ok()
            );
        }
        assert!(
            control
                .check_browser_context_request("browser_navigate", &request("new", Some("lane06")))
                .is_ok()
        );
    }

    #[test]
    fn playwright_json_extension_mode_cannot_bypass_guard() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("playwright.json");
        std::fs::write(&file, r#"{"extension":true}"#).unwrap();
        let config_name = file.to_str().unwrap();
        let settings = spec(&["@playwright/mcp", "--config", config_name]);
        let control = UpstreamControl::default().for_server("playwright", &settings);
        assert_eq!(
            control.browser_context_capability(),
            Some("existing-tabs-only")
        );
        assert!(
            control
                .check_browser_context_request("browser_tabs", &request("new", Some("lane06")))
                .unwrap_err()
                .contains("No request was dispatched")
        );

        std::fs::write(
            &file,
            r#"{"extension":false,"browser":{"cdpEndpoint":"chrome"}}"#,
        )
        .unwrap();
        let control = UpstreamControl::default().for_server("playwright", &settings);
        assert_eq!(
            control.browser_context_capability(),
            Some("cdp-permission-dependent")
        );

        std::fs::write(&file, r#"{"extension":true}"#).unwrap();
        let mut overriding = settings.clone();
        overriding
            .env
            .insert("PLAYWRIGHT_MCP_EXTENSION".into(), "false".into());
        let control = UpstreamControl::default().for_server("playwright", &overriding);
        assert_eq!(control.browser_context_capability(), Some("browser-level"));
    }

    #[test]
    fn missing_or_invalid_json_config_fails_closed_without_blocking_default_tab() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("config.json");
        let mut settings = spec(&["@playwright/mcp", "--config"]);
        settings.args.push(file.to_string_lossy().into_owned());
        let control = UpstreamControl::default().for_server("playwright", &settings);
        assert_eq!(
            control.snapshot()["browserContextCapability"],
            "unverified-config"
        );
        assert!(
            control
                .check_browser_context_request("browser_tabs", &request("new", Some("lane06")))
                .unwrap_err()
                .contains("No request was dispatched")
        );
        assert!(
            control
                .check_browser_context_request("browser_tabs", &request("new", None))
                .is_ok()
        );

        std::fs::write(&file, "not json").unwrap();
        let control = UpstreamControl::default().for_server("playwright", &settings);
        assert_eq!(
            control.snapshot()["browserContextCapability"],
            "unverified-config"
        );

        std::fs::write(&file, r#"{"extension":true}"#).unwrap();
        settings.args.push("--extension=false".into());
        let control = UpstreamControl::default().for_server("playwright", &settings);
        assert_eq!(
            control.snapshot()["browserContextCapability"],
            "browser-level"
        );
    }

    #[test]
    fn playwright_config_relative_to_upstream_cwd_is_resolved() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("playwright.json"), r#"{"extension":true}"#).unwrap();
        let mut settings = spec(&["@playwright/mcp", "--config=playwright.json"]);
        settings.cwd = Some(temp.path().to_string_lossy().into_owned());
        let control = UpstreamControl::default().for_server("playwright", &settings);
        assert_eq!(
            control.snapshot()["browserContextCapability"],
            "existing-tabs-only"
        );
    }

    #[test]
    fn browser_and_cdp_backend_allow_capable_context_attempts() {
        for settings in [
            spec(&["playwright/mcp.js"]),
            spec(&["playwright/mcp.js", "--cdp-endpoint=http://127.0.0.1:9222"]),
        ] {
            let control = UpstreamControl::default().for_server("playwright", &settings);
            assert!(
                control
                    .check_browser_context_request(
                        "browser_tabs",
                        &request("new", Some("lane06_grants_funding"))
                    )
                    .is_ok()
            );
        }
    }

    #[test]
    fn cdp_minus_32000_is_precise_but_unrelated_errors_stay_untouched() {
        let control = UpstreamControl::default().for_server(
            "playwright",
            &spec(&["playwright/mcp.js", "--cdp-endpoint=chrome"]),
        );
        let error = r#"Protocol error (Target.createBrowserContext): {"code":-32000,"message":"Not allowed"}"#;
        assert!(
            control
                .browser_context_refusal(true, error)
                .unwrap()
                .contains("No isolated context was confirmed")
        );
        assert!(control.browser_context_refusal(false, error).is_none());
        assert!(
            control
                .browser_context_refusal(
                    true,
                    "Protocol error -32000: Not allowed for Page.navigate"
                )
                .is_none()
        );
    }

    #[test]
    fn unrelated_mcp_backend_and_disconnect_cannot_silently_switch_modes() {
        let ordinary = UpstreamControl::default()
            .for_server("other-mcp", &spec(&["unrelated-server.js", "--extension"]));
        assert!(
            ordinary
                .check_browser_context_request("browser_tabs", &request("new", Some("lane06")))
                .is_ok()
        );
        let extension = UpstreamControl::default().for_server(
            "playwright_extension",
            &spec(&["playwright/mcp.js", "--extension"]),
        );
        extension.health_result(false);
        extension.health_result(true);
        assert!(
            extension
                .check_browser_context_request("browser_tabs", &request("new", Some("lane06")))
                .is_err()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failures_pause_calls_and_recovery_allows_only_one_probe() {
        let limits = McpRequestLimits {
            failure_threshold: 2,
            recovery_delay_ms: 20,
            ..Default::default()
        };
        let control = UpstreamControl::new(limits);
        let token = CancellationToken::new();
        for _ in 0..2 {
            let mut admission = control
                .admit(Instant::now() + Duration::from_secs(1), &token)
                .await
                .unwrap();
            admission.complete("deadline_exceeded");
        }
        assert_eq!(control.snapshot()["state"], "unresponsive");
        assert!(
            control
                .admit(Instant::now() + Duration::from_secs(1), &token)
                .await
                .is_err()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
        let mut probe = control
            .admit(Instant::now() + Duration::from_secs(1), &token)
            .await
            .unwrap();
        assert!(
            control
                .admit(Instant::now() + Duration::from_secs(1), &token)
                .await
                .is_err()
        );
        probe.complete("response_received");
        drop(probe);
        assert_eq!(control.snapshot()["state"], "connected");
    }

    #[tokio::test]
    async fn admission_limits_do_not_spread_to_another_upstream() {
        let control = UpstreamControl::new(McpRequestLimits {
            max_concurrent_requests: 1,
            max_pending_requests: 0,
            ..Default::default()
        });
        let token = CancellationToken::new();
        let mut first = control
            .admit(Instant::now() + Duration::from_secs(1), &token)
            .await
            .unwrap();
        assert!(
            control
                .admit(Instant::now() + Duration::from_secs(1), &token)
                .await
                .err()
                .unwrap()
                .contains("queue is full")
        );
        let other = UpstreamControl::default();
        let mut independent = other
            .admit(Instant::now() + Duration::from_secs(1), &token)
            .await
            .unwrap();
        independent.complete("response_received");
        first.complete("response_received");
    }

    #[tokio::test]
    async fn queued_cancellation_does_not_count_as_an_upstream_failure() {
        let control = UpstreamControl::new(McpRequestLimits {
            max_concurrent_requests: 1,
            max_pending_requests: 1,
            ..Default::default()
        });
        let token = CancellationToken::new();
        let mut first = control
            .admit(Instant::now() + Duration::from_secs(1), &token)
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        let work = control.admit(Instant::now() + Duration::from_secs(1), &cancellation);
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(work, cancel);
        assert!(result.err().unwrap().contains("before dispatch"));
        assert_eq!(control.snapshot()["consecutiveFailures"], 0);
        first.complete("response_received");
    }
}
