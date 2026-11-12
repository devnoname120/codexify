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
    let (catalog, health) = supervisor
        .prepare_verified_new_chat(
            state
                .config
                .markdown_chat
                .automation
                .chatgpt_project_url
                .as_deref(),
        )
        .await
        .map_err(internal_error)?;
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
    let previous_root = bindings
        .selected_project_root(&state.config, &task)
        .map_err(internal_error)?;
    if previous_root.is_some() {
        let previous_config = bindings
            .effective_config(&state.config, &task)
            .map_err(internal_error)?;
        AgentAutomationStore::for_config(&previous_config)
            .map_err(internal_error)?
            .record_creation_request(&task, &digest)
            .map_err(internal_error)?;
    }
    let config = selection_config(&state.config, body.create_worktree);
    crate::project_progress::phase("Preparing workspace");
    // The stable request already owns its selected checkout. Selecting it again
    // with createWorktree can allocate another checkout and duplicate the chat.
    let _selected_root = if let Some(previous_root) = previous_root {
        previous_root
    } else if let Some(worktree) = &body.worktree_path {
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
    crate::project_progress::phase("Creating local chat");
    let effective = bindings
        .effective_config(&state.config, &task)
        .map_err(internal_error)?;
    let root = effective.work_dir.clone();
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
    if owner != 0 {
        return Err(internal_error(
            "This chat was already created; replace its pairing instead of creating it again",
        ));
    }
    let token = state
        .continuations
        .issue_token(task, task, root)
        .map_err(internal_error)?;
    let now = crate::markdown_chat::now_ms();
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
        .issue_creation_pairing(task, destination, 1, now)
        .map_err(internal_error)?;
    let supervisor = state
        .automation
        .as_ref()
        .ok_or_else(|| internal_error("Browser automation supervisor is unavailable"))?;
    supervisor
        .register_task(AutomationTaskHandle::new(
            task.clone(),
            1,
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
    if !saved.conversation_recovery.previous_urls.is_empty() {
        // Release the creation lock before entering the shared replacement path.
        drop(_guard);
        recovery::queue_replacement(&state, &located, saved.action.id.as_deref(), false).await?;
        return Ok(Json(json!({"queued":true})));
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    struct CreationExecutor {
        catalog: CapabilityCatalog,
        executions: Mutex<Vec<AutomationExecution>>,
        preparations: AtomicUsize,
        block_health: AtomicBool,
        health_entered: tokio::sync::Notify,
        health_release: tokio::sync::Notify,
        health_account: Mutex<String>,
    }
    #[async_trait]
    impl AutomationExecutor for CreationExecutor {
        async fn prepare_new_chat(
            &self,
            _: Option<&str>,
            _: CancellationToken,
        ) -> Result<CapabilityCatalog, String> {
            self.preparations.fetch_add(1, Ordering::SeqCst);
            Ok(self.catalog.clone())
        }
        async fn execute(
            &self,
            execution: AutomationExecution,
            _: mpsc::UnboundedSender<CompanionEvent>,
            _: CancellationToken,
        ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
            self.executions.lock().unwrap().push(execution.clone());
            Ok(CompanionActionSnapshot {
                protocol_version: 1,
                build: self.catalog.build.clone(),
                action_id: execution.request.action_id,
                state: CompanionActionState::Idle,
                send_accepted: true,
                canonical_url: Some("https://chatgpt.com/c/created-chat".into()),
                selected: Some(CompanionSelection {
                    model: execution.request.model.unwrap(),
                    effort: execution.request.effort.unwrap(),
                }),
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

        config.markdown_chat.enabled = true;
        config.markdown_chat.automation.enabled = true;
        config.markdown_chat.features.conversation_recovery = true;
        config.markdown_chat.automation.connector_name = "Codexify".into();
        config.memory.dir = Some(root.join("metadata").display().to_string());
        config.markdown_chat.automation.descriptor_path =
            root.join("descriptor.json").display().to_string();
        config.markdown_chat.automation.chatgpt_project_url =
            Some("https://chatgpt.com/g/g-p-0123456789abcdef0123456789abcdef/project".into());
        let build = format!("fixture{}", crate::agent_automation::PROTOCOL_BUILD_SUFFIX);
        let catalog:CapabilityCatalog=serde_json::from_value(json!({"protocolVersion":1,"build":build,"accountFingerprint":"creation-account","catalogRevision":"creation-catalog","models":[{"id":"discovered-model","label":"Discovered model","efforts":[{"id":"adaptive-choice","label":"Adaptive choice","available":true,"reason":null}]}]})).unwrap();
        let executor = Arc::new(CreationExecutor {
            catalog,
            executions: Mutex::new(Vec::new()),
            preparations: AtomicUsize::new(0),
            block_health: AtomicBool::new(false),
            health_entered: Default::default(),
            health_release: Default::default(),
            health_account: Mutex::new("creation-account".into()),
        });
        let health_executor = executor.clone();
        let health_build = build.clone();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route(
            "/v1/health",
            get(move || {
                let executor = health_executor.clone();
                let build = health_build.clone();
                async move {
                    if executor.block_health.load(Ordering::SeqCst) {
                        executor.health_entered.notify_one();
                        executor.health_release.notified().await;
                    }
                    Json(json!({"protocolVersion":1,"kind":"codexify-browser-companion","build":build,"status":"idle","authenticated":true,"accountFingerprint":executor.health_account.lock().unwrap().clone(),"activeActionId":null}))
                }
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
        let supervisor = AgentAutomationSupervisor::new(
            config.markdown_chat.automation.clone(),
            executor.clone(),
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

    #[tokio::test]
    async fn new_chat_preparation_keeps_its_browser_lease_through_account_validation() {
        let (_temp, state, executor, server) = fixture().await;
        executor.block_health.store(true, Ordering::SeqCst);
        let preparing_state = state.clone();
        let preparing = tokio::spawn(async move {
            prepare_chat(State(preparing_state), Json(Default::default())).await
        });
        tokio::time::timeout(Duration::from_secs(2), executor.health_entered.notified())
            .await.unwrap();
        let supervisor = state.automation.as_ref().unwrap().clone();
        let mut contender = tokio::spawn(async move { supervisor.prepare_new_chat(None).await });
        assert!(tokio::time::timeout(Duration::from_millis(50), &mut contender).await.is_err(),
            "another browser action must wait for preparation's health proof");
        assert_eq!(executor.preparations.load(Ordering::SeqCst), 1);
        executor.block_health.store(false, Ordering::SeqCst);
        executor.health_release.notify_one();
        let prepared = preparing.await.unwrap().unwrap().0;
        assert_eq!(prepared["catalogRevision"], "creation-catalog");
        contender.await.unwrap().unwrap();
        assert_eq!(executor.preparations.load(Ordering::SeqCst), 2);
        assert!(state.catalog_cache.lock().await.entry.is_some());
        server.abort();
    }

    #[tokio::test]
    async fn new_chat_preparation_rejects_a_genuinely_different_account() {
        let (_temp, state, executor, server) = fixture().await;
        *executor.health_account.lock().unwrap() = "different-account".into();
        let error = prepare_chat(State(state.clone()), Json(Default::default())).await.unwrap_err();
        assert!(error.1.contains("ChatGPT account changed"));
        assert!(state.catalog_cache.lock().await.entry.is_none());
        assert!(executor.executions.lock().unwrap().is_empty());
        server.abort();
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
    async fn broken_chat_replacement_keeps_the_task_project_goal_and_journal_and_retires_its_old_owner()
     {
        use crate::agent_automation::{AutomationActionKind, AutomationActionStatus};
        let (_temp, state, executor, server) = fixture().await;
        let _ = prepare_chat(State(state.clone()), Json(Default::default()))
            .await
            .unwrap();
        let created = create_chat(State(state.clone()), Json(body("recover-existing", false)))
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
        let ActionPrompt::CreateChat {
            continuation_token,
            reference,
            ..
        } = executor.executions.lock().unwrap()[0]
            .request
            .prompt
            .clone()
        else {
            panic!("missing creation")
        };
        let old = ConversationIdentity::from_openai_session("broken-physical-owner").unwrap();
        state
            .continuations
            .claim(&old, &continuation_token, |_, _| Ok(()))
            .unwrap();
        context
            .store
            .verify_pairing_reference(&context.task, 1, &reference, crate::markdown_chat::now_ms())
            .unwrap();
        let goal = context
            .store
            .set_goal(&context.task, "Finish the original work", 10)
            .unwrap();
        context
            .store
            .run_goal(&context.task, goal.revision, 11)
            .unwrap();
        let journal = context
            .store
            .task_directory(&context.task)
            .unwrap()
            .join("rollout.md");
        std::fs::write(&journal, "Saved progress and next steps").unwrap();
        let pairing = context.store.load(&context.task).unwrap().pairing;
        // Real replacements retain the previous physical owner's turn clock.
        // It must not revoke the next owner's pending creation on registration.
        context.store.queue_action(&context.task, "old-goal-turn", AutomationActionKind::OfflineGoal,
            pairing.generation, Some(goal.revision), 12).unwrap();
        context.store.record_turn_send(&context.task, crate::agent_automation::turn_renewal::TurnSend {
            action_id:"old-goal-turn".into(),at_ms:12,model:Some("discovered-model".into()),
            effort:Some("adaptive-choice".into()),pairing_generation:pairing.generation,owner_generation:1,
        }).unwrap();
        context.store.update_action_status(&context.task,"old-goal-turn",AutomationActionStatus::Idle,None,None,12).unwrap();
        assert!(context.store.load(&context.task).unwrap().turn_renewal.send.is_some());
        context
            .store
            .queue_action(
                &context.task,
                "confirmed-load-failure",
                AutomationActionKind::OfflineGoal,
                pairing.generation,
                Some(goal.revision),
                12,
            )
            .unwrap();
        context.store.update_action_status(&context.task,"confirmed-load-failure",AutomationActionStatus::ConversationUnavailable,Some(crate::agent_automation::conversation_recovery::UNAVAILABLE_CONVERSATION_MESSAGE),None,13).unwrap();
        let root = state
            .project_bindings
            .selected_project_root(&state.config, &context.task)
            .unwrap();
        super::super::recovery::queue_replacement(&state, &located, Some("stale-action"), false)
            .await
            .unwrap_err();
        super::super::recovery::queue_replacement(
            &state,
            &located,
            Some("confirmed-load-failure"),
            false,
        )
        .await
        .unwrap();
        assert_eq!(state.automation.as_ref().unwrap().queued_count().await, 1);
        // A status/activity request can carry the previous owner's snapshot
        // across the creation transaction. It must not cancel the new pairing.
        state
            .automation
            .as_ref()
            .unwrap()
            .register_task(AutomationTaskHandle::new(
                context.task.clone(),
                context.owner_generation,
                context.store.clone(),
                context.chat.clone(),
            ))
            .await
            .unwrap();
        let observed = automation_context(&state, &located).await.unwrap();
        assert_eq!(observed.owner_generation, 2);
        assert_eq!(state.automation.as_ref().unwrap().queued_count().await, 1);
        // A pre-dispatch crash/older cancellation leaves no browser action to
        // replay. Recover the reservation without counting another failed chat.
        state
            .automation
            .as_ref()
            .unwrap()
            .discard_queued_actions(&context.task)
            .await;
        context
            .store
            .invalidate_pairing_and_supersede_action(&context.task, crate::markdown_chat::now_ms())
            .unwrap();
        assert!(
            crate::agent_automation::conversation_recovery::interrupted_replacement_creation(
                &context.store.load(&context.task).unwrap()
            )
        );
        super::super::recovery::queue_replacement(&state, &located, None, false)
            .await
            .unwrap();
        assert_eq!(state.automation.as_ref().unwrap().queued_count().await, 1);
        assert!(
            super::super::recovery::queue_replacement(
                &state,
                &located,
                Some("confirmed-load-failure"),
                false
            )
            .await
            .is_err()
        );
        state
            .automation
            .as_ref()
            .unwrap()
            .run_next(crate::markdown_chat::now_ms())
            .await
            .unwrap()
            .unwrap();
        let ActionPrompt::CreateChat {
            continuation_token,
            reference,
            ..
        } = executor.executions.lock().unwrap()[1]
            .request
            .prompt
            .clone()
        else {
            panic!("missing replacement")
        };
        let replacement =
            ConversationIdentity::from_openai_session("replacement-physical-owner").unwrap();
        let claim = state
            .continuations
            .claim(&replacement, &continuation_token, |task, path| {
                assert_eq!(task.stable_key(), context.task.stable_key());
                assert_eq!(Some(path.to_path_buf()), root);
                Ok(())
            })
            .unwrap();
        context
            .store
            .verify_pairing_reference(&claim.task, 2, &reference, crate::markdown_chat::now_ms())
            .unwrap();
        let saved = context.store.load(&context.task).unwrap();
        assert_eq!(saved.goal.unwrap().markdown, "Finish the original work");
        assert_eq!(saved.conversation_recovery.failed_chat_count, 1);
        assert_eq!(saved.selection.model.as_deref(), Some("discovered-model"));
        assert_eq!(
            std::fs::read_to_string(journal).unwrap(),
            "Saved progress and next steps"
        );
        assert!(!state.continuations.owner_snapshot(&old).unwrap().active);
        assert!(
            state
                .continuations
                .owner_snapshot(&replacement)
                .unwrap()
                .active
        );
        server.abort();
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
}
