use super::*;
use crate::project_bindings::WorkspaceSelection;
use crate::tools::set_project_root::selection_config;

#[derive(Deserialize)]
pub(super) struct ProjectQuery {
    q: Option<String>,
}

pub(super) async fn projects(
    State(state): State<OwnerState>,
    Query(query): Query<ProjectQuery>,
) -> ApiResult {
    let config = state.config.clone();
    let catalog = tokio::task::spawn_blocking(move || {
        crate::project_catalog::discover_project_catalog(&config)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;
    let listing = catalog.list(query.q.as_deref(), 50);
    Ok(Json(
        json!({"catalog":listing,"createWorktree":state.config.worktrees.mode != crate::types::WorktreeMode::Never}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateProjectBody {
    name: String,
}

pub(super) async fn create_project(
    State(state): State<OwnerState>,
    Json(body): Json<CreateProjectBody>,
) -> ApiResult {
    let name = body.name.trim();
    if name.is_empty()
        || name.len() > 255
        || matches!(name, "." | "..")
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Enter a project folder name without path separators".into(),
        ));
    }
    let (root, _) = crate::project_bindings::resolve_project_root(&state.config, ".")
        .map_err(internal_error)?;
    let path = root.join(name);
    std::fs::create_dir(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            (
                StatusCode::CONFLICT,
                "A file or folder with that name already exists".into(),
            )
        } else {
            internal_error(format!("Could not create project folder: {error}"))
        }
    })?;
    Ok(Json(json!({"path":path,"name":name})))
}

#[derive(Deserialize)]
pub(super) struct WorktreeQuery {
    path: String,
}

pub(super) async fn worktrees(
    State(state): State<OwnerState>,
    Query(query): Query<WorktreeQuery>,
) -> ApiResult {
    let (_, root) = crate::project_bindings::resolve_project_root(&state.config, &query.path)
        .map_err(internal_error)?;
    let rows = crate::worktrees::list_existing_worktrees(&state.config, &root)
        .await
        .map_err(internal_error)?;
    Ok(Json(json!({"sourcePath":root,"worktrees":rows})))
}

pub(super) async fn login(
    State(state): State<OwnerState>,
    Json(_body): Json<serde_json::Map<String, Value>>,
) -> ApiResult {
    let companion = crate::agent_automation::BrowserCompanionManager::from_config(
        &state.config.markdown_chat.automation,
    )
    .map_err(internal_error)?
    .login()
    .await
    .map_err(internal_error)?;
    state.global.lock().await.reset_plan();
    Ok(Json(json!({"authenticated":companion.authenticated})))
}

pub(super) async fn prepare_chat(
    State(state): State<OwnerState>,
    Json(_body): Json<serde_json::Map<String, Value>>,
) -> ApiResult {
    require_connector_name(&state.config)?;
    if !state.config.markdown_chat.automation.enabled {
        return Err(internal_error(
            "Enable agentChat.automation to create ChatGPT conversations",
        ));
    }
    let supervisor = state
        .automation
        .as_ref()
        .ok_or_else(|| internal_error("Browser automation supervisor is unavailable"))?;
    let catalog = supervisor
        .prepare_new_chat(
            state
                .config
                .markdown_chat
                .automation
                .chatgpt_project_url
                .as_deref(),
        )
        .await
        .map_err(internal_error)?;
    let health = companion_client(&state)?
        .health(&CancellationToken::new())
        .await
        .map_err(internal_error)?;
    if !health.authenticated
        || health.account_fingerprint.as_deref() != Some(catalog.account_fingerprint.as_str())
    {
        return Err(internal_error(
            "ChatGPT account changed while preparing the new chat",
        ));
    }
    let entry = OwnerCatalogCache {
        key: catalog_context(&health),
        catalog: catalog.clone(),
    };
    write_catalog_cache(&state, &entry).map_err(internal_error)?;
    state
        .catalog_cache
        .lock()
        .await
        .remember(entry, crate::markdown_chat::now_ms());
    Ok(Json(
        json!({"models":catalog.models,"catalogRevision":catalog.catalog_revision,"chatgptProjectUrl":state.config.markdown_chat.automation.chatgpt_project_url,"workspaceName":state.config.work_dir.file_name().map(|name|name.to_string_lossy()),"createWorktree":state.config.worktrees.mode != crate::types::WorktreeMode::Never}),
    ))
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct CreateBody {
    request_id: String,
    path: Option<String>,
    without_project: Option<bool>,
    create_worktree: Option<bool>,
    resume_path: Option<String>,
    worktree_path: Option<String>,
    model: String,
    effort: String,
    catalog_revision: String,
}

fn creation_identity(
    config: &AppConfig,
    request_id: &str,
) -> Result<ConversationIdentity, (StatusCode, String)> {
    if request_id.is_empty()
        || request_id.len() > 80
        || !request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Invalid new chat request ID".into(),
        ));
    }
    ConversationIdentity::from_openai_session(&format!(
        "standalone-new-chat:{}:{}:{request_id}",
        config.work_dir.display(),
        config.port
    ))
    .ok_or_else(|| internal_error("Invalid new chat identity"))
}

fn located_chat(chat: &crate::markdown_chat::ChatFile) -> LocatedChat {
    let path = chat.path().to_owned();
    let owner = path
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    LocatedChat {
        id: format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes())),
        state_root: path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned(),
        owner,
        path,
        work_dir: None,
        workspace: String::new(),
        is_scratch: false,
    }
}

fn selected_root(selection: WorkspaceSelection) -> PathBuf {
    match selection {
        WorkspaceSelection::Project(project) => project.project_root,
        WorkspaceSelection::WithoutProject(scratch) => scratch.scratch_root,
    }
}

pub(super) async fn create_chat_response(
    State(state): State<OwnerState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateBody>,
) -> Response {
    if headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        != Some("application/x-ndjson")
    {
        return create_chat(State(state), Json(body)).await.into_response();
    }
    // Keep the normal precondition response so the owner UI can request setup.
    if let Err(error) = require_connector_name(&state.config) {
        return error.into_response();
    }
    if let Err(error) = creation_identity(&state.config, &body.request_id) {
        return error.into_response();
    }
    stream_creation(create_chat(State(state), Json(body)))
}

fn stream_creation(
    operation: impl std::future::Future<Output = ApiResult> + Send + 'static,
) -> Response {
    use tokio::io::AsyncWriteExt;
    let (sender, mut progress) = tokio::sync::watch::channel(
        crate::project_progress::Progress::new("Waiting to prepare workspace"),
    );
    let (reader, mut writer) = tokio::io::duplex(16 * 1024);
    let mut operation = tokio::spawn(crate::project_progress::scope(sender, operation));
    tokio::spawn(async move {
        let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
        let mut observing = true;
        loop {
            let (event, finished) = tokio::select! {
                result = &mut operation => {
                    let event = match result {
                        Ok(Ok(Json(result))) => json!({"type":"result","result":result}),
                        Ok(Err((code, message))) => json!({"type":"error","code":code.as_u16(),"message":message}),
                        Err(_) => json!({"type":"error","message":"Workspace preparation stopped unexpectedly. Retry to check the same request."}),
                    };
                    (event, true)
                },
                changed = progress.changed(), if observing => {
                    if changed.is_err() { observing = false; continue }
                    let value = progress.borrow_and_update().clone();
                    (json!({"type":"progress","progress":value}), false)
                },
                _ = heartbeat.tick() => (json!({"type":"heartbeat"}), false),
            };
            let mut bytes = serde_json::to_vec(&event).expect("serializable creation progress");
            bytes.push(b'\n');
            if writer.write_all(&bytes).await.is_err() || finished {
                break;
            }
        }
        // Dropping the JoinHandle leaves preparation running if the browser
        // disconnects; retrying the same request retains the existing identity.
    });
    (
        [
            (header::CONTENT_TYPE, "application/x-ndjson"),
            (header::CACHE_CONTROL, "no-store, no-transform"),
            (
                axum::http::HeaderName::from_static("x-accel-buffering"),
                "no",
            ),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(reader)),
    )
        .into_response()
}

pub(super) async fn create_chat(
    State(state): State<OwnerState>,
    Json(body): Json<CreateBody>,
) -> ApiResult {
    require_connector_name(&state.config)?;
    if !state.config.markdown_chat.automation.enabled || state.automation.is_none() {
        return Err(internal_error(
            "Enable browser automation to create a new chat",
        ));
    }
    let task = creation_identity(&state.config, &body.request_id)?;
    crate::project_progress::phase("Waiting for other workspace preparation");
    let _guard = state.creation_lock.lock().await;
    crate::project_progress::phase("Checking ChatGPT model selection");
    // Model discovery for New chat must be performed on its actual ChatGPT destination.
    let client = companion_client(&state)?;
    let health = client
        .health(&CancellationToken::new())
        .await
        .map_err(internal_error)?;
    let catalog = cached_owner_catalog(&state, &catalog_context(&health))
        .await
        .ok_or_else(|| internal_error("Prepare the new chat to discover available models first"))?;
    if catalog.catalog_revision != body.catalog_revision
        || !catalog.models.iter().any(|model| {
            model.id == body.model
                && model
                    .efforts
                    .iter()
                    .any(|effort| effort.id == body.effort && effort.available)
        })
    {
        return Err(internal_error(
            "The selected model or effort is no longer available; refresh new chat options",
        ));
    }
    let bindings = state.project_bindings.clone();
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&body).map_err(internal_error)?)
    );
    if let Some(previous_root) = bindings
        .selected_project_root(&state.config, &task)
        .map_err(internal_error)?
    {
        let mut previous_config = state.config.as_ref().clone();
        previous_config.work_dir = previous_root;
        AgentAutomationStore::for_config(&previous_config)
            .map_err(internal_error)?
            .record_creation_request(&task, &digest)
            .map_err(internal_error)?;
    }
    let config = selection_config(&state.config, body.create_worktree);
    crate::project_progress::phase("Preparing workspace");
    let root = if let Some(worktree) = &body.worktree_path {
        if body.without_project.is_some() || body.resume_path.is_some() {
            return Err(internal_error(
                "Worktree reuse cannot be combined with another workspace choice",
            ));
        }
        let project = body
            .path
            .as_deref()
            .ok_or_else(|| internal_error("Choose the worktree source project"))?;
        if body.create_worktree == Some(true) {
            bindings
                .select_project_root_from_worktree(&config, &task, project, FilePath::new(worktree))
                .await
                .map_err(internal_error)?
                .project_root
        } else {
            bindings
                .reuse_worktree(&state.config, &task, project, FilePath::new(worktree))
                .await
                .map_err(internal_error)?
                .project_root
        }
    } else {
        match (&body.path, body.without_project, &body.resume_path) {
            (Some(path), None, None) if !path.trim().is_empty() => {
                bindings
                    .select_project_root(&config, &task, path)
                    .await
                    .map_err(internal_error)?
                    .project_root
            }
            (None, Some(true), None) if body.create_worktree.is_none() => {
                bindings
                    .select_without_project(&state.config, &task)
                    .await
                    .map_err(internal_error)?
                    .scratch_root
            }
            (None, None, Some(path)) if body.create_worktree.is_none() => selected_root(
                bindings
                    .resume_workspace(&state.config, &task, path)
                    .await
                    .map_err(internal_error)?,
            ),
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Choose one project, Git URL, saved workspace, or chat without a project"
                        .into(),
                ));
            }
        }
    };
    let mut effective = state.config.as_ref().clone();
    crate::project_progress::phase("Creating local chat");
    effective.work_dir = root.clone();
    let chat = state
        .chats
        .chat(
            &effective,
            Some(&task),
            &crate::exec_sessions::SessionState::new(),
        )
        .map_err(internal_error)?;
    chat.ensure().await.map_err(internal_error)?;
    let store = Arc::new(AgentAutomationStore::for_config(&effective).map_err(internal_error)?);
    store
        .record_creation_request(&task, &digest)
        .map_err(internal_error)?;
    let previous = store.load(&task).map_err(internal_error)?;
    if previous.action.id.is_none() {
        crate::project_progress::phase("Queuing the new ChatGPT conversation");
        queue_creation(
            &state,
            &task,
            &root,
            &store,
            &chat,
            Some((&body.model, &body.effort, &catalog)),
        )
        .await?;
    }
    Ok(Json(json!({"id":located_chat(&chat).id,"queued":true})))
}

