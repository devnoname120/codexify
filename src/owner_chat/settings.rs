use super::*;

#[derive(Default)]
pub(super) struct GlobalRuntime {
    plan: Option<Value>,
    plan_account: Option<String>,
    plan_attempted: bool,
    plan_retry_at_ms: u64,
    refresh: Option<Value>,
    manual_refresh: Option<Value>,
    update: Option<Value>,
}
impl GlobalRuntime {
    pub(super) fn reset_plan(&mut self) {
        self.plan = None;
        self.plan_attempted = false;
        self.plan_retry_at_ms = 0;
    }
    fn record_plan(&mut self, result: Result<Value, String>, now_ms: u64) {
        match result {
            Ok(plan) => {
                self.plan = Some(plan);
                self.plan_attempted = true;
                self.plan_retry_at_ms = 0;
            }
            Err(error) if error.to_lowercase().contains("busy") || error.contains("423") => {}
            Err(_) => {
                // Cache successful lookups only; retry transient failures after a cooldown.
                self.plan_retry_at_ms = now_ms.saturating_add(60_000);
            }
        }
    }
    fn observe_account(&mut self, account: Option<String>) {
        // A transient browser-health failure does not identify an account change.
        // Retain the startup cache until a different account is actually verified.
        if account.is_some() && account != self.plan_account {
            self.reset_plan();
            self.plan_account = account;
        }
    }
}

async fn refresh_saved_connector(
    state: &OwnerState,
    client: &BrowserCompanionClient,
) -> Result<Value, String> {
    let name = require_connector_name(&state.config).map_err(|(_, error)| error)?;
    client
        .refresh_connector(&name, &CancellationToken::new())
        .await
}