async fn queue_creation(
    state: &OwnerState,
    task: &ConversationIdentity,
    root: &FilePath,
    store: &Arc<AgentAutomationStore>,
    chat: &Arc<crate::markdown_chat::ChatFile>,
    selection: Option<(&str, &str, &crate::agent_automation::CapabilityCatalog)>,
) -> Result<(), (StatusCode, String)> {
    let owner = state
        .continuations
        .owner_generation_for_task_key(task.stable_key())
        .map_err(internal_error)?;
    let saved = store.load(task).map_err(internal_error)?;
    if owner != 0 && (!saved.pairing.creating_chat || saved.action.send_accepted != Some(false)) {
        return Err(internal_error(
            "This chat was already created; replace its pairing instead of creating it again",
        ));
    }
    let token = state
        .continuations
        .issue_token_for_owner(task, owner, root)
        .map_err(internal_error)?;
    let now = crate::markdown_chat::now_ms();
    let next_owner = owner
        .checked_add(1)
        .ok_or_else(|| internal_error("Conversation owner generation overflow"))?;
    if let Some((model, effort, catalog)) = selection {
        store
            .set_selection(
                task,
                model,
                effort,
                &catalog.catalog_revision,
                &catalog.account_fingerprint,
                now,
            )
            .map_err(internal_error)?;
    }
    let destination = state
        .config
        .markdown_chat
        .automation
        .chatgpt_project_url
        .as_deref()
        .unwrap_or("https://chatgpt.com/new");
    let issue = store
        .issue_pairing(task, destination, next_owner, now)
        .map_err(internal_error)?;
    store.mark_creating_chat(task).map_err(internal_error)?;
    let supervisor = state
        .automation
        .as_ref()
        .ok_or_else(|| internal_error("Browser automation supervisor is unavailable"))?;
    supervisor
        .register_task(AutomationTaskHandle::new(
            task.clone(),
            next_owner,
            store.clone(),
            chat.clone(),
        ))
        .await
        .map_err(internal_error)?;
    if let Err(error) = supervisor
        .queue_new_chat(
            task,
            &issue.reference,
            issue.generation,
            now,
            token,
            state
                .config
                .conversation_auth_token
                .as_deref()
                .map(str::to_owned),
        )
        .await
    {
        let _ = store.invalidate_pairing(task, issue.generation, now);
        return Err(internal_error(error));
    }
    supervisor.notify();
    Ok(())
}

pub(super) async fn retry_creation(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(_body): Json<OwnerMutationBody>,
) -> ApiResult {
    let _guard = state.creation_lock.lock().await;
    let located = selected(&state, &id)?;
    let context = automation_context(&state, &located).await?;
    let saved = context.store.load(&context.task).map_err(internal_error)?;
    if !saved.pairing.creating_chat || saved.action.send_accepted != Some(false) {
        return Err(internal_error(
            "Creation can only be retried after a confirmed failure before Send",
        ));
    }
    let root = state
        .project_bindings
        .clone()
        .selected_project_root(&state.config, &context.task)
        .map_err(internal_error)?
        .ok_or_else(|| internal_error("The new chat workspace is unavailable"))?;
    queue_creation(
        &state,
        &context.task,
        &root,
        &context.store,
        &context.chat,
        None,
    )
    .await?;
    Ok(Json(json!({"queued":true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_automation::{
        ActionPrompt, AutomationExecution, AutomationExecutionError, AutomationExecutor,
        CapabilityCatalog, CompanionActionSnapshot, CompanionActionState, CompanionEvent,
        CompanionSelection,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    struct CreationExecutor {
        catalog: CapabilityCatalog,
        executions: Mutex<Vec<AutomationExecution>>,
        next_limit: Mutex<Option<bool>>,
        next_failure: Mutex<Option<(&'static str, bool)>>,
        created_url: Mutex<String>,
        reserve_pairing: std::sync::atomic::AtomicBool,
        released_pairings: Mutex<Vec<String>>,
        tickets: Arc<crate::agent_tickets::AgentTicketStore>,
    }
    #[async_trait]
    impl AutomationExecutor for CreationExecutor {
        fn supports_pairing_reservation(&self) -> bool {
            self.reserve_pairing
                .load(std::sync::atomic::Ordering::SeqCst)
        }

        async fn release_pairing_window(&self, action_id: &str) -> Result<(), String> {
            self.released_pairings
                .lock()
                .unwrap()
                .push(action_id.into());
            Ok(())
        }
        async fn prepare_new_chat(
            &self,
            _: Option<&str>,
            _: CancellationToken,
        ) -> Result<CapabilityCatalog, String> {
            Ok(self.catalog.clone())
        }
        async fn execute(
            &self,
            execution: AutomationExecution,
            _: mpsc::UnboundedSender<CompanionEvent>,
            _: CancellationToken,
        ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
            self.executions.lock().unwrap().push(execution.clone());
            if let Some(accepted) = self.next_limit.lock().unwrap().take() {
                return Ok(CompanionActionSnapshot {
                    protocol_version: 1,
                    build: self.catalog.build.clone(),
                    action_id: execution.request.action_id,
                    state: CompanionActionState::Failed,
                    send_accepted: accepted,
                    canonical_url: accepted.then_some(execution.request.expected_url),
                    selected: accepted.then(|| CompanionSelection {
                        model: execution.request.model.unwrap(),
                        effort: execution.request.effort.unwrap(),
                    }),
                    last_event_sequence: 0,
                    cooldown_until_ms: None,
                    attention: Some(crate::agent_automation::CompanionAttention {
                        code: "conversation_limit".into(),
                        message: Some("Conversation length limit".into()),
                    }),
                });
            }
            if let Some((code, accepted)) = self.next_failure.lock().unwrap().take() {
                return Ok(CompanionActionSnapshot {
                    protocol_version: 1,
                    build: self.catalog.build.clone(),
                    action_id: execution.request.action_id,
                    state: CompanionActionState::Failed,
                    send_accepted: accepted,
                    canonical_url: accepted.then_some(execution.request.expected_url),
                    selected: execution
                        .request
                        .model
                        .zip(execution.request.effort)
                        .map(|(model, effort)| CompanionSelection { model, effort }),
                    last_event_sequence: 0,
                    cooldown_until_ms: None,
                    attention: Some(crate::agent_automation::CompanionAttention {
                        code: code.into(),
                        message: Some("Fixture failure".into()),
                    }),
                });
            }
            Ok(CompanionActionSnapshot {
                protocol_version: 1,
                build: self.catalog.build.clone(),
                action_id: execution.request.action_id,
                state: CompanionActionState::Idle,
                send_accepted: true,
                canonical_url: Some(self.created_url.lock().unwrap().clone()),
                selected: execution
                    .request
                    .model
                    .zip(execution.request.effort)
                    .map(|(model, effort)| CompanionSelection { model, effort }),
                last_event_sequence: 0,
                cooldown_until_ms: None,
                attention: None,
            })
        }
    }

    async fn fixture() -> (
        tempfile::TempDir,
        OwnerState,
        Arc<CreationExecutor>,
        tokio::task::JoinHandle<()>,
    ) {
        fixture_with_settings(false).await
    }

    async fn fixture_with_settings(
        settings: bool,
    ) -> (
        tempfile::TempDir,
        OwnerState,
        Arc<CreationExecutor>,
        tokio::task::JoinHandle<()>,
    ) {
        fixture_with_options(settings, false).await
    }

    async fn fixture_with_options(
        settings: bool,
        tickets: bool,
    ) -> (
        tempfile::TempDir,
        OwnerState,
        Arc<CreationExecutor>,
        tokio::task::JoinHandle<()>,
    ) {
        fixture_with_recovery(settings, tickets, false).await
    }

    async fn fixture_with_recovery(
        settings: bool,
        tickets: bool,
        recovery: bool,
    ) -> (
        tempfile::TempDir,
        OwnerState,
        Arc<CreationExecutor>,
        tokio::task::JoinHandle<()>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let project = root.join("projects");
        std::fs::create_dir_all(project.join("repo")).unwrap();
        let mut config = crate::config::default_config(project);
        config
            .markdown_chat
            .automation
            .new_conversation_on_load_rate_limit = recovery;

        config.markdown_chat.enabled = true;
        config.experimental.agent_tickets = tickets;
        config.markdown_chat.automation.enabled = true;
        config.markdown_chat.automation.connector_name = "Codexify".into();
        config.memory.dir = Some(root.join("metadata").display().to_string());
        config.markdown_chat.automation.descriptor_path =
            root.join("descriptor.json").display().to_string();
        config.markdown_chat.automation.chatgpt_project_url =
            Some("https://chatgpt.com/g/g-p-0123456789abcdef0123456789abcdef/project".into());
        if settings {
            let path = root.join("settings.json");
            std::fs::write(
                &path,
                serde_json::to_vec(&json!({"agentChat":{"automation":{"connectorName":"Codexify"}},"experimental":{"agentTickets":tickets}})).unwrap(),
            )
            .unwrap();
            config.loaded_config_path = Some(path);
            let revision = crate::global_settings::schema_revision(&config).unwrap();
            crate::global_settings::write_schema_intent(
                &config,
                &json!({"phase":"idle","observedRevision":revision,
                    "observedConnectorSchemaVersion":crate::server::automation_schema_version(
                        &crate::registry::load_tools_for_config(&config), &config)}),
            )
            .unwrap();
        }
        let build = format!("fixture{}", crate::agent_automation::PROTOCOL_BUILD_SUFFIX);
        let catalog:CapabilityCatalog=serde_json::from_value(json!({"protocolVersion":1,"build":build,"accountFingerprint":"creation-account","catalogRevision":"creation-catalog","models":[{"id":"discovered-model","label":"Discovered model","efforts":[{"id":"adaptive-choice","label":"Adaptive choice","available":true,"reason":null}]}]})).unwrap();
        let health = json!({"protocolVersion":1,"kind":"codexify-browser-companion","build":build,"status":"idle","authenticated":true,"accountFingerprint":"creation-account","activeActionId":null});
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route(
            "/v1/health",
            get(move || {
                let health = health.clone();
                async move { Json(health) }
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        std::fs::write(&config.markdown_chat.automation.descriptor_path,serde_json::to_vec(&json!({"version":1,"kind":"codexify-browser-companion","pid":std::process::id(),"endpoint":format!("http://127.0.0.1:{port}"),"token":"A".repeat(43),"build":build,"createdAt":"2026-10-12T00:00:00Z"})).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                &config.markdown_chat.automation.descriptor_path,
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        let tickets = Arc::new(crate::agent_tickets::AgentTicketStore::default());
        let executor = Arc::new(CreationExecutor {
            catalog,
            executions: Mutex::new(Vec::new()),
            next_limit: Mutex::new(None),
            next_failure: Mutex::new(None),
            created_url: Mutex::new("https://chatgpt.com/c/created-chat".into()),
            reserve_pairing: Default::default(),
            released_pairings: Default::default(),
            tickets: tickets.clone(),
        });
        let supervisor = AgentAutomationSupervisor::new_with_pairing_wait_timeout(
            config.markdown_chat.automation.clone(),
            executor.clone(),
            Duration::from_millis(750),
        );
        let state = OwnerState {
            config: Arc::new(config.clone()),
            chats: Arc::default(),
            project_bindings: Arc::new(crate::project_bindings::ProjectBindingStore::new(
                root.join("bindings"),
            )),
            creation_lock: Arc::default(),
            artifacts: Arc::new(ArtifactEgressStore::new_at(
                config.artifact_egress.clone(),
                root.join("artifacts"),
            )),
            continuations: Arc::new(
                ConversationContinuationStore::new(root.join("continuations.json")).unwrap(),
            ),
            automation: Some(supervisor),
            open_browser: Arc::new(|_| Ok(())),
            catalog_cache: Arc::default(),
            global: Arc::default(),
            token: "owner-test".into(),
        };
        state
            .automation
            .as_ref()
            .unwrap()
            .enable_conversation_rollover(
                state.config.clone(),
                state.project_bindings.clone(),
                state.continuations.clone(),
                Arc::new(crate::registry::load_tools_for_config(&state.config)),
                tickets,
            )
            .unwrap();
        (temp, state, executor, task)
    }

    fn body(id: &str, scratch: bool) -> CreateBody {
        CreateBody {
            request_id: id.into(),
            path: (!scratch).then(|| "repo".into()),
            without_project: scratch.then_some(true),
            create_worktree: (!scratch).then_some(false),
            resume_path: None,
            worktree_path: None,
            model: "discovered-model".into(),
            effort: "adaptive-choice".into(),
            catalog_revision: "creation-catalog".into(),
        }
    }

    async fn schema_source() -> (
        tempfile::TempDir,
        OwnerState,
        Arc<CreationExecutor>,
        tokio::task::JoinHandle<()>,
        Value,
        OwnerAutomationContext,
        ConversationIdentity,
    ) {
        schema_source_with_recovery(false).await
    }

    async fn schema_source_with_recovery(
        recovery: bool,
    ) -> (
        tempfile::TempDir,
        OwnerState,
        Arc<CreationExecutor>,
        tokio::task::JoinHandle<()>,
        Value,
        OwnerAutomationContext,
        ConversationIdentity,
    ) {
        let (temp, state, executor, server) = fixture_with_recovery(true, false, recovery).await;
        let _ = prepare_chat(State(state.clone()), Json(Default::default()))
            .await
            .unwrap();
        let created = create_chat(State(state.clone()), Json(body("schema-source", false)))
            .await
            .unwrap()
            .0;
        let located = selected(&state, created["id"].as_str().unwrap()).unwrap();
        let context = automation_context(&state, &located).await.unwrap();
        state
            .automation
            .as_ref()
            .unwrap()
            .run_next(crate::markdown_chat::now_ms())
            .await
            .unwrap()
            .unwrap();
        let execution = executor.executions.lock().unwrap()[0].clone();
        let ActionPrompt::CreateChat {
            continuation_token,
            reference,
            ..
        } = execution.request.prompt
        else {
            panic!("creation expected")
        };
        let source = ConversationIdentity::from_openai_session("schema-source-owner").unwrap();
        state
            .continuations
            .claim(&source, &continuation_token, |_, _| Ok(()))
            .unwrap();
        context
            .store
            .verify_pairing_reference(&context.task, 1, &reference, crate::markdown_chat::now_ms())
            .unwrap();
        let context = automation_context(&state, &located).await.unwrap();
        (temp, state, executor, server, created, context, source)
    }

    #[tokio::test]
    async fn connector_schema_changes_during_creation_do_not_relabel_the_conversation() {
        let (_temp, state, executor, server) = fixture_with_settings(true).await;
        let _ = prepare_chat(State(state.clone()), Json(Default::default()))
            .await
            .unwrap();
        let created = create_chat(State(state.clone()), Json(body("schema-in-flight", false)))
            .await
            .unwrap()
            .0;
        let located = selected(&state, created["id"].as_str().unwrap()).unwrap();
        let context = automation_context(&state, &located).await.unwrap();
        let supervisor = state.automation.as_ref().unwrap();
        supervisor
            .run_next(crate::markdown_chat::now_ms())
            .await
            .unwrap()
            .unwrap();
        let before = context
            .store
            .load(&context.task)
            .unwrap()
            .pairing
            .connector_schema_version;
        change_schema(&state, true);
        let execution = executor.executions.lock().unwrap()[0].clone();
        let ActionPrompt::CreateChat {
            continuation_token,
            reference,
            ..
        } = execution.request.prompt
        else {
            panic!("creation expected")
        };
        let physical =
            ConversationIdentity::from_openai_session("created-with-old-schema").unwrap();
        state
            .continuations
            .claim(&physical, &continuation_token, |_, _| Ok(()))
            .unwrap();
        context
            .store
            .verify_pairing_reference(&context.task, 1, &reference, crate::markdown_chat::now_ms())
            .unwrap();
        let _ = automation_context(&state, &located).await.unwrap();
        assert_eq!(
            context
                .store
                .load(&context.task)
                .unwrap()
                .pairing
                .connector_schema_version,
            before
        );
        assert_ne!(supervisor.current_schema_version(), before);
        assert!(
            supervisor
                .prepare_schema_rollover(&context.task)
                .await
                .unwrap()
        );
        supervisor
            .scan_once(crate::markdown_chat::now_ms())
            .await
            .unwrap();
        assert_eq!(supervisor.queued_count().await, 1);
        server.abort();
    }

    #[tokio::test]
    async fn re_pairing_keeps_the_window_for_its_first_prompt_and_releases_on_stop_or_timeout() {
        use std::sync::atomic::Ordering;
        for decision in [
            "verified",
            "goal",
            "continuation",
            "timeout",
            "stop",
            "superseded",
        ] {
            let (_temp, state, executor, server, _created, context, _source) =
                schema_source().await;
            let supervisor = state.automation.as_ref().unwrap().clone();
            // Queue an unrelated chat before the replacement is created.
            *executor.created_url.lock().unwrap() = "https://chatgpt.com/c/competing-chat".into();
            let created = create_chat(State(state.clone()), Json(body("competing-chat", false)))
                .await
                .unwrap()
                .0;
            let located = selected(&state, created["id"].as_str().unwrap()).unwrap();
            let other = automation_context(&state, &located).await.unwrap();
            supervisor
                .run_next(crate::markdown_chat::now_ms())
                .await
                .unwrap()
                .unwrap();
            let initial = executor.executions.lock().unwrap().last().unwrap().clone();
            assert!(
                !initial.request.retain_pairing_window,
                "ordinary new chats do not reserve a follow-up"
            );
            let ActionPrompt::CreateChat {
                continuation_token,
                reference,
                ..
            } = initial.request.prompt
            else {
                unreachable!()
            };
            let physical = ConversationIdentity::from_openai_session("competing-owner").unwrap();
            state
                .continuations
                .claim(&physical, &continuation_token, |_, _| Ok(()))
                .unwrap();
            other
                .store
                .verify_pairing_reference(
                    &other.task,
                    1,
                    &reference,
                    crate::markdown_chat::now_ms(),
                )
                .unwrap();
            let _ = automation_context(&state, &located).await.unwrap();
            other
                .chat
                .append_user("other-request".into(), "Handle the other task".into())
                .await
                .unwrap();
            let now = crate::markdown_chat::now_ms()
                + crate::agent_automation::PAIRING_VERIFICATION_GRACE_MS
                + 1000;
            supervisor.scan_once(now).await.unwrap();
            assert_eq!(supervisor.queued_count().await, 1);
            if decision == "goal" {
                let goal = context
                    .store
                    .set_goal(&context.task, "Resume this goal", now)
                    .unwrap();
                context
                    .store
                    .run_goal(&context.task, goal.revision, now)
                    .unwrap();
            } else if decision == "continuation" {
                let path = context.chat.path().with_file_name("automation.json");
                let mut value: Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                value["pairing"]["resumeTask"] = json!(true);
                std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            } else {
                context
                    .chat
                    .append_user("follow-up".into(), "Continue my request".into())
                    .await
                    .unwrap();
            }
            executor.reserve_pairing.store(true, Ordering::SeqCst);
            change_schema(&state, true);
            assert!(
                supervisor
                    .prepare_schema_rollover(&context.task)
                    .await
                    .unwrap()
            );
            *executor.created_url.lock().unwrap() = "https://chatgpt.com/c/retained-pair".into();
            supervisor.scan_once(now).await.unwrap();
            let before = executor.executions.lock().unwrap().len();
            let running = tokio::spawn({
                let supervisor = supervisor.clone();
                async move { supervisor.run_next(crate::markdown_chat::now_ms()).await }
            });
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if executor.executions.lock().unwrap().len() > before
                        && context
                            .store
                            .load(&context.task)
                            .unwrap()
                            .pairing
                            .canonical_url
                            .is_some()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            let repair = executor.executions.lock().unwrap()[before].clone();
            assert!(repair.request.retain_pairing_window);
            let competing = tokio::spawn({
                let supervisor = supervisor.clone();
                async move { supervisor.run_next(crate::markdown_chat::now_ms()).await }
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(
                !competing.is_finished(),
                "another queue runner must wait for the handoff"
            );
            match decision {
                "verified" | "goal" | "continuation" => {
                    let ActionPrompt::CreateChat {
                        continuation_token,
                        reference,
                        ..
                    } = &repair.request.prompt
                    else {
                        unreachable!()
                    };
                    let destination =
                        ConversationIdentity::from_openai_session("retained-new-owner").unwrap();
                    state
                        .continuations
                        .claim(&destination, continuation_token, |_, _| Ok(()))
                        .unwrap();
                    context
                        .store
                        .verify_pairing_reference(
                            &context.task,
                            2,
                            reference,
                            crate::markdown_chat::now_ms(),
                        )
                        .unwrap();
                    supervisor.notify();
                }
                "stop" => {
                    context
                        .chat
                        .request_agent_stop("stop-repair".into())
                        .await
                        .unwrap();
                    supervisor.notify();
                }
                "superseded" => {
                    context
                        .store
                        .issue_pairing(
                            &context.task,
                            "https://chatgpt.com/c/manual-replacement",
                            1,
                            crate::markdown_chat::now_ms(),
                        )
                        .unwrap();
                    supervisor.notify();
                }
                "timeout" => {}
                _ => unreachable!(),
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), running)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(1), competing)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let executions = executor.executions.lock().unwrap().clone();
            assert_eq!(
                executor.released_pairings.lock().unwrap().as_slice(),
                std::slice::from_ref(&repair.request.action_id)
            );
            if matches!(decision, "verified" | "goal" | "continuation") {
                let follow_up = &executions[before + 1];
                assert_eq!(follow_up.task_key, context.task.stable_key());
                assert_eq!(
                    follow_up.request.pairing_continuation_of.as_deref(),
                    Some(repair.request.action_id.as_str())
                );
                assert_eq!(follow_up.request.url, "https://chatgpt.com/c/retained-pair");
                assert!(!follow_up.request.retain_pairing_window);
                assert!(matches!(
                    (&follow_up.request.prompt, decision),
                    (ActionPrompt::OfflineChat { .. }, "verified")
                        | (ActionPrompt::OfflineGoal { .. }, "goal")
                        | (ActionPrompt::TaskContinuation {}, "continuation")
                ));
            } else {
                assert_eq!(
                    executions.len(),
                    before + 1,
                    "unverified/stopped/superseded pairing must not send task work"
                );
            }
            assert_eq!(
                other.chat.unread_snapshot().await.unwrap().messages.len(),
                1
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn browser_messages_supply_fresh_valid_tickets_for_initialization_messages_and_retries() {
        let (_temp, state, executor, server) = fixture_with_options(true, true).await;
        let supervisor = state.automation.as_ref().unwrap();
        let _ = prepare_chat(State(state.clone()), Json(Default::default()))
            .await
            .unwrap();
        let created = create_chat(State(state.clone()), Json(body("ticketed-browser", false)))
            .await
            .unwrap()
            .0;
        let located = selected(&state, created["id"].as_str().unwrap()).unwrap();
        let context = automation_context(&state, &located).await.unwrap();
        supervisor
            .run_next(crate::markdown_chat::now_ms())
            .await
            .unwrap()
            .unwrap();
        let initial = executor.executions.lock().unwrap()[0].clone();
        let first_ticket = initial.request.agent_ticket.unwrap();
        let ActionPrompt::CreateChat {
            continuation_token,
            reference,
            ..
        } = initial.request.prompt
        else {
            unreachable!()
        };
        let physical = ConversationIdentity::from_openai_session("ticketed-browser-owner").unwrap();
        state
            .continuations
            .claim(&physical, &continuation_token, |_, _| Ok(()))
            .unwrap();
        let session = crate::exec_sessions::SessionState::new();
        let latest = executor
            .tickets
            .reserve_for_call(
                crate::agent_tickets::TicketCall {
                    conversation: Some(&physical),
                    task: Some(&context.task),
                    owner_generation: Some(1),
                    tool: "automation_verify_pairing",
                },
                &session,
                Some(first_ticket.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .commit(CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        context
            .store
            .verify_pairing_reference(&context.task, 1, &reference, crate::markdown_chat::now_ms())
            .unwrap();
        let _ = automation_context(&state, &located).await.unwrap();
        let mut previous = first_ticket;
        let mut latest = latest;
        for (index, retry) in [false, true, false].into_iter().enumerate() {
            context
                .chat
                .append_user(format!("ticket-input-{index}"), "Resume my task".into())
                .await
                .unwrap();
            let now = crate::markdown_chat::now_ms()
                + crate::agent_automation::PAIRING_VERIFICATION_GRACE_MS
                + 1000;
            supervisor.scan_once(now).await.unwrap();
            if retry {
                let action = context
                    .store
                    .load(&context.task)
                    .unwrap()
                    .action
                    .id
                    .unwrap();
                context
                    .store
                    .update_action_status(
                        &context.task,
                        &action,
                        crate::agent_automation::AutomationActionStatus::Failed,
                        Some("confirmed unsent"),
                        None,
                        now,
                    )
                    .unwrap();
                let _ = supervisor
                    .retry_failed_action(&context.task, &action, now + 1)
                    .await
                    .unwrap();
            }
            supervisor.run_next(now + 2).await.unwrap().unwrap();
            let sent = executor.executions.lock().unwrap().last().unwrap().clone();
            let supplied = sent.request.agent_ticket.unwrap();
            assert_ne!(supplied, previous);
            // The previous chain remains usable until the fresh prompt is redeemed.
            latest = executor
                .tickets
                .reserve(Some(&physical), &session, Some(latest))
                .await
                .unwrap()
                .commit(CancellationToken::new())
                .await
                .unwrap()
                .unwrap();
            let old = latest;
            latest = executor
                .tickets
                .reserve_for_call(
                    crate::agent_tickets::TicketCall {
                        conversation: Some(&physical),
                        task: Some(&context.task),
                        owner_generation: Some(1),
                        tool: "chat_read",
                    },
                    &session,
                    Some(supplied.clone()),
                    CancellationToken::new(),
                )
                .await
                .unwrap()
                .commit(CancellationToken::new())
                .await
                .unwrap()
                .unwrap();
            assert!(
                executor
                    .tickets
                    .reserve(Some(&physical), &session, Some(old))
                    .await
                    .is_err()
            );
            context.chat.read(true).await.unwrap();
            previous = supplied;
        }
        server.abort();
    }

    fn change_schema(state: &OwnerState, refreshed: bool) {
        let previous = crate::global_settings::schema_revision(&state.config).unwrap();
        let path = state.config.loaded_config_path.as_ref().unwrap();
        let mut document: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        document["experimental"]["forceReadOnlyToolAnnotations"] = json!(true);
        std::fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
        let revision = crate::global_settings::schema_revision(&state.config).unwrap();
        assert_ne!(previous, revision);
        crate::global_settings::write_schema_intent(&state.config, &json!({
            "phase": if refreshed {"completed"} else {"queued"},
            "revision": revision,
            "observedRevision": if refreshed {revision} else {previous},
            "observedConnectorSchemaVersion": if refreshed {state.automation.as_ref().unwrap().current_schema_version()} else {None},
        })).unwrap();
    }

    #[tokio::test]
    async fn connector_schema_rollover_from_idle_legacy_pairing_queues_and_delivers_the_wake() {
        let (_temp, state, executor, server, _created, context, _source) = schema_source().await;
        let supervisor = state.automation.as_ref().unwrap();
        let path = context.chat.path().with_file_name("automation.json");
        let mut saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        saved["action"] =
            serde_json::to_value(crate::agent_automation::AutomationActionState::default())
                .unwrap();
        saved["pairing"]["connectorSchemaVersion"] = Value::Null;
        std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
        context
            .chat
            .append_user(
                "idle-schema-message".into(),
                "Continue from the idle conversation".into(),
            )
            .await
            .unwrap();
        assert!(
            supervisor
                .prepare_schema_rollover(&context.task)
                .await
                .unwrap()
        );
        let before = context.store.load(&context.task).unwrap();
        assert_eq!(before.pairing.generation, 1);
        assert_eq!(before.action.pairing_generation, None);
        assert!(before.pairing.schema_rollover_pending);

        let now = crate::markdown_chat::now_ms();
        supervisor.scan_once(now).await.unwrap();
        assert_eq!(
            supervisor.queued_count().await,
            1,
            "an idle action must not strand schema rollover"
        );
        *executor.created_url.lock().unwrap() =
            "https://chatgpt.com/c/idle-schema-replacement".into();
        supervisor.run_next(now + 1).await.unwrap().unwrap();
        let execution = executor.executions.lock().unwrap()[1].clone();
        let ActionPrompt::CreateChat {
            continuation_token,
            reference,
            ..
        } = execution.request.prompt
        else {
            panic!("a fresh conversation must precede the wake");
        };
        let destination = ConversationIdentity::from_openai_session("idle-schema-owner").unwrap();
        state
            .continuations
            .claim(&destination, &continuation_token, |_, _| Ok(()))
            .unwrap();
        context
            .store
            .verify_pairing_reference(&context.task, 2, &reference, now + 2)
            .unwrap();
        let ready = context.store.load(&context.task).unwrap();
        assert!(!ready.pairing.schema_rollover_pending);
        assert_eq!(ready.selection, before.selection);
        let next = now + crate::agent_automation::PAIRING_VERIFICATION_GRACE_MS + 1_000;
        supervisor.scan_once(next).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 1);
        assert_eq!(
            supervisor.next_queued_kind().await,
            Some(crate::agent_automation::AutomationActionKind::OfflineChat)
        );
        supervisor.run_next(next + 1).await.unwrap().unwrap();
        let execution = executor.executions.lock().unwrap()[2].clone();
        let ActionPrompt::OfflineChat { messages, .. } = execution.request.prompt else {
            panic!("the original message must wake the replacement conversation");
        };
        assert_eq!(messages.len(), 1);
        assert!(
            messages[0]
                .markdown
                .contains("Continue from the idle conversation")
        );
        server.abort();
    }

    #[tokio::test]
    async fn connector_schema_rollover_holds_online_input_until_refresh_and_preserves_owner_data() {
        for mode in [
            crate::markdown_chat::MessageMode::Steer,
            crate::markdown_chat::MessageMode::Queue,
        ] {
            let (_temp, state, executor, server, created, context, source) = schema_source().await;
            let supervisor = state.automation.as_ref().unwrap();
            let before = context.store.load(&context.task).unwrap();
            assert!(before.pairing.connector_schema_version.is_some());
            context
                .chat
                .record_agent_call(crate::markdown_chat::now_ms())
                .await
                .unwrap();
            change_schema(&state, false);
            let _ = super::super::chat_send(
                State(state.clone()),
                Path(created["id"].as_str().unwrap().into()),
                Json(SendBody {
                    request_id: "new-schema-input".into(),
                    message: "Use the refreshed tools".into(),
                    stop_agent: false,
                    attachment_ids: vec![],
                    message_mode: Some(mode),
                }),
            )
            .await
            .unwrap();
            let held = context.store.load(&context.task).unwrap();
            assert!(held.pairing.schema_rollover_pending);
            assert_eq!(
                held.pairing.connector_schema_version,
                before.pairing.connector_schema_version
            );
            assert!(
                context
                    .chat
                    .read(true)
                    .await
                    .err()
                    .unwrap()
                    .contains("connector schema changed")
            );
            assert!(
                context
                    .chat
                    .append("late old-owner response".into())
                    .await
                    .is_err()
            );
            let now = crate::markdown_chat::now_ms();
            supervisor.scan_once(now).await.unwrap();
            assert_eq!(
                supervisor.queued_count().await,
                0,
                "creation must wait for connector refresh"
            );
            assert_eq!(executor.executions.lock().unwrap().len(), 1);
            let revision = crate::global_settings::schema_revision(&state.config).unwrap();
            crate::global_settings::write_schema_intent(
                &state.config,
                &json!({"phase":"completed","observedRevision":revision,"revision":revision,
                    "observedConnectorSchemaVersion":supervisor.current_schema_version()}),
            )
            .unwrap();
            supervisor.scan_once(now + 1).await.unwrap();
            supervisor.scan_once(now + 2).await.unwrap();
            assert_eq!(supervisor.queued_count().await, 1);
            *executor.created_url.lock().unwrap() =
                "https://chatgpt.com/c/schema-replacement".into();
            supervisor.run_next(now + 3).await.unwrap().unwrap();
            let execution = executor.executions.lock().unwrap()[1].clone();
            let ActionPrompt::CreateChat {
                continuation_token,
                reference,
                ..
            } = execution.request.prompt
            else {
                panic!("replacement expected")
            };
            let replacing = context.store.load(&context.task).unwrap();
            assert!(replacing.pairing.schema_rollover_pending);
            assert_ne!(
                replacing.pairing.connector_schema_version,
                before.pairing.connector_schema_version
            );
            assert_eq!(replacing.selection, before.selection);
            assert_eq!(replacing.action.automatic_retry_attempts, 0);
            let destination =
                ConversationIdentity::from_openai_session("schema-replacement-owner").unwrap();
            state
                .continuations
                .claim(&destination, &continuation_token, |task, _| {
                    assert_eq!(task.stable_key(), context.task.stable_key());
                    Ok(())
                })
                .unwrap();
            context
                .store
                .verify_pairing_reference(&context.task, 2, &reference, now + 4)
                .unwrap();
            assert!(!state.continuations.owner_snapshot(&source).unwrap().active);
            let final_state = context.store.load(&context.task).unwrap();
            assert!(!final_state.pairing.schema_rollover_pending);
            if mode == crate::markdown_chat::MessageMode::Queue {
                context.chat.release_queued_message(u64::MAX).await.unwrap();
            }
            let unread = context.chat.unread_snapshot().await.unwrap();
            assert_eq!(unread.messages.len(), 1);
            assert_eq!(unread.messages[0].id, "new-schema-input");
            assert!(
                !supervisor
                    .prepare_schema_rollover(&context.task)
                    .await
                    .unwrap()
            );
            assert_eq!(
                executor.executions.lock().unwrap().len(),
                2,
                "one replacement, no old-conversation Send"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn connector_schema_check_covers_queued_sends_goals_retries_renewals_and_legacy_pairings()
    {
        for scenario in ["queued", "goal", "retry", "renewal", "legacy", "matching"] {
            let (_temp, state, executor, server, _created, context, _source) =
                schema_source().await;
            let supervisor = state.automation.as_ref().unwrap();
            let now = crate::markdown_chat::now_ms()
                + crate::agent_automation::PAIRING_VERIFICATION_GRACE_MS
                + 1_000;
            if scenario == "goal" {
                let goal = context
                    .store
                    .set_goal(&context.task, "Continue the goal", now)
                    .unwrap();
                context
                    .store
                    .run_goal(&context.task, goal.revision, now)
                    .unwrap();
            } else if scenario == "renewal" {
                let mut document: Value = serde_json::from_slice(
                    &std::fs::read(context.chat.path().with_file_name("automation.json")).unwrap(),
                )
                .unwrap();
                document["turnRenewal"] = json!({"send":{"actionId":"old-turn","atMs":now-60_000,
                    "model":"gpt-5.6-sol","effort":"max","pairingGeneration":1,"ownerGeneration":1},
                    "handoff":{"id":"schema-handoff","requestedAtMs":now,"reportComplete":true}});
                std::fs::write(
                    context.chat.path().with_file_name("automation.json"),
                    serde_json::to_vec(&document).unwrap(),
                )
                .unwrap();
                // Renewal settings are live and independent of the connector schema.
                let path = state.config.loaded_config_path.as_ref().unwrap();
                let mut settings: Value =
                    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                settings["agentChat"]["turnRenewal"]["enabled"] = json!(true);
                std::fs::write(path, serde_json::to_vec(&settings).unwrap()).unwrap();
            } else {
                context
                    .chat
                    .append_user("schema-unread".into(), "Preserve this message".into())
                    .await
                    .unwrap();
            }
            supervisor.scan_once(now).await.unwrap();
            assert_eq!(supervisor.queued_count().await, 1, "{scenario}");
            if scenario == "renewal" {
                assert_eq!(
                    supervisor.next_queued_kind().await,
                    Some(crate::agent_automation::AutomationActionKind::TurnRenewal)
                );
            }
            if scenario == "matching" {
                supervisor.run_next(now).await.unwrap().unwrap();
                assert!(matches!(
                    executor.executions.lock().unwrap()[1].request.prompt,
                    ActionPrompt::OfflineChat { .. }
                ));
                server.abort();
                continue;
            }
            if scenario == "legacy" {
                let path = context.chat.path().with_file_name("automation.json");
                let mut value: Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                value["pairing"]
                    .as_object_mut()
                    .unwrap()
                    .remove("connectorSchemaVersion");
                std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            } else {
                change_schema(&state, true);
            }
            *executor.created_url.lock().unwrap() =
                format!("https://chatgpt.com/c/schema-{scenario}-replacement");
            if scenario == "retry" {
                let action_id = context
                    .store
                    .load(&context.task)
                    .unwrap()
                    .action
                    .id
                    .unwrap();
                context
                    .store
                    .update_action_status(
                        &context.task,
                        &action_id,
                        crate::agent_automation::AutomationActionStatus::Failed,
                        Some("unsent"),
                        None,
                        now,
                    )
                    .unwrap();
                supervisor
                    .retry_failed_action(&context.task, &action_id, now + 1)
                    .await
                    .unwrap();
            } else {
                assert!(supervisor.run_next(now + 1).await.unwrap().is_none());
            }
            assert_eq!(
                executor.executions.lock().unwrap().len(),
                1,
                "{scenario}: stale conversation must receive no browser Send"
            );
            assert_eq!(supervisor.queued_count().await, 1, "{scenario}");
            assert!(
                context
                    .store
                    .load(&context.task)
                    .unwrap()
                    .pairing
                    .creating_chat,
                "{scenario}"
            );
            supervisor.run_next(now + 2).await.unwrap().unwrap();
            assert!(
                matches!(
                    executor.executions.lock().unwrap()[1].request.prompt,
                    ActionPrompt::CreateChat { .. }
                ),
                "{scenario}"
            );
            if scenario == "renewal" {
                let execution = executor.executions.lock().unwrap()[1].clone();
                let ActionPrompt::CreateChat {
                    continuation_token,
                    reference,
                    ..
                } = execution.request.prompt
                else {
                    unreachable!()
                };
                let physical =
                    ConversationIdentity::from_openai_session("renewal-new-owner").unwrap();
                state
                    .continuations
                    .claim(&physical, &continuation_token, |_, _| Ok(()))
                    .unwrap();
                context
                    .store
                    .verify_pairing_reference(&context.task, 2, &reference, now + 3)
                    .unwrap();
                let _ = automation_context(
                    &state,
                    &selected(&state, _created["id"].as_str().unwrap()).unwrap(),
                )
                .await
                .unwrap();
                assert!(
                    context
                        .store
                        .load(&context.task)
                        .unwrap()
                        .pairing
                        .resume_task
                );
                supervisor
                    .scan_once(now + crate::agent_automation::PAIRING_VERIFICATION_GRACE_MS + 5)
                    .await
                    .unwrap();
                assert_eq!(
                    supervisor.queued_count().await,
                    1,
                    "renewal without a goal must resume after pairing"
                );
                supervisor
                    .run_next(now + crate::agent_automation::PAIRING_VERIFICATION_GRACE_MS + 6)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(matches!(
                    executor.executions.lock().unwrap()[2].request.prompt,
                    ActionPrompt::TaskContinuation {}
                ));
                assert!(
                    !context
                        .store
                        .load(&context.task)
                        .unwrap()
                        .pairing
                        .resume_task
                );
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn creation_progress_streams_before_completion_and_preserves_the_result() {
        let (advance, wait) = tokio::sync::oneshot::channel::<()>();
        let wait = Arc::new(Mutex::new(Some(wait)));
        let app = Router::new().route(
            "/",
            get(move || {
                let wait = wait.lock().unwrap().take().unwrap();
                async move {
                    stream_creation(async move {
                        crate::project_progress::phase("Cloning repository");
                        crate::project_progress::detail(
                            "Receiving objects: 42% (42/100)",
                            Some(42),
                        );
                        wait.await.unwrap();
                        Ok(Json(json!({"id":"created-chat","queued":true})))
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut response = crate::tls::client_builder()
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/x-ndjson"
        );
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !String::from_utf8_lossy(&bytes).contains("Receiving objects") {
                bytes.extend_from_slice(&response.chunk().await.unwrap().unwrap());
            }
        })
        .await
        .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("created-chat"));
        advance.send(()).unwrap();
        while let Some(chunk) = response.chunk().await.unwrap() {
            bytes.extend_from_slice(&chunk);
        }
        let events = String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .any(|event| event["progress"]["percent"] == 42)
        );
        assert_eq!(
            events.last().unwrap()["result"],
            json!({"id":"created-chat","queued":true})
        );
        server.abort();
    }

    #[tokio::test]
    async fn streamed_creation_errors_are_terminal_and_keep_authentication_preconditions() {
        let response = stream_creation(async {
            Err((
                StatusCode::CONFLICT,
                "Clone destination already exists".into(),
            ))
        });
        let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024)
            .await
            .unwrap();
        let events = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.last().unwrap()["type"], "error");
        assert_eq!(
            events.last().unwrap()["message"],
            "Clone destination already exists"
        );

        let (_temp, mut state, _, server) = fixture().await;
        Arc::make_mut(&mut state.config)
            .markdown_chat
            .automation
            .connector_name
            .clear();
        let headers = axum::http::HeaderMap::from_iter([(
            header::ACCEPT,
            "application/x-ndjson".parse().unwrap(),
        )]);
        let response = create_chat_response(
            State(state),
            headers,
            Json(body("missing-connector-stream", true)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);
        server.abort();
    }

    #[tokio::test]
    async fn streamed_creation_builds_a_worktree_and_copies_included_files() {
        let (temp, mut state, _, server) = fixture().await;
        Arc::make_mut(&mut state.config).worktrees.root = temp.path().join("worktrees");
        let source = state.config.work_dir.join("repo");
        std::fs::write(source.join(".gitignore"), "*.bin\n").unwrap();
        std::fs::write(source.join(".worktreeinclude"), "*.bin\n").unwrap();
        std::fs::write(source.join("cache.bin"), vec![7; 600_000]).unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["add", ".gitignore", ".worktreeinclude"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&source)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let _ = prepare_chat(State(state.clone()), Json(Default::default()))
            .await
            .unwrap();
        let mut request = body("streamed-worktree", false);
        request.create_worktree = Some(true);
        let headers = axum::http::HeaderMap::from_iter([(
            header::ACCEPT,
            "application/x-ndjson".parse().unwrap(),
        )]);
        let response = create_chat_response(State(state.clone()), headers, Json(request)).await;
        let bytes = axum::body::to_bytes(response.into_body(), 128 * 1024)
            .await
            .unwrap();
        let events = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.last().unwrap()["type"], "result", "{events:?}");
        assert!(events.iter().any(|event| event["type"] == "progress"));
        let task = creation_identity(&state.config, "streamed-worktree").unwrap();
        let selected = state
            .project_bindings
            .selected_project_root(&state.config, &task)
            .unwrap()
            .unwrap();
        assert_ne!(selected, source);
        assert_eq!(
            std::fs::read(selected.join("cache.bin")).unwrap(),
            vec![7; 600_000]
        );
        server.abort();
    }

    #[tokio::test]
    async fn new_projects_create_only_new_folders_under_the_access_root() {
        let (temp, state, executor, server) = fixture().await;
        let app = router(state.clone());
        use tower::ServiceExt;
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::post("/api/projects")
                    .header("host", "127.0.0.1:3120")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"name":"unauthorized"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!state.config.work_dir.join("unauthorized").exists());

        for name in [
            "",
            " ",
            ".",
            "..",
            "../escape",
            "a/b",
            "a\\b",
            "/absolute",
            "C:drive",
            "bad\nname",
        ] {
            let error = create_project(
                State(state.clone()),
                Json(CreateProjectBody { name: name.into() }),
            )
            .await
            .unwrap_err();
            assert_eq!(error.0, StatusCode::BAD_REQUEST, "{name:?}");
        }
        let response = app
            .oneshot(
                axum::http::Request::post("/api/projects")
                    .header("host", "127.0.0.1:3120")
                    .header("authorization", "Bearer owner-test")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"name":"New project"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let created: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap(),
        )
        .unwrap();
        let path = std::fs::canonicalize(state.config.work_dir.join("New project")).unwrap();
        assert_eq!(created["path"], path.to_str().unwrap());
        assert_eq!(created["name"], "New project");
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
        std::fs::write(path.join("keep.txt"), "keep").unwrap();
        let error = create_project(
            State(state.clone()),
            Json(CreateProjectBody {
                name: "New project".into(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            std::fs::read_to_string(path.join("keep.txt")).unwrap(),
            "keep"
        );
        std::fs::write(state.config.work_dir.join("existing-file"), "keep").unwrap();
        let error = create_project(
            State(state.clone()),
            Json(CreateProjectBody {
                name: "existing-file".into(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        #[cfg(unix)]
        {
            let outside = temp.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, state.config.work_dir.join("link")).unwrap();
            let error = create_project(
                State(state.clone()),
                Json(CreateProjectBody {
                    name: "link".into(),
                }),
            )
            .await
            .unwrap_err();
            assert_eq!(error.0, StatusCode::CONFLICT);
            assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
        }
        assert!(executor.executions.lock().unwrap().is_empty());
        assert_eq!(state.automation.as_ref().unwrap().queued_count().await, 0);
        server.abort();
    }

    #[tokio::test]
    async fn new_chats_accept_plain_folders_and_unborn_repositories_with_default_worktrees() {
        for initialized in [false, true] {
            let (_temp, state, _, server) = fixture().await;
            if initialized {
                let output = std::process::Command::new("git")
                    .args(["init", "--quiet"])
                    .current_dir(state.config.work_dir.join("repo"))
                    .output()
                    .unwrap();
                assert!(output.status.success());
            }
            let _ = prepare_chat(State(state.clone()), Json(Default::default()))
                .await
                .unwrap();
            let mut request = body("default-worktree-folder", false);
            request.create_worktree = Some(true);
            let _ = create_chat(State(state.clone()), Json(request))
                .await
                .unwrap();
            let identity = creation_identity(&state.config, "default-worktree-folder").unwrap();
            assert_eq!(
                state
                    .project_bindings
                    .selected_project_root(&state.config, &identity)
                    .unwrap(),
                Some(std::fs::canonicalize(state.config.work_dir.join("repo")).unwrap())
            );
            assert_eq!(state.automation.as_ref().unwrap().queued_count().await, 1);
            server.abort();
        }
    }

    #[tokio::test]
    async fn a_missing_connector_rejects_preparation_and_creation_before_workspace_changes() {
        let (_temp, mut state, executor, server) = fixture().await;
        Arc::make_mut(&mut state.config)
            .markdown_chat
            .automation
            .connector_name
            .clear();
        let error = prepare_chat(State(state.clone()), Json(Default::default()))
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::PRECONDITION_REQUIRED);
        let error = create_chat(State(state.clone()), Json(body("missing-connector", true)))
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::PRECONDITION_REQUIRED);
        let identity = creation_identity(&state.config, "missing-connector").unwrap();
        assert!(
            state
                .project_bindings
                .selected_project_root(&state.config, &identity)
                .unwrap()
                .is_none()
        );
        assert_eq!(state.automation.as_ref().unwrap().queued_count().await, 0);
        assert!(executor.executions.lock().unwrap().is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn new_chat_selects_workspace_and_model_then_hands_off_and_verifies_once() {
        for scratch in [false, true] {
            let (_temp, state, executor, server) = fixture().await;
            let prepared = prepare_chat(State(state.clone()), Json(Default::default()))
                .await
                .unwrap()
                .0;
            assert_eq!(
                prepared["models"][0]["efforts"][0]["label"],
                "Adaptive choice"
            );
            let created = create_chat(State(state.clone()), Json(body("new-request", scratch)))
                .await
                .unwrap()
                .0;
            let repeated = create_chat(State(state.clone()), Json(body("new-request", scratch)))
                .await
                .unwrap()
                .0;
            assert_eq!(created["id"], repeated["id"]);
            assert_eq!(state.automation.as_ref().unwrap().queued_count().await, 1);
            let identity = creation_identity(&state.config, "new-request").unwrap();
            let workspace = state
                .project_bindings
                .selected_project_root(&state.config, &identity)
                .unwrap()
                .unwrap();
            if !scratch {
                assert_eq!(
                    workspace,
                    std::fs::canonicalize(state.config.work_dir.join("repo")).unwrap()
                );
            }
            let located = selected(&state, created["id"].as_str().unwrap()).unwrap();
            let context = automation_context(&state, &located).await.unwrap();
            assert_eq!(
                context.owner_generation, 1,
                "owner polling must not invalidate the queued creation"
            );
            state
                .automation
                .as_ref()
                .unwrap()
                .run_next(crate::markdown_chat::now_ms())
                .await
                .unwrap()
                .unwrap();
            let execution = executor.executions.lock().unwrap()[0].clone();
            assert_eq!(execution.request.model.as_deref(), Some("discovered-model"));
            assert_eq!(execution.request.effort.as_deref(), Some("adaptive-choice"));
            assert_eq!(
                execution.request.url,
                state
                    .config
                    .markdown_chat
                    .automation
                    .chatgpt_project_url
                    .clone()
                    .unwrap()
            );
            let ActionPrompt::CreateChat {
                continuation_token,
                reference,
                ..
            } = execution.request.prompt
            else {
                panic!("wrong creation prompt")
            };
            let physical =
                ConversationIdentity::from_openai_session("actual-created-chat").unwrap();
            let claimed = state
                .continuations
                .claim(&physical, &continuation_token, |task, root| {
                    assert_eq!(task.stable_key(), identity.stable_key());
                    assert_eq!(root, workspace);
                    Ok(())
                })
                .unwrap();
            assert_eq!(claimed.task.stable_key(), identity.stable_key());
            let verified = context
                .store
                .verify_pairing_reference(
                    &claimed.task,
                    1,
                    &reference,
                    crate::markdown_chat::now_ms(),
                )
                .unwrap();
            assert_eq!(
                verified.canonical_url.as_deref(),
                Some("https://chatgpt.com/c/created-chat")
            );
            assert_eq!(verified.verified_owner_generation, Some(1));
            assert!(!verified.creating_chat);
            assert_eq!(
                state
                    .continuations
                    .owner_snapshot(&physical)
                    .unwrap()
                    .generation,
                1
            );
            assert!(
                context
                    .store
                    .verify_pairing_reference(
                        &claimed.task,
                        1,
                        &reference,
                        crate::markdown_chat::now_ms()
                    )
                    .is_err()
            );

            // Owner status reads must show the same durable wakeup phase in
            // the chat header and sidebar without launching browser work.
            use tower::ServiceExt;
            let response = router(state.clone()).oneshot(
                axum::http::Request::post(format!("/api/chats/{}/send", created["id"].as_str().unwrap()))
                    .header("host", "127.0.0.1:3120")
                    .header("authorization", "Bearer owner-test")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"request_id":"wake-queued","message":"Please continue","message_mode":"queue"}"#)).unwrap(),
            ).await.unwrap();
            let payload: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 32 * 1024)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                payload["_meta"][CHAT_WIDGET_META]["wake_status"]["phase"],
                "queued"
            );
            let sent_at = crate::markdown_chat::now_ms();
            context
                .store
                .queue_action(
                    &identity,
                    "visible-wake",
                    crate::agent_automation::AutomationActionKind::OfflineChat,
                    verified.generation,
                    None,
                    sent_at,
                )
                .unwrap();
            context
                .store
                .record_turn_send(
                    &identity,
                    crate::agent_automation::turn_renewal::TurnSend {
                        action_id: "visible-wake".into(),
                        at_ms: sent_at,
                        model: None,
                        effort: None,
                        pairing_generation: verified.generation,
                        owner_generation: 1,
                    },
                )
                .unwrap();
            context
                .store
                .update_action_status(
                    &identity,
                    "visible-wake",
                    crate::agent_automation::AutomationActionStatus::Idle,
                    None,
                    None,
                    sent_at,
                )
                .unwrap();
            let page = chat_state(
                State(state.clone()),
                Path(created["id"].as_str().unwrap().into()),
                Query(PageQuery {
                    before: None,
                    revision: None,
                }),
            )
            .await
            .unwrap()
            .0;
            assert_eq!(
                page["_meta"][CHAT_WIDGET_META]["wake_status"]["phase"],
                "waiting"
            );
            let listed = list_chats(State(state.clone()), Query(ListQuery::default()))
                .await
                .unwrap()
                .0;
            assert_eq!(listed["chats"][0]["automationWake"]["phase"], "waiting");
            context.chat.record_agent_call(sent_at + 1).await.unwrap();
            let page = chat_state(
                State(state.clone()),
                Path(created["id"].as_str().unwrap().into()),
                Query(PageQuery {
                    before: None,
                    revision: None,
                }),
            )
            .await
            .unwrap()
            .0;
            assert!(page["_meta"][CHAT_WIDGET_META]["wake_status"].is_null());
            let listed = list_chats(State(state.clone()), Query(ListQuery::default()))
                .await
                .unwrap()
                .0;
            assert!(listed["chats"][0]["automationWake"].is_null());
            assert_eq!(executor.executions.lock().unwrap().len(), 1);
            server.abort();
        }
    }

    #[tokio::test]
    async fn creation_accepts_pasted_and_explicit_worktree_bases_without_reusing_or_replaying_them()
    {
        fn git(root: &FilePath, args: &[&str]) -> String {
            let output = std::process::Command::new("git")
                .current_dir(root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        }
        for pasted in [false, true] {
            let (temp, mut state, _executor, server) = fixture().await;
            let config = Arc::make_mut(&mut state.config);
            config.worktrees.root = temp.path().join("managed");
            config.worktrees.auto_cleanup_enabled = false;
            let source = config.work_dir.join("repo");
            git(&source, &["init", "-qb", "main"]);
            git(
                &source,
                &[
                    "-c",
                    "user.name=Tests",
                    "-c",
                    "user.email=tests@example.invalid",
                    "commit",
                    "-qm",
                    "initial",
                    "--allow-empty",
                ],
            );
            let base = temp.path().join("base");
            git(
                &source,
                &["worktree", "add", "-b", "base", base.to_str().unwrap()],
            );
            std::fs::write(base.join("base.txt"), "committed base content").unwrap();
            git(&base, &["add", "base.txt"]);
            git(
                &base,
                &[
                    "-c",
                    "user.name=Tests",
                    "-c",
                    "user.email=tests@example.invalid",
                    "commit",
                    "-qm",
                    "base commit",
                ],
            );
            let head = git(&base, &["rev-parse", "HEAD"]);
            std::fs::write(base.join("base.txt"), "uncommitted base content").unwrap();
            let _ = prepare_chat(State(state.clone()), Json(Default::default()))
                .await
                .unwrap();
            let request = || {
                let mut request = body("from-base", false);
                request.create_worktree = Some(true);
                if pasted {
                    request.path = Some(base.to_string_lossy().into_owned());
                } else {
                    request.worktree_path = Some(base.to_string_lossy().into_owned());
                }
                request
            };
            let created = create_chat(State(state.clone()), Json(request()))
                .await
                .unwrap()
                .0;
            let repeated = create_chat(State(state.clone()), Json(request()))
                .await
                .unwrap()
                .0;
            assert_eq!(created["id"], repeated["id"]);
            assert_eq!(state.automation.as_ref().unwrap().queued_count().await, 1);
            let task = creation_identity(&state.config, "from-base").unwrap();
            let workspace = state
                .project_bindings
                .selected_project_root(&state.config, &task)
                .unwrap()
                .unwrap();
            assert_ne!(workspace, base);
            assert_eq!(git(&workspace, &["rev-parse", "HEAD"]), head);
            assert_eq!(
                std::fs::read_to_string(workspace.join("base.txt")).unwrap(),
                "committed base content"
            );
            assert_eq!(
                std::fs::read_to_string(base.join("base.txt")).unwrap(),
                "uncommitted base content"
            );
            // The existing reuse API still selects the base without creating another checkout.
            let before = git(&source, &["worktree", "list", "--porcelain"]);
            let mut reuse = body("reuse-base", false);
            reuse.create_worktree = None;
            reuse.worktree_path = Some(base.to_string_lossy().into_owned());
            let _ = create_chat(State(state.clone()), Json(reuse))
                .await
                .unwrap();
            let task = creation_identity(&state.config, "reuse-base").unwrap();
            assert_eq!(
                state
                    .project_bindings
                    .selected_project_root(&state.config, &task)
                    .unwrap()
                    .unwrap(),
                std::fs::canonicalize(&base).unwrap()
            );
            assert_eq!(git(&source, &["worktree", "list", "--porcelain"]), before);
            server.abort();
        }
    }

    #[tokio::test]
    async fn new_chat_rejects_unavailable_options_without_creating_a_workspace() {
        let (_temp, state, _, server) = fixture().await;
        let _ = prepare_chat(State(state.clone()), Json(Default::default()))
            .await
            .unwrap();
        let mut request = body("bad-model", false);
        request.effort = "not-discovered".into();
        assert!(
            create_chat(State(state.clone()), Json(request))
                .await
                .is_err()
        );
        let identity = creation_identity(&state.config, "bad-model").unwrap();
        assert!(
            state
                .project_bindings
                .selected_project_root(&state.config, &identity)
                .unwrap()
                .is_none()
        );
        server.abort();
    }

    #[tokio::test]
    async fn load_rate_limit_recovery_is_opt_in_pre_send_and_keeps_pairing_and_first_work_together()
    {
        for (enabled, code, accepted, goal_only, already_recovering, expect_recovery) in [
            (
                true,
                "conversation_load_rate_limited",
                false,
                true,
                false,
                true,
            ),
            (
                true,
                "conversation_load_rate_limited",
                false,
                false,
                false,
                true,
            ),
            (
                false,
                "conversation_load_rate_limited",
                false,
                true,
                false,
                false,
            ),
            (
                true,
                "conversation_load_rate_limited",
                true,
                true,
                false,
                false,
            ),
            (true, "rate_limited", false, true, false, false),
            (true, "conversation_unavailable", false, true, false, false),
            (
                true,
                "conversation_load_rate_limited",
                false,
                true,
                true,
                false,
            ),
        ] {
            let (_temp, state, executor, server, _created, context, source) =
                schema_source_with_recovery(enabled).await;
            let supervisor = state.automation.as_ref().unwrap().clone();
            if goal_only {
                let goal = context
                    .store
                    .set_goal(
                        &context.task,
                        "Keep pursuing the existing goal",
                        crate::markdown_chat::now_ms(),
                    )
                    .unwrap();
                context
                    .store
                    .run_goal(&context.task, goal.revision, crate::markdown_chat::now_ms())
                    .unwrap();
            } else {
                context
                    .chat
                    .append_user(
                        "pending-load".into(),
                        "Preserve this pending message".into(),
                    )
                    .await
                    .unwrap();
            }
            if already_recovering {
                let path = context
                    .store
                    .task_directory(&context.task)
                    .unwrap()
                    .join("automation.json");
                let mut saved: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                saved["pairing"]["loadRateLimitRecovery"] = serde_json::json!(true);
                std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
                assert!(
                    context
                        .store
                        .load(&context.task)
                        .unwrap()
                        .pairing
                        .load_rate_limit_recovery
                );
            }
            let before = context.store.load(&context.task).unwrap();
            executor
                .reserve_pairing
                .store(true, std::sync::atomic::Ordering::SeqCst);
            *executor.next_failure.lock().unwrap() = Some((code, accepted));
            let now = crate::markdown_chat::now_ms()
                + crate::agent_automation::PAIRING_VERIFICATION_GRACE_MS
                + 1000;
            supervisor.scan_once(now).await.unwrap();
            supervisor.run_next(now).await.unwrap().unwrap();
            let recovering = context.store.load(&context.task).unwrap();
            assert_eq!(
                recovering.pairing.creating_chat, expect_recovery,
                "{enabled} {code} {accepted}"
            );
            assert!(state.continuations.owner_snapshot(&source).unwrap().active);
            if !expect_recovery {
                server.abort();
                continue;
            }
            assert!(recovering.pairing.load_rate_limit_recovery);
            assert_eq!(recovering.selection, before.selection);
            assert_eq!(
                recovering
                    .goal
                    .as_ref()
                    .map(|g| (&g.markdown, g.revision, g.status)),
                before
                    .goal
                    .as_ref()
                    .map(|g| (&g.markdown, g.revision, g.status))
            );
            // An unrelated creation must not overtake pairing or its first work.
            let _ = create_chat(
                State(state.clone()),
                Json(body("competing-recovery", false)),
            )
            .await
            .unwrap();
            assert_eq!(supervisor.queued_count().await, 2);
            *executor.created_url.lock().unwrap() = "https://chatgpt.com/c/load-replacement".into();
            let count = executor.executions.lock().unwrap().len();
            let running = tokio::spawn({
                let supervisor = supervisor.clone();
                async move { supervisor.run_next(crate::markdown_chat::now_ms()).await }
            });
            tokio::time::timeout(Duration::from_secs(2), async {
                while executor.executions.lock().unwrap().len() == count
                    || context
                        .store
                        .load(&context.task)
                        .unwrap()
                        .pairing
                        .canonical_url
                        .is_none()
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            let replacement = executor.executions.lock().unwrap()[count].clone();
            assert_eq!(replacement.task_key, context.task.stable_key());
            assert!(replacement.request.retain_pairing_window);
            let competing = tokio::spawn({
                let supervisor = supervisor.clone();
                async move { supervisor.run_next(crate::markdown_chat::now_ms()).await }
            });
            tokio::time::sleep(Duration::from_millis(900)).await;
            assert!(
                !running.is_finished(),
                "rate-limit recovery must outlast the ordinary short pairing wait"
            );
            assert!(
                !competing.is_finished(),
                "unrelated work must not navigate away"
            );
            let ActionPrompt::CreateChat {
                continuation_token,
                reference,
                ..
            } = replacement.request.prompt
            else {
                panic!("replacement initialization expected")
            };
            let destination =
                ConversationIdentity::from_openai_session("load-replacement-owner").unwrap();
            state
                .continuations
                .claim(&destination, &continuation_token, |_, _| Ok(()))
                .unwrap();
            context
                .store
                .verify_pairing_reference(
                    &context.task,
                    2,
                    &reference,
                    crate::markdown_chat::now_ms(),
                )
                .unwrap();
            supervisor.notify();
            tokio::time::timeout(Duration::from_secs(3), running)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), competing)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(!state.continuations.owner_snapshot(&source).unwrap().active);
            let after = context.store.load(&context.task).unwrap();
            assert_eq!(after.selection, before.selection);
            assert_eq!(
                after
                    .goal
                    .as_ref()
                    .map(|g| (&g.markdown, g.revision, g.status)),
                before
                    .goal
                    .as_ref()
                    .map(|g| (&g.markdown, g.revision, g.status))
            );
            assert_eq!(
                after.pairing.canonical_url.as_deref(),
                Some("https://chatgpt.com/c/load-replacement")
            );
            assert!(
                !after.pairing.load_rate_limit_recovery,
                "the first accepted work prompt ends recovery"
            );
            let requests = executor.executions.lock().unwrap().clone();
            let follow_up = &requests[count + 1];
            assert_eq!(follow_up.task_key, context.task.stable_key());
            assert_eq!(
                follow_up.request.url,
                "https://chatgpt.com/c/load-replacement"
            );
            assert_eq!(
                follow_up.request.pairing_continuation_of.as_deref(),
                Some(replacement.request.action_id.as_str())
            );
            assert!(matches!(
                (&follow_up.request.prompt, goal_only),
                (ActionPrompt::OfflineGoal { .. }, true)
                    | (ActionPrompt::OfflineChat { .. }, false)
            ));
            assert_ne!(requests[count + 2].task_key, context.task.stable_key());
            server.abort();
        }
    }

    #[tokio::test]
    async fn conversation_limit_rollover_preserves_task_and_pairs_one_replacement() {
        for (accepted, stop_before_creation, no_selection, no_pending_message) in [
            (false, false, false, false),
            (true, false, false, false),
            (false, true, false, false),
            (false, false, true, false),
            (true, false, false, true),
        ] {
            let (_temp, state, executor, server) = fixture().await;
            let _ = prepare_chat(State(state.clone()), Json(serde_json::Map::new()))
                .await
                .unwrap();
            let created = create_chat(State(state.clone()), Json(body("limit-source", false)))
                .await
                .unwrap()
                .0;
            let located = selected(&state, created["id"].as_str().unwrap()).unwrap();
            let context = automation_context(&state, &located).await.unwrap();
            let supervisor = state.automation.as_ref().unwrap();
            let now = crate::markdown_chat::now_ms();
            supervisor.run_next(now).await.unwrap().unwrap();
            let first = executor.executions.lock().unwrap()[0].clone();
            let ActionPrompt::CreateChat {
                continuation_token,
                reference,
                ..
            } = first.request.prompt
            else {
                panic!("initial creation expected")
            };
            let source = ConversationIdentity::from_openai_session("exhausted-source").unwrap();
            state
                .continuations
                .claim(&source, &continuation_token, |_, _| Ok(()))
                .unwrap();
            context
                .store
                .verify_pairing_reference(&context.task, 1, &reference, now)
                .unwrap();
            let context = automation_context(&state, &located).await.unwrap();
            context
                .chat
                .append_user("read-before-limit".into(), "Already handled".into())
                .await
                .unwrap();
            context.chat.read(true).await.unwrap();
            if !no_pending_message {
                context
                    .chat
                    .append_user("pending-at-limit".into(), "Keep this request".into())
                    .await
                    .unwrap();
            }
            let goal = context
                .store
                .set_goal(&context.task, "Preserve the active task", now)
                .unwrap();
            context
                .store
                .run_goal(&context.task, goal.revision, now)
                .unwrap();
            if no_selection {
                context.store.clear_selection(&context.task).unwrap();
            }
            let before = context.store.load(&context.task).unwrap();
            *executor.next_limit.lock().unwrap() = Some(accepted);
            *executor.created_url.lock().unwrap() = "https://chatgpt.com/c/replacement-chat".into();
            let scan_at = crate::markdown_chat::now_ms()
                + crate::agent_automation::PAIRING_VERIFICATION_GRACE_MS
                + 1_000;
            supervisor.scan_once(scan_at).await.unwrap();
            supervisor.run_next(scan_at).await.unwrap().unwrap();
            let replacing = context.store.load(&context.task).unwrap();
            assert!(replacing.pairing.creating_chat);
            assert_eq!(
                replacing.pairing.pending.as_ref().unwrap().owner_generation,
                2
            );
            assert_eq!(
                replacing.action.kind,
                Some(crate::agent_automation::AutomationActionKind::PairVerify)
            );
            assert_eq!(replacing.action.automatic_retry_attempts, 0);
            assert_eq!(replacing.selection, before.selection);
            assert_eq!(
                replacing.goal.as_ref().unwrap().status,
                crate::agent_automation::GoalStatus::Active
            );
            assert_eq!(supervisor.queued_count().await, 1);
            // Late activity/polling by the old owner cannot revoke or duplicate creation.
            supervisor
                .register_task(AutomationTaskHandle::new(
                    context.task.clone(),
                    1,
                    context.store.clone(),
                    context.chat.clone(),
                ))
                .await
                .unwrap();
            supervisor.scan_once(scan_at + 1).await.unwrap();
            assert_eq!(supervisor.queued_count().await, 1);
            if stop_before_creation {
                context
                    .chat
                    .request_agent_stop("stop-rollover".into())
                    .await
                    .unwrap();
                assert!(supervisor.run_next(scan_at + 1).await.unwrap().is_none());
                assert_eq!(executor.executions.lock().unwrap().len(), 2);
                assert!(state.continuations.owner_snapshot(&source).unwrap().active);
                assert_eq!(
                    context
                        .store
                        .load(&context.task)
                        .unwrap()
                        .action
                        .send_accepted,
                    Some(false)
                );
                server.abort();
                continue;
            }
            supervisor.run_next(scan_at + 1).await.unwrap().unwrap();
            let executions = executor.executions.lock().unwrap().clone();
            assert_eq!(executions.len(), 3);
            assert_eq!(
                executions
                    .iter()
                    .filter(|e| e.request.url == "https://chatgpt.com/c/created-chat")
                    .count(),
                1
            );
            let ActionPrompt::CreateChat {
                continuation_token,
                reference,
                ..
            } = &executions[2].request.prompt
            else {
                panic!("replacement creation expected")
            };
            let destination =
                ConversationIdentity::from_openai_session("replacement-owner").unwrap();
            let workspace = state
                .project_bindings
                .selected_project_root(&state.config, &context.task)
                .unwrap()
                .unwrap();
            let claimed = state
                .continuations
                .claim(&destination, continuation_token, |task, root| {
                    assert_eq!(task.stable_key(), context.task.stable_key());
                    assert_eq!(root, workspace);
                    Ok(())
                })
                .unwrap();
            assert_eq!(claimed.task.stable_key(), context.task.stable_key());
            context
                .store
                .verify_pairing_reference(&context.task, 2, reference, scan_at + 2)
                .unwrap();
            assert!(!state.continuations.owner_snapshot(&source).unwrap().active);
            assert_eq!(
                state
                    .continuations
                    .owner_snapshot(&destination)
                    .unwrap()
                    .generation,
                2
            );
            let after = context.store.load(&context.task).unwrap();
            assert_eq!(
                after.pairing.canonical_url.as_deref(),
                Some("https://chatgpt.com/c/replacement-chat")
            );
            assert_eq!(after.selection, before.selection);
            assert_eq!(after.goal.as_ref().unwrap().markdown, goal.markdown);
            let unread = context.chat.unread_snapshot().await.unwrap();
            assert_eq!(
                unread
                    .messages
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>(),
                if no_pending_message {
                    vec![]
                } else {
                    vec!["pending-at-limit"]
                }
            );
            supervisor
                .scan_once(scan_at + crate::agent_automation::PAIRING_VERIFICATION_GRACE_MS + 1_001)
                .await
                .unwrap();
            assert_eq!(
                supervisor.queued_count().await,
                1,
                "the replacement must wake pending work after initialization"
            );
            assert_eq!(
                supervisor.next_queued_kind().await,
                Some(if no_pending_message {
                    crate::agent_automation::AutomationActionKind::OfflineGoal
                } else {
                    crate::agent_automation::AutomationActionKind::OfflineCombined
                })
            );
            server.abort();
        }
    }

    #[test]
    fn stale_limit_detection_cannot_replace_a_newer_manual_pairing() {
        let root = tempfile::tempdir().unwrap();
        let task = ConversationIdentity::from_openai_session("stale-length-limit").unwrap();
        let store = AgentAutomationStore::new(root.path().join("chats")).unwrap();
        let first = store
            .issue_pairing(&task, "https://chatgpt.com/c/source", 0, 100)
            .unwrap();
        store
            .record_pairing_canonical_url(
                &task,
                first.generation,
                "https://chatgpt.com/c/source",
                100,
            )
            .unwrap();
        store
            .verify_pairing_reference(&task, 0, &first.reference, 100)
            .unwrap();
        store
            .queue_action(
                &task,
                "limit-action",
                crate::agent_automation::AutomationActionKind::OfflineChat,
                first.generation,
                None,
                101,
            )
            .unwrap();
        store
            .update_action_status(
                &task,
                "limit-action",
                crate::agent_automation::AutomationActionStatus::ConversationLimit,
                Some("Full"),
                None,
                102,
            )
            .unwrap();
        let expected = store.load(&task).unwrap().action;
        let manual = store
            .issue_pairing(&task, "https://chatgpt.com/c/manually-selected", 0, 103)
            .unwrap();
        assert!(
            store
                .issue_rollover_pairing(&task, "https://chatgpt.com/new", 1, 104, &expected)
                .unwrap()
                .is_none()
        );
        let after = store.load(&task).unwrap();
        assert_eq!(after.pairing.generation, manual.generation);
        assert_eq!(
            after.pairing.candidate_url.as_deref(),
            Some("https://chatgpt.com/c/manually-selected")
        );
    }
}