pub(super) fn start(state: OwnerState) {
    if state.config.loaded_config_path.is_none() {
        return;
    }
    tokio::spawn(async move {
        let epoch = format!("{}-{}", std::process::id(), crate::markdown_chat::now_ms());
        loop {
            let pending = crate::global_settings::read_refresh_intent().filter(|intent| {
                intent["targetVersion"] == env!("CARGO_PKG_VERSION")
                    && matches!(
                        intent["phase"].as_str(),
                        Some("waiting_for_restart" | "starting_browser")
                    )
            });
            if state.config.markdown_chat.automation.enabled
                && companion_client(&state).is_err()
                && !crate::agent_automation::BrowserCompanionControl::new(
                    &state.config.markdown_chat.automation,
                )
                .is_ok_and(|c| c.public_status()["manuallyStopped"] == true)
                && let Some(mut intent) = pending
                && intent["updateId"]
                    .as_str()
                    .and_then(|id| crate::self_update::read_update_status(id).ok())
                    .is_some_and(|record| {
                        record.state == crate::self_update::UpdatePhase::Succeeded
                    })
            {
                intent["phase"] = json!("starting_browser");
                let _ = crate::global_settings::write_refresh_intent(&intent);
                let upgraded = async {
                    let manager = crate::agent_automation::BrowserCompanionManager::from_config(
                        &state.config.markdown_chat.automation,
                    )?;
                    let status = manager.status().await;
                    if !status.installed || status.version != env!("CARGO_PKG_VERSION") {
                        manager.install().await?;
                    }
                    crate::agent_automation::BrowserCompanionControl::new(
                        &state.config.markdown_chat.automation,
                    )?
                    .ensure_running(&CancellationToken::new())
                    .await?;
                    Ok::<_, String>(())
                }
                .await;
                if let Err(error) = upgraded {
                    intent["phase"] = json!("failed");
                    intent["result"] = json!({"error":error});
                    let _ = crate::global_settings::write_refresh_intent(&intent);
                    state.global.lock().await.refresh = Some(intent);
                }
            }
            if let Ok(client) = companion_client(&state)
                && let Ok(health) = client.health(&CancellationToken::new()).await
            {
                let account = health.account_fingerprint.clone();
                let mut global = state.global.lock().await;
                global.observe_account(account);
                if health.authenticated
                    && !global.plan_attempted
                    && crate::markdown_chat::now_ms() >= global.plan_retry_at_ms
                    && health.active_action_id.is_none()
                {
                    drop(global);
                    let plan = client.account_plan(&epoch, &CancellationToken::new()).await;
                    global = state.global.lock().await;
                    global.record_plan(plan, crate::markdown_chat::now_ms());
                }
                drop(global);
                if health.authenticated && health.active_action_id.is_none() {
                    let intent = crate::global_settings::read_refresh_intent();
                    if let Some(mut intent) = intent.filter(|intent| {
                        intent["targetVersion"] == env!("CARGO_PKG_VERSION")
                            && matches!(
                                intent["phase"].as_str(),
                                Some(
                                    "waiting_for_restart"
                                        | "starting_browser"
                                        | "queued"
                                        | "running"
                                )
                            )
                    }) {
                        // Wait for the updater's readiness check, rather than refreshing a half-started server.
                        let succeeded = intent["updateId"]
                            .as_str()
                            .and_then(|id| crate::self_update::read_update_status(id).ok())
                            .is_some_and(|record| {
                                record.state == crate::self_update::UpdatePhase::Succeeded
                            });
                        if succeeded {
                            intent["phase"] = json!("running");
                            let _ = crate::global_settings::write_refresh_intent(&intent);
                            let result = refresh_saved_connector(&state, &client).await;
                            if result.as_ref().is_err_and(|error| error.contains("423")) {
                                intent["phase"] = json!("queued");
                                let _ = crate::global_settings::write_refresh_intent(&intent);
                                tokio::time::sleep(Duration::from_secs(5)).await;
                                continue;
                            }
                            intent["phase"] = json!(if result.is_ok() {
                                "completed"
                            } else {
                                "failed"
                            });
                            intent["result"] = match result {
                                Ok(value) => value,
                                Err(error) => json!({"error":error}),
                            };
                            let _ = crate::global_settings::write_refresh_intent(&intent);
                            state.global.lock().await.refresh = Some(intent);
                        }
                    }
                }
                refresh_owner_catalog_if_due(&state, &health, crate::markdown_chat::now_ms()).await;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

pub(super) async fn health(State(state): State<OwnerState>) -> ApiResult {
    let schema = crate::global_settings::read_schema_intent(&state.config);
    let (mut body, readiness) = super::readiness::snapshot(&state, schema.as_ref()).await;
    body["readiness"] = readiness;
    Ok(Json(body))
}

fn update_progress(current: Option<&Value>, intent: Option<&Value>) -> Option<Value> {
    let update_id = current
        .and_then(|value| value["updateId"].as_str())
        .or_else(|| intent.and_then(|value| value["updateId"].as_str()));
    if current.is_some_and(|value| value["state"] == "preparing" || value["updateId"].is_null()) {
        return current.cloned();
    }
    update_id
        .and_then(|id| crate::self_update::read_update_status(id).ok())
        .and_then(|record| serde_json::to_value(record).ok())
        .or_else(|| current.cloned())
}

pub(super) async fn status(State(state): State<OwnerState>) -> ApiResult {
    let latest = crate::self_update::inspect_latest_version().await;
    let latest = match latest {
        Ok(value) => {
            json!({"version":value.latest.to_string(),"updateAvailable":value.status==crate::self_update::LatestVersionStatus::UpdateAvailable})
        }
        Err(_) => json!({"version":null,"updateAvailable":false,"unavailable":true}),
    };
    let schema = crate::global_settings::read_schema_intent(&state.config);
    let global = state.global.lock().await;
    let update_intent = crate::global_settings::read_refresh_intent();
    let update = update_progress(global.update.as_ref(), update_intent.as_ref());
    let refresh = schema
        .as_ref()
        .filter(|value| {
            matches!(
                value["phase"].as_str(),
                Some("queued" | "running" | "starting_browser" | "waiting_for_restart" | "failed")
            )
        })
        .cloned()
        .or_else(|| {
            global
                .manual_refresh
                .clone()
                .or_else(|| global.refresh.clone())
                .or_else(crate::global_settings::read_refresh_intent)
        })
        .or(schema);
    let plan = global.plan.clone();
    drop(global);
    let (_, readiness) = super::readiness::snapshot(&state, refresh.as_ref()).await;
    Ok(Json(
        json!({"version":env!("CARGO_PKG_VERSION"),"instanceId":std::process::id().to_string(),"latest":latest,"plan":plan,"connectorRefresh":refresh,"readiness":readiness,"update":update,
        "settingsAvailable":state.config.loaded_config_path.is_some(),"restartAvailable":std::env::var_os(crate::process_env::SERVICE_SUPERVISED_ENV).as_deref()==Some(std::ffi::OsStr::new("1"))&&crate::service::is_installed().unwrap_or(false)}),
    ))
}

pub(super) async fn connector(State(state): State<OwnerState>) -> ApiResult {
    let name = crate::global_settings::connector_name(
        state.config.loaded_config_path.as_deref(),
        &state.config.markdown_chat.automation.connector_name,
    )
    .map_err(internal_error)?;
    Ok(Json(
        json!({"configured": name.is_some(), "connectorName": name,
        "canSave": state.config.loaded_config_path.is_some()}),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ConnectorBody {
    connector_name: String,
}

pub(super) async fn save_connector(
    State(state): State<OwnerState>,
    Json(body): Json<ConnectorBody>,
) -> ApiResult {
    let name = body.connector_name.trim().to_string();
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Enter the exact connector name (1 to 128 visible characters).".into(),
        ));
    }
    let config = state.config.clone();
    let saved_name = name.clone();
    tokio::task::spawn_blocking(move || {
        let snapshot = crate::global_settings::snapshot(&config)?;
        let revision = snapshot["revision"]
            .as_str()
            .ok_or("Configuration revision is unavailable")?;
        let mut changes = serde_json::Map::from_iter([(
            "agentChat.automation.connectorName".into(),
            json!(saved_name),
        )]);
        if snapshot["document"]
            .get("workDir")
            .is_none_or(Value::is_null)
        {
            changes.insert("workDir".into(), json!(config.work_dir));
        }
        crate::global_settings::save(&config, revision, None, Some(changes))
    })
    .await
    .map_err(internal_error)?
    .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    Ok(Json(json!({"configured":true,"connectorName":name})))
}

pub(super) async fn read(State(state): State<OwnerState>) -> ApiResult {
    let config = state.config.clone();
    let value = tokio::task::spawn_blocking(move || crate::global_settings::snapshot(&config))
        .await
        .map_err(internal_error)?
        .map_err(internal_error)?;
    Ok(Json(value))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SaveBody {
    revision: String,
    raw: Option<String>,
    changes: Option<serde_json::Map<String, Value>>,
}
pub(super) async fn save(State(state): State<OwnerState>, Json(body): Json<SaveBody>) -> ApiResult {
    let config = state.config.clone();
    let mut value = tokio::task::spawn_blocking(move || {
        crate::global_settings::save(&config, &body.revision, body.raw, body.changes)
    })
    .await
    .map_err(internal_error)?
    .map_err(|error| {
        (
            if error.contains("changed on disk") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            },
            error,
        )
    })?;
    if let Some(supervisor) = &state.automation {
        supervisor.notify();
    }
    if value["schemaRefreshRequired"] == true
        && let Err(error) = super::schema_refresh::queue(state.config.clone(), &value).await
    {
        value["schemaRefreshError"] = json!(error);
    }
    Ok(Json(value))
}

pub(super) async fn restart(State(state): State<OwnerState>) -> ApiResult {
    // Request rules, presentation, recovery, and navigation are launch-time policy.
    // Apply them only when no companion UI action is in progress.
    if let Some(path) = state.config.loaded_config_path.clone() {
        let next = tokio::task::spawn_blocking(move || {
            use clap::Parser;
            let cli = crate::config::Cli::try_parse_from([
                std::ffi::OsString::from("codexify"),
                std::ffi::OsString::from("--config"),
                path.into_os_string(),
            ])
            .map_err(|e| e.to_string())?;
            crate::config::load_config_quiet_read_only(cli)
        })
        .await
        .map_err(internal_error)?
        .map_err(internal_error)?;
        let old = &state.config.markdown_chat.automation;
        let new = &next.markdown_chat.automation;
        if old.enabled
            && (!new.enabled
                || old.effective_browser_simplicity() != new.effective_browser_simplicity()
                || old.recovery_enhancements != new.recovery_enhancements
                || old.effective_request_block_list() != new.effective_request_block_list()
                || old.retry_timings.conversation_load_interval_seconds
                    != new.retry_timings.conversation_load_interval_seconds
                || old.retry_timings.conversation_unavailable_cooldown_seconds
                    != new.retry_timings.conversation_unavailable_cooldown_seconds
                || old.retry_timings.rate_limit_cooldown_seconds
                    != new.retry_timings.rate_limit_cooldown_seconds)
        {
            let manager = crate::agent_automation::BrowserCompanionManager::from_config(old)
                .map_err(internal_error)?;
            if manager.status().await.running {
                let client = companion_client(&state)?;
                let health = client
                    .health(&CancellationToken::new())
                    .await
                    .map_err(internal_error)?;
                if health.active_action_id.is_some() {
                    return Err((
                        StatusCode::LOCKED,
                        "A browser action is active. Wait for it to finish, then restart.".into(),
                    ));
                }
                crate::agent_automation::BrowserCompanionControl::new(old)
                    .map_err(internal_error)?
                    .stop_for_restart()
                    .await
                    .map_err(internal_error)?;
            }
        }
    }
    tokio::task::spawn_blocking(crate::global_settings::schedule_restart)
        .await
        .map_err(internal_error)?
        .map_err(internal_error)?;
    Ok(Json(json!({"scheduled":true})))
}

pub(super) async fn update(State(state): State<OwnerState>) -> ApiResult {
    {
        let mut global = state.global.lock().await;
        let pending = update_progress(
            global.update.as_ref(),
            crate::global_settings::read_refresh_intent().as_ref(),
        );
        if pending.as_ref().is_some_and(|value| {
            matches!(
                value["state"].as_str(),
                Some("preparing" | "scheduled" | "installing" | "validating" | "restarting")
            )
        }) {
            return Err((
                StatusCode::CONFLICT,
                "An update is already being prepared.".into(),
            ));
        }
        global.update = Some(json!({"state":"preparing"}));
    }
    match crate::self_update::trigger(state.config.port).await {
        Ok(receipt) => {
            let value = serde_json::to_value(receipt).map_err(internal_error)?;
            state.global.lock().await.update = if value["status"] == "scheduled" {
                Some(
                    json!({"state":"scheduled","updateId":value["updateId"],"targetVersion":value["targetVersion"]}),
                )
            } else {
                None
            };
            Ok(Json(value))
        }
        Err(error) => {
            let detail = error.to_string();
            state.global.lock().await.update =
                Some(json!({"state":"failed","failureDetail":detail}));
            Err(internal_error(detail))
        }
    }
}

pub(super) async fn refresh_connector(State(state): State<OwnerState>) -> ApiResult {
    let mut global = state.global.lock().await;
    if let Some(pending) = global.manual_refresh.as_ref()
        && matches!(pending["phase"].as_str(), Some("queued" | "running"))
    {
        return Ok(Json(pending.clone()));
    }
    // Validate the companion connection before accepting a new job, then return
    // immediately. The job belongs to the server, not the requesting browser page.
    companion_client(&state)?;
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = format!(
        "refresh-{}-{}",
        crate::markdown_chat::now_ms(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let receipt = json!({"refreshId":id,"source":"manual","phase":"queued"});
    global.manual_refresh = Some(receipt.clone());
    drop(global);
    tokio::spawn(run_manual_refresh(state, id, Duration::from_secs(5)));
    Ok(Json(receipt))
}

async fn set_manual_refresh_phase(
    state: &OwnerState,
    id: &str,
    phase: &str,
    result: Option<Value>,
) {
    let mut global = state.global.lock().await;
    if let Some(refresh) = global.manual_refresh.as_mut()
        && refresh["refreshId"] == id
    {
        refresh["phase"] = json!(phase);
        if let Some(result) = result {
            refresh["result"] = result;
        }
    }
}

async fn run_manual_refresh(state: OwnerState, id: String, retry_after: Duration) {
    loop {
        let result = async {
            let client = super::schema_refresh::refresh_client(&state.config).await?;
            let health = client.health(&CancellationToken::new()).await?;
            if health.active_action_id.is_some()
                || health.status == crate::agent_automation::CompanionSessionStatus::Busy
            {
                return Err("Browser companion is busy".to_string());
            }
            set_manual_refresh_phase(&state, &id, "running", None).await;
            refresh_saved_connector(&state, &client).await
        }
        .await;
        match result {
            Err(error)
                if error.to_lowercase().contains("busy") || error.contains("HTTP status 423") =>
            {
                set_manual_refresh_phase(&state, &id, "queued", None).await;
                tokio::time::sleep(retry_after).await;
            }
            Ok(result) => {
                // A manual refresh also satisfies any previously pending update refresh.
                if state.config.loaded_config_path.is_some()
                    && let Some(mut intent) = crate::global_settings::read_refresh_intent()
                {
                    intent["phase"] = json!("completed");
                    intent["result"] = result.clone();
                    let _ = crate::global_settings::write_refresh_intent(&intent);
                }
                set_manual_refresh_phase(&state, &id, "completed", Some(result)).await;
                return;
            }
            Err(error) => {
                set_manual_refresh_phase(&state, &id, "failed", Some(json!({"error":error}))).await;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_current_update_failure_is_not_replaced_by_an_older_update_intent() {
        let failed = json!({"state":"failed","failureDetail":"Update verification failed"});
        let preparing = json!({"state":"preparing"});
        let old = json!({"updateId":"invalid-old-id"});
        assert_eq!(
            update_progress(Some(&failed), Some(&old)),
            Some(failed.clone())
        );
        assert_eq!(
            update_progress(Some(&preparing), Some(&old)),
            Some(preparing.clone())
        );
        assert_eq!(update_progress(None, None), None);
    }
    #[derive(Clone, Default)]
    struct RefreshFixture {
        mode: Arc<std::sync::atomic::AtomicUsize>,
        health_calls: Arc<std::sync::atomic::AtomicUsize>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        successes: Arc<std::sync::atomic::AtomicUsize>,
        release: Arc<tokio::sync::Notify>,
    }
    async fn refresh_health(State(fixture): State<RefreshFixture>) -> Json<Value> {
        use std::sync::atomic::Ordering;
        fixture.health_calls.fetch_add(1, Ordering::SeqCst);
        Json(
            json!({"protocolVersion":1,"kind":"codexify-browser-companion",
            "build":format!("{}{}",env!("CARGO_PKG_VERSION"),crate::agent_automation::PROTOCOL_BUILD_SUFFIX),
            "authenticated":true,"accountFingerprint":"fixture-account","activeActionId":null,
            "status":if fixture.mode.load(Ordering::SeqCst)==0 {"busy"} else {"idle"}}),
        )
    }
    async fn fixture_refresh(
        State(fixture): State<RefreshFixture>,
        headers: axum::http::HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        use std::sync::atomic::Ordering;
        assert_eq!(
            headers[header::AUTHORIZATION],
            "Bearer AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        );
        assert_eq!(body["name"], "Fixture connector");
        fixture.calls.fetch_add(1, Ordering::SeqCst);
        match fixture.mode.load(Ordering::SeqCst) {
            1 => (
                StatusCode::LOCKED,
                Json(json!({"error":"Browser companion is busy"})),
            )
                .into_response(),
            2 => {
                fixture.release.notified().await;
                fixture.successes.fetch_add(1, Ordering::SeqCst);
                Json(json!({"refreshObserved":true,"permissionChanged":false,"schemaVerified":false})).into_response()
            }
            4 => (
                StatusCode::CONFLICT,
                Json(json!({"error":"Selector changed"})),
            )
                .into_response(),
            _ => {
                fixture.successes.fetch_add(1, Ordering::SeqCst);
                Json(json!({"refreshObserved":true,"permissionChanged":false,"schemaVerified":false})).into_response()
            }
        }
    }
    async fn refresh_fixture(
        root: &FilePath,
    ) -> (
        OwnerState,
        RefreshFixture,
        String,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    ) {
        crate::tls::ensure_crypto_provider();
        let fixture = RefreshFixture::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/health",
                get(|| async {
                    Json(json!({"status":"ok","tunnelsReady":0,"tunnelsConfigured":0}))
                }),
            )
            .route("/v1/health", get(refresh_health))
            .route("/v1/connectors/refresh", post(fixture_refresh))
            .with_state(fixture.clone());
        let companion = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let browser_directory = root.join("browser");
        std::fs::create_dir_all(&browser_directory).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&browser_directory, std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let descriptor = browser_directory.join("descriptor.json");
        std::fs::write(&descriptor, json!({"version":1,"kind":"codexify-browser-companion","pid":std::process::id(),
            "endpoint":endpoint,"token":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "build":format!("{}{}",env!("CARGO_PKG_VERSION"),crate::agent_automation::PROTOCOL_BUILD_SUFFIX),"createdAt":"2026-10-14T00:00:00Z"}).to_string()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&descriptor, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut config = crate::config::default_config(root.to_owned());
        config.port = reqwest::Url::parse(&endpoint).unwrap().port().unwrap();
        config.markdown_chat.automation.enabled = true;
        config.markdown_chat.automation.descriptor_path = descriptor.display().to_string();
        config.markdown_chat.automation.connector_name = "Fixture connector".into();
        let state = OwnerState {
            config: Arc::new(config),
            chats: Arc::default(),
            project_bindings: Arc::new(crate::project_bindings::ProjectBindingStore::new(
                root.join("bindings"),
            )),
            creation_lock: Arc::default(),
            artifacts: Arc::new(ArtifactEgressStore::new_at(
                crate::types::ArtifactEgressConfig::default(),
                root.join("artifacts"),
            )),
            continuations: Arc::new(
                ConversationContinuationStore::new(root.join("continuations.json")).unwrap(),
            ),
            automation: None,
            open_browser: Arc::new(|_| Ok(())),
            catalog_cache: Arc::default(),
            global: Arc::default(),
            token: "owner-test-token".into(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        companion_client(&state).expect("fixture companion descriptor must be valid");
        let app = router(state.clone());
        let owner = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (state, fixture, base, owner, companion)
    }
    async fn wait_refresh_phase(state: &OwnerState, phase: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let refresh = state.global.lock().await.manual_refresh.clone().unwrap();
                if refresh["phase"] == phase {
                    return refresh;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("refresh changed phase")
    }
    #[tokio::test]
    async fn manual_refresh_queues_and_deduplicates_until_busy_tab_becomes_available() {
        use std::sync::atomic::Ordering;
        let root = tempfile::tempdir().unwrap();
        let (state, fixture, base, owner, companion) = refresh_fixture(root.path()).await;
        let client = reqwest::Client::new();
        let url = format!("{base}/api/settings/refresh-connector");
        assert_eq!(
            client.post(&url).send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let receipt: Value = tokio::time::timeout(
            Duration::from_secs(1),
            client.post(&url).bearer_auth("owner-test-token").send(),
        )
        .await
        .unwrap()
        .unwrap()
        .json()
        .await
        .unwrap();
        assert_eq!(receipt["phase"], "queued");
        let duplicate: Value = client
            .post(&url)
            .bearer_auth("owner-test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(duplicate["refreshId"], receipt["refreshId"]);
        assert_eq!(
            fixture.calls.load(Ordering::SeqCst),
            0,
            "health-busy tabs are left untouched"
        );
        fixture.mode.store(1, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(6), async {
            while fixture.calls.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        wait_refresh_phase(&state, "queued").await;
        assert_eq!(
            fixture.successes.load(Ordering::SeqCst),
            0,
            "HTTP 423 keeps the job queued"
        );
        fixture.mode.store(2, Ordering::SeqCst);
        wait_refresh_phase(&state, "running").await;
        let duplicate: Value = client
            .post(&url)
            .bearer_auth("owner-test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(duplicate["refreshId"], receipt["refreshId"]);
        assert_eq!(duplicate["phase"], "running");
        fixture.release.notify_one();
        let done = wait_refresh_phase(&state, "completed").await;
        assert_eq!(done["refreshId"], receipt["refreshId"]);
        assert_eq!(done["result"]["refreshObserved"], true);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.successes.load(Ordering::SeqCst), 1);
        owner.abort();
        companion.abort();
    }
    #[tokio::test]
    async fn manual_refresh_reports_real_failure_and_allows_a_new_retry() {
        use std::sync::atomic::Ordering;
        let root = tempfile::tempdir().unwrap();
        let (state, fixture, base, owner, companion) = refresh_fixture(root.path()).await;
        fixture.mode.store(4, Ordering::SeqCst);
        let client = reqwest::Client::new();
        let url = format!("{base}/api/settings/refresh-connector");
        let first: Value = client
            .post(&url)
            .bearer_auth("owner-test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let failed = wait_refresh_phase(&state, "failed").await;
        assert!(
            failed["result"]["error"]
                .as_str()
                .unwrap()
                .contains("state changed")
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        fixture.mode.store(3, Ordering::SeqCst);
        let retry: Value = client
            .post(&url)
            .bearer_auth("owner-test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_ne!(first["refreshId"], retry["refreshId"]);
        wait_refresh_phase(&state, "completed").await;
        assert_eq!(fixture.successes.load(Ordering::SeqCst), 1);
        owner.abort();
        companion.abort();
    }
    #[test]
    fn failed_plan_lookup_retries_and_success_is_cached_until_restart() {
        let mut runtime = GlobalRuntime::default();
        runtime.record_plan(Err("Browser operation timed out".into()), 1000);
        assert!(!runtime.plan_attempted);
        assert!(runtime.plan.is_none());
        assert_eq!(runtime.plan_retry_at_ms, 61_000);
        runtime.record_plan(Ok(json!({"label":"Pro 20x","kind":"pro"})), 61_000);
        assert!(runtime.plan_attempted);
        assert_eq!(runtime.plan_retry_at_ms, 0);
        assert_eq!(runtime.plan.as_ref().unwrap()["label"], "Pro 20x");
        runtime.reset_plan();
        assert!(!runtime.plan_attempted);
        assert!(runtime.plan.is_none());
        assert_eq!(runtime.plan_retry_at_ms, 0);
    }
    #[test]
    fn cached_plan_survives_transient_health_failures_and_resets_for_a_verified_new_account() {
        let mut runtime = GlobalRuntime {
            update: None,
            plan: Some(json!({"label":"Pro 20x","kind":"pro"})),
            plan_account: Some("account-one".into()),
            plan_attempted: true,
            plan_retry_at_ms: 0,
            refresh: None,
            manual_refresh: None,
        };
        runtime.observe_account(None);
        runtime.observe_account(Some("account-one".into()));
        assert_eq!(runtime.plan.as_ref().unwrap()["label"], "Pro 20x");
        assert!(runtime.plan_attempted);
        runtime.observe_account(Some("account-two".into()));
        assert!(runtime.plan.is_none());
        assert!(!runtime.plan_attempted);
    }
    #[tokio::test]
    async fn global_editor_is_owner_authenticated_and_invalid_saves_leave_the_file_unchanged() {
        crate::tls::ensure_crypto_provider();
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("config.json");
        std::fs::write(
            &file,
            format!(
                "{{\"workDir\":{},\"codexMcp\":{{\"enabled\":false}},\"debug\":false}}",
                serde_json::to_string(root.path()).unwrap()
            ),
        )
        .unwrap();
        let mut config = crate::config::default_config(root.path().to_owned());
        config.loaded_config_path = Some(file.clone());
        let state = OwnerState {
            config: Arc::new(config),
            chats: Arc::default(),
            project_bindings: Arc::new(crate::project_bindings::ProjectBindingStore::new(
                root.path().join("bindings"),
            )),
            creation_lock: Arc::default(),
            artifacts: Arc::new(ArtifactEgressStore::new_at(
                crate::types::ArtifactEgressConfig::default(),
                root.path().join("artifacts"),
            )),
            continuations: Arc::new(
                ConversationContinuationStore::new(root.path().join("continuations.json")).unwrap(),
            ),
            automation: None,
            open_browser: Arc::new(|_| Ok(())),
            catalog_cache: Arc::default(),
            global: Arc::default(),
            token: "test-token".into(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server =
            tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
        let client = reqwest::Client::new();
        // Setup is owner-authenticated and checked before browser work starts.
        assert_eq!(
            client
                .get(format!("{base}/api/settings/connector"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let setup: Value = client
            .get(format!("{base}/api/settings/connector"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(setup["configured"], false);
        assert!(setup["connectorName"].is_null());
        assert_eq!(
            client
                .post(format!("{base}/api/new-chat"))
                .bearer_auth("test-token")
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::PRECONDITION_REQUIRED
        );
        let unchanged = std::fs::read(&file).unwrap();
        for name in [
            "".to_string(),
            "  ".into(),
            "invalid\nname".into(),
            "x".repeat(129),
        ] {
            assert_eq!(
                client
                    .post(format!("{base}/api/settings/connector"))
                    .bearer_auth("test-token")
                    .json(&json!({"connectorName":name}))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::BAD_REQUEST
            );
            assert_eq!(std::fs::read(&file).unwrap(), unchanged);
        }
        assert_eq!(
            client
                .post(format!("{base}/api/settings/connector"))
                .bearer_auth("test-token")
                .json(&json!({"connectorName":"My exact connector"}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let setup: Value = client
            .get(format!("{base}/api/settings/connector"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(setup["configured"], true);
        assert_eq!(setup["connectorName"], "My exact connector");

        assert_eq!(
            client
                .get(format!("{base}/api/settings"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        for operation in ["start", "stop"] {
            let url = format!("{base}/api/browser/{operation}");
            assert_eq!(
                client
                    .post(&url)
                    .json(&json!({"requestId":"unauthorized-control"}))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                client
                    .post(&url)
                    .bearer_auth("test-token")
                    .json(&json!({"requestId":""}))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::BAD_REQUEST
            );
        }
        let snapshot: Value = client
            .get(format!("{base}/api/settings"))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let before = std::fs::read(&file).unwrap();
        let rejected = client
            .post(format!("{base}/api/settings"))
            .bearer_auth("test-token")
            .json(&json!({"revision":snapshot["revision"],"raw":"{\"port\":99999}"}))
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        assert_eq!(std::fs::read(&file).unwrap(), before);
        let saved: Value = client
            .post(format!("{base}/api/settings"))
            .bearer_auth("test-token")
            .json(&json!({"revision":snapshot["revision"],"changes":{"debug":true}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(saved["document"]["debug"], true);
        let stale = client
            .post(format!("{base}/api/settings"))
            .bearer_auth("test-token")
            .json(&json!({"revision":snapshot["revision"],"changes":{"debug":false}}))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::CONFLICT);
        server.abort();
    }
    #[tokio::test]
    async fn schema_edits_debounce_and_external_edits_refresh_without_an_open_settings_page() {
        use std::sync::atomic::Ordering;
        let root = tempfile::tempdir().unwrap();
        let (mut state, fixture, _base, owner, companion) = refresh_fixture(root.path()).await;
        let path = root.path().join("config.json");
        std::fs::write(
            &path,
            json!({"workDir":root.path(),"codexMcp":{"enabled":false}}).to_string(),
        )
        .unwrap();
        Arc::make_mut(&mut state.config).loaded_config_path = Some(path.clone());
        crate::owner_chat::start_schema_refresh(state.config.clone());
        tokio::time::timeout(Duration::from_secs(2), async {
            while crate::global_settings::read_schema_intent(&state.config).is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        // The first edit is through the owner API's save path; the second is an
        // external file edit, like config set or a text editor. Only the last
        // schema should be refreshed, after ten quiet seconds.
        let first = crate::global_settings::snapshot(&state.config).unwrap();
        let saved = crate::global_settings::save(
            &state.config,
            first["revision"].as_str().unwrap(),
            None,
            Some(serde_json::Map::from_iter([(
                "experimental.forceReadOnlyToolAnnotations".into(),
                json!(true),
            )])),
        )
        .unwrap();
        assert_eq!(saved["schemaRefreshRequired"], true);
        assert_eq!(saved["schemaRestartRequired"], false);
        assert_eq!(saved["restartRequired"], false);
        super::super::schema_refresh::queue(state.config.clone(), &saved)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::write(&path,json!({"workDir":root.path(),"codexMcp":{"enabled":false},"experimental":{"forceReadOnlyToolAnnotations":true,"forceIdempotentToolAnnotations":true}}).to_string()).unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let pending = crate::global_settings::read_schema_intent(&state.config).unwrap();
        assert_eq!(pending["phase"], "queued");
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert!(
            crate::global_settings::advertising_config(&state.config)
                .experimental
                .force_read_only_tool_annotations
        );
        tokio::time::timeout(Duration::from_secs(14), async {
            loop {
                if crate::global_settings::read_schema_intent(&state.config)
                    .is_some_and(|v| v["phase"] == "completed")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.successes.load(Ordering::SeqCst), 1);
        owner.abort();
        companion.abort();
    }
    #[tokio::test]
    async fn structural_refresh_waits_for_restart_and_survives_agent_chat_being_disabled() {
        use std::sync::atomic::Ordering;
        let root = tempfile::tempdir().unwrap();
        let (mut state, fixture, _base, owner, companion) = refresh_fixture(root.path()).await;
        let path = root.path().join("config.json");
        std::fs::write(&path,json!({"workDir":root.path(),"agentChat":{"enabled":false},"codexMcp":{"enabled":false}}).to_string()).unwrap();
        let config = Arc::make_mut(&mut state.config);
        config.loaded_config_path = Some(path);
        config.markdown_chat.enabled = false;
        let mut saved = crate::global_settings::snapshot(&state.config).unwrap();
        saved["schemaRestartRequired"] = json!(true);
        super::super::schema_refresh::queue(state.config.clone(), &saved)
            .await
            .unwrap();
        let mut intent = crate::global_settings::read_schema_intent(&state.config).unwrap();
        intent["dueAtMs"] = json!(0);
        crate::global_settings::write_schema_intent(&state.config, &intent).unwrap();
        // A following annotation-only edit must retain the pending restart.
        saved["schemaRestartRequired"] = json!(false);
        super::super::schema_refresh::queue(state.config.clone(), &saved)
            .await
            .unwrap();
        intent = crate::global_settings::read_schema_intent(&state.config).unwrap();
        assert_eq!(intent["phase"], "waiting_for_restart");
        assert_eq!(intent["requiresRestart"], true);
        intent["dueAtMs"] = json!(0);
        crate::global_settings::write_schema_intent(&state.config, &intent).unwrap();
        crate::owner_chat::start_schema_refresh(state.config.clone());
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        // Simulate the new worker after Restart: its PID differs from the writer.
        intent = crate::global_settings::read_schema_intent(&state.config).unwrap();
        intent["originPid"] = json!(0);
        crate::global_settings::write_schema_intent(&state.config, &intent).unwrap();
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                if crate::global_settings::read_schema_intent(&state.config)
                    .is_some_and(|v| v["phase"] == "completed")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fixture.successes.load(Ordering::SeqCst), 1);
        owner.abort();
        companion.abort();
    }
}
