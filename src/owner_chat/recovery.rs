//! Recover the existing task in a fresh physical ChatGPT conversation.
use super::*;
use crate::agent_automation::conversation_recovery::{
    interrupted_replacement_creation, unavailable_conversation,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SettingsBody {
    request_id: String,
    enabled: bool,
}

pub(super) async fn settings(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<SettingsBody>,
) -> ApiResult {
    if body.enabled && !state.config.markdown_chat.features.conversation_recovery {
        return Err(internal_error(
            "Enable conversation recovery in settings first",
        ));
    }
    validate_owner_request_id(&body.request_id)?;
    let located = selected(&state, &id)?;
    let context = automation_context(&state, &located).await?;
    context
        .store
        .set_conversation_recovery_enabled(&context.task, body.enabled)
        .map_err(internal_error)?;
    Ok(Json(json!({"enabled":body.enabled})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct RecoveryBody {
    request_id: String,
    action_id: String,
}

pub(super) async fn replace(
    State(state): State<OwnerState>,
    Path(id): Path<String>,
    Json(body): Json<RecoveryBody>,
) -> ApiResult {
    validate_owner_request_id(&body.request_id)?;
    let located = selected(&state, &id)?;
    queue_replacement(&state, &located, Some(&body.action_id), false).await?;
    Ok(Json(json!({"queued":true,"id":id})))
}

fn destination(previous: &str) -> Result<String, (StatusCode, String)> {
    let parsed = reqwest::Url::parse(previous).map_err(internal_error)?;
    let parts: Vec<&str> = parsed
        .path_segments()
        .map(|p| p.collect())
        .unwrap_or_default();
    if parsed.origin().ascii_serialization() != "https://chatgpt.com" {
        return Err(internal_error("Invalid recovery conversation origin"));
    }
    Ok(if parts.len() == 4 && parts[0] == "g" && parts[2] == "c" {
        format!("https://chatgpt.com/g/{}/project", parts[1])
    } else if parts.len() == 2 && parts[0] == "c" {
        "https://chatgpt.com/new".into()
    } else {
        return Err(internal_error("Invalid recovery conversation URL"));
    })
}

type ActivityStamp = (Option<u64>, Option<String>, u64, u64);

async fn recovery_activity(
    context: &OwnerAutomationContext,
) -> Result<ActivityStamp, (StatusCode, String)> {
    let activity = context
        .chat
        .offline_activity()
        .await
        .map_err(internal_error)?;
    let now = crate::markdown_chat::now_ms();
    if activity.agent_stop_requested
        || activity
            .last_call_at_ms
            .is_some_and(|last| now.saturating_sub(last) < 60_000)
        || activity
            .agent_presence_until_ms
            .is_some_and(|until| until > now)
        || activity
            .agent_waiting_until_ms
            .is_some_and(|until| until > now)
    {
        return Err(internal_error(
            "The agent is stopped or still working; its conversation was preserved",
        ));
    }
    Ok((
        activity.last_call_at_ms,
        activity.activity_epoch,
        activity.activity_sequence,
        activity.tool_call_count,
    ))
}

async fn ensure_recovery_unchanged(
    state: &OwnerState,
    context: &OwnerAutomationContext,
    saved: &crate::agent_automation::AutomationTaskState,
    workspace: &Option<PathBuf>,
    activity: &ActivityStamp,
    owner_generation: u64,
    automatic: bool,
) -> Result<(), (StatusCode, String)> {
    let current = context.store.load(&context.task).map_err(internal_error)?;
    let goal = |s: &crate::agent_automation::AutomationTaskState| {
        s.goal.as_ref().map(|g| (g.revision, g.status))
    };
    if current.action != saved.action
        || current.pairing != saved.pairing
        || goal(&current) != goal(saved)
        || current.selection != saved.selection
        || state
            .continuations
            .owner_generation_for_task_key(context.task.stable_key())
            .map_err(internal_error)?
            != owner_generation
    {
        return Err(internal_error(
            "The failed conversation, goal, account or selection changed before recovery; nothing was replaced",
        ));
    }
    if automatic
        && (!current.conversation_recovery.enabled
            || !current.automatic_retries_enabled
            || current.action.automatic_retry_paused)
    {
        return Err(internal_error(
            "Automatic conversation recovery was disabled or paused; nothing was replaced",
        ));
    }
    if state
        .project_bindings
        .selected_project_root(&state.config, &context.task)
        .map_err(internal_error)?
        != *workspace
    {
        return Err(internal_error(
            "The task workspace changed before recovery; nothing was replaced",
        ));
    }
    if recovery_activity(context).await? != *activity {
        return Err(internal_error(
            "The agent resumed while recovery waited; its conversation was preserved",
        ));
    }
    Ok(())
}

pub(super) async fn queue_replacement(
    state: &OwnerState,
    located: &LocatedChat,
    expected: Option<&str>,
    automatic: bool,
) -> Result<(), (StatusCode, String)> {
    if !state.config.markdown_chat.features.conversation_recovery {
        return Err(internal_error(
            "Enable conversation recovery in settings before replacing a chat",
        ));
    }
    let _guard = state.creation_lock.lock().await;
    let context = automation_context(state, located).await?;
    let saved = context.store.load(&context.task).map_err(internal_error)?;
    if !saved
        .goal
        .as_ref()
        .is_some_and(|goal| goal.status == crate::agent_automation::GoalStatus::Active)
    {
        return Err(internal_error(
            "Only an active goal can replace its inaccessible conversation",
        ));
    }
    let interrupted_creation = interrupted_replacement_creation(&saved);
    let retry_creation = interrupted_creation
        || (saved.pairing.creating_chat
            && !saved.conversation_recovery.previous_urls.is_empty()
            && saved.action.send_accepted == Some(false)
            && saved.action_needs_attention());
    if !unavailable_conversation(&saved) && !retry_creation {
        return Err(internal_error(
            "This task has no confirmed failed conversation to replace",
        ));
    }
    if expected.is_some_and(|id| saved.action.id.as_deref() != Some(id)) {
        return Err(internal_error(
            "The failed conversation changed before recovery",
        ));
    }
    let activity = recovery_activity(&context).await?;
    let owner_generation = state
        .continuations
        .owner_generation_for_task_key(context.task.stable_key())
        .map_err(internal_error)?;
    let previous = if retry_creation {
        saved.conversation_recovery.previous_urls.last().cloned()
    } else {
        saved.pairing.canonical_url.clone()
    }
    .ok_or_else(|| internal_error("No saved conversation URL"))?;
    let destination = destination(&previous)?;
    let model = saved
        .selection
        .model
        .as_deref()
        .ok_or_else(|| internal_error("The saved model is unavailable"))?;
    let effort = saved
        .selection
        .effort
        .as_deref()
        .ok_or_else(|| internal_error("The saved effort is unavailable"))?;
    let account = saved
        .pairing
        .account_fingerprint
        .as_ref()
        .or(saved.selection.account_fingerprint.as_ref())
        .ok_or_else(|| internal_error("The saved ChatGPT account is unavailable"))?;
    let supervisor = state
        .automation
        .as_ref()
        .ok_or_else(|| internal_error("Browser automation is unavailable"))?;
    let workspace = state
        .project_bindings
        .selected_project_root(&state.config, &context.task)
        .map_err(internal_error)?;
    let root = workspace
        .clone()
        .or_else(|| located.work_dir.clone())
        .ok_or_else(|| internal_error("The saved task workspace is unavailable"))?;
    // Join the fair exclusive preparation queue even while another task owns
    // the browser. A passive busy health snapshot must not starve recovery.
    let lease = supervisor
        .recovery_preparation_lease(&context.task)
        .await
        .map_err(internal_error)?;
    ensure_recovery_unchanged(
        state,
        &context,
        &saved,
        &workspace,
        &activity,
        owner_generation,
        automatic,
    )
    .await?;
    let (catalog, _health) = lease
        .prepare_verified_new_chat(
            (destination != "https://chatgpt.com/new").then_some(destination.as_str()),
        )
        .await
        .map_err(internal_error)?;
    if &catalog.account_fingerprint != account {
        return Err(internal_error(
            "ChatGPT account changed while preparing recovery",
        ));
    }
    if !catalog.models.iter().any(|candidate| {
        candidate.id == model
            && candidate
                .efforts
                .iter()
                .any(|option| option.id == effort && option.available)
    }) {
        return Err(internal_error(
            "The saved model or effort is unavailable; the original conversation was preserved",
        ));
    }
    ensure_recovery_unchanged(
        state,
        &context,
        &saved,
        &workspace,
        &activity,
        owner_generation,
        automatic,
    )
    .await?;
    lease.ensure_live().map_err(internal_error)?;
    let now = crate::markdown_chat::now_ms();
    let generation = state
        .continuations
        .owner_generation_for_task_key(context.task.stable_key())
        .map_err(internal_error)?
        .checked_add(1)
        .ok_or_else(|| internal_error("Conversation owner generation overflow"))?;
    // Keep the same logical task, workspace, plan, memory, journal, queue and
    // resident commands. continue_task retires the old physical owner on claim.
    let token = state
        .continuations
        .issue_recovery_token(&context.task, &root)
        .map_err(internal_error)?;
    if !retry_creation {
        context
            .store
            .reserve_conversation_recovery(
                &context.task,
                saved
                    .action
                    .id
                    .as_deref()
                    .ok_or_else(|| internal_error("No failed action"))?,
                now,
                automatic,
            )
            .map_err(internal_error)?;
    } else if interrupted_creation {
        context
            .store
            .record_interrupted_replacement_retry(&context.task, now)
            .map_err(internal_error)?;
    } else {
        context
            .store
            .record_conversation_creation_retry(
                &context.task,
                saved
                    .action
                    .id
                    .as_deref()
                    .ok_or_else(|| internal_error("No failed creation action"))?,
                now,
            )
            .map_err(internal_error)?;
    }
    context
        .store
        .set_selection(
            &context.task,
            model,
            effort,
            &catalog.catalog_revision,
            &catalog.account_fingerprint,
            now,
        )
        .map_err(internal_error)?;
    let issue = context
        .store
        .issue_creation_pairing(&context.task, &destination, generation, now)
        .map_err(internal_error)?;
    supervisor
        .register_task(AutomationTaskHandle::new(
            context.task.clone(),
            generation,
            context.store.clone(),
            context.chat.clone(),
        ))
        .await
        .map_err(internal_error)?;
    if let Err(error) = supervisor
        .queue_new_chat(
            &context.task,
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
        let _ = context
            .store
            .invalidate_pairing(&context.task, issue.generation, now);
        return Err(internal_error(error));
    }
    // queue_new_chat may wake a dispatcher while this task is still leased.
    // Publish another wake only after both preparation guards are released.
    drop(lease);
    supervisor.notify();
    Ok(())
}

pub(super) async fn maintain(state: &OwnerState, chats: &[LocatedChat]) {
    if !state.config.markdown_chat.features.conversation_recovery {
        return;
    }
    for located in chats {
        let Ok(task) = automation_task(located) else {
            continue;
        };
        let Ok(store) = AgentAutomationStore::for_state_root(&state.config, &located.state_root)
        else {
            continue;
        };
        let Ok(saved) = store.load(&task) else {
            continue;
        };
        if !saved.conversation_recovery.enabled
            || !saved.automatic_retries_enabled
            || saved.action.automatic_retry_paused
        {
            continue;
        }
        let now = crate::markdown_chat::now_ms();
        let recovery = match store.observe_unavailable_conversation(&task, now) {
            Ok(recovery) => recovery,
            Err(_) => continue,
        };
        let retry_worker = saved.pairing.creating_chat
            && saved.agents.parent_task_key.is_some()
            && saved.conversation_recovery.previous_urls.is_empty()
            && saved.action.send_accepted == Some(false)
            && saved.action_needs_attention_at(now)
            && saved
                .goal
                .as_ref()
                .is_some_and(|goal| goal.status == crate::agent_automation::GoalStatus::Active)
            && saved
                .conversation_recovery
                .last_recovery_at_ms
                .or(saved.action.updated_at_ms)
                .is_some_and(|last| now.saturating_sub(last) >= 300_000);
        if retry_worker {
            if let Some(supervisor) = state.automation.as_ref() {
                let runtime = crate::agent_automation::agents::AgentRuntime::new(
                    state.config.clone(),
                    state.chats.clone(),
                    state.project_bindings.clone(),
                    state.continuations.clone(),
                    supervisor.clone(),
                );
                if let Err(error) = runtime.retry_initial_creation(&task).await {
                    tracing::debug!(task=task.stable_key(),%error,"worker creation retry deferred");
                }
            }
            continue;
        }
        let retry_creation = (interrupted_replacement_creation(&saved)
            || (saved.pairing.creating_chat
                && !saved.conversation_recovery.previous_urls.is_empty()
                && saved.action.send_accepted == Some(false)
                && saved.action_needs_attention_at(now)))
            && saved
                .conversation_recovery
                .last_recovery_at_ms
                .is_some_and(|last| now.saturating_sub(last) >= 300_000);
        if retry_creation {
            let _ = queue_replacement(state, located, saved.action.id.as_deref(), true).await;
            continue;
        }
        if !unavailable_conversation(&saved) {
            continue;
        }
        if !recovery.automatic_replacement_due(now) {
            continue;
        }
        // Readiness and activity failures are normal deferrals. The original
        // task remains visible until its verified replacement takes ownership.
        if let Err(error) =
            queue_replacement(state, located, saved.action.id.as_deref(), true).await
        {
            tracing::debug!(
                task = task.stable_key(),
                ?error,
                "conversation recovery deferred"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_automation::{
        AutomationActionKind, AutomationActionStatus, AutomationExecution,
        AutomationExecutionError, AutomationExecutor, CapabilityCatalog, CompanionActionSnapshot,
        CompanionEvent,
    };
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Notify;

    struct Executor {
        catalog: CapabilityCatalog,
        preparations: AtomicUsize,
        hold: AtomicBool,
        entered: Notify,
        release: Notify,
        health_reads: AtomicUsize,
        wrong_account: AtomicBool,
        missing_model: AtomicBool,
    }
    #[async_trait]
    impl AutomationExecutor for Executor {
        async fn prepare_new_chat(
            &self,
            _: Option<&str>,
            cancellation: CancellationToken,
        ) -> Result<CapabilityCatalog, String> {
            self.preparations.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            if self.hold.load(Ordering::SeqCst) {
                tokio::select! {
                    _ = cancellation.cancelled() => return Err("fixture preparation cancelled".into()),
                    _ = self.release.notified() => {}
                }
            }
            let mut catalog = self.catalog.clone();
            if self.missing_model.load(Ordering::SeqCst) {
                catalog.models.clear();
            }
            Ok(catalog)
        }
        async fn execute(
            &self,
            _: AutomationExecution,
            _: tokio::sync::mpsc::UnboundedSender<CompanionEvent>,
            _: CancellationToken,
        ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
            panic!("recovery fixture must queue creation without sending it")
        }
    }

    struct Fixture {
        _root: tempfile::TempDir,
        state: OwnerState,
        context: Arc<OwnerAutomationContext>,
        executor: Arc<Executor>,
        id: String,
        server: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }
    impl Fixture {
        fn located(&self) -> LocatedChat {
            selected(&self.state, &self.id).unwrap()
        }
        fn queue(&self) -> tokio::task::JoinHandle<Result<(), (StatusCode, String)>> {
            let state = self.state.clone();
            let id = self.id.clone();
            let expected = self
                .context
                .store
                .load(&self.context.task)
                .unwrap()
                .action
                .id;
            tokio::spawn(async move {
                let located = selected(&state, &id)?;
                queue_replacement(&state, &located, expected.as_deref(), true).await
            })
        }
    }

    async fn fixture() -> Fixture {
        crate::tls::ensure_crypto_provider();
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let mut config = crate::config::default_config(project.clone());
        config.worktrees.mode = crate::types::WorktreeMode::Never;
        config.markdown_chat.enabled = true;
        config.markdown_chat.automation.enabled = true;
        config.markdown_chat.features.conversation_recovery = true;
        config
            .markdown_chat
            .automation
            .retry_timings
            .infinite_retries = true;
        config.markdown_chat.automation.connector_name = "Codexify".into();
        config.loaded_config_path = Some(root.path().join("settings.json"));
        std::fs::write(
            config.loaded_config_path.as_ref().unwrap(),
            json!({"agentChat":{"automation":{"automaticRetries":true,"connectorName":"Codexify"}}}).to_string(),
        )
        .unwrap();
        config.memory.dir = Some(root.path().join("metadata").display().to_string());
        // The real companion client requires a private descriptor directory.
        let descriptor_directory = root.path().join("browser");
        std::fs::create_dir(&descriptor_directory).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                &descriptor_directory,
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        config.markdown_chat.automation.descriptor_path = descriptor_directory
            .join("descriptor.json")
            .display()
            .to_string();
        let build = format!(
            "recovery-test{}",
            crate::agent_automation::PROTOCOL_BUILD_SUFFIX
        );
        let catalog:CapabilityCatalog=serde_json::from_value(json!({"protocolVersion":1,"build":build,"accountFingerprint":"recovery-account","catalogRevision":"recovery-catalog","models":[{"id":"recovery-model","label":"Recovery model","efforts":[{"id":"high","label":"High","available":true,"reason":null}]}]})).unwrap();
        let executor = Arc::new(Executor {
            catalog,
            preparations: AtomicUsize::new(0),
            hold: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
            health_reads: AtomicUsize::new(0),
            wrong_account: AtomicBool::new(false),
            missing_model: AtomicBool::new(false),
        });
        let health_executor = executor.clone();
        let health_build = build.clone();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let app=Router::new().route("/v1/health",get(move || {
            let executor=health_executor.clone();let build=health_build.clone();async move {
                let ready=executor.preparations.load(Ordering::SeqCst)>0 && executor.health_reads.fetch_add(1,Ordering::SeqCst)>0;
                let account=if executor.wrong_account.load(Ordering::SeqCst) {"changed-browser-account"}else{"recovery-account"};
                Json(json!({"protocolVersion":1,"kind":"codexify-browser-companion","build":build,"status":if ready {"idle"}else{"busy"},"authenticated":ready,"accountFingerprint":if ready {Some(account)}else{None},"activeActionId":null}))
            }
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        std::fs::write(&config.markdown_chat.automation.descriptor_path,serde_json::to_vec(&json!({"version":1,"kind":"codexify-browser-companion","pid":std::process::id(),"endpoint":format!("http://127.0.0.1:{port}"),"token":"A".repeat(43),"build":build,"createdAt":"2026-10-30T00:00:00Z"})).unwrap()).unwrap();
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
                root.path().join("bindings"),
            )),
            creation_lock: Arc::default(),
            artifacts: Arc::new(ArtifactEgressStore::new_at(
                config.artifact_egress.clone(),
                root.path().join("artifacts"),
            )),
            continuations: Arc::new(
                ConversationContinuationStore::new(root.path().join("continuations.json")).unwrap(),
            ),
            automation: Some(supervisor),
            open_browser: Arc::new(|_| Ok(())),
            catalog_cache: Arc::default(),
            global: Arc::default(),
            token: "recovery-fixture".into(),
        };
        let task = ConversationIdentity::from_openai_session("recover-lease-task").unwrap();
        state
            .project_bindings
            .select_project_root(&config, &task, &project.to_string_lossy())
            .await
            .unwrap();
        let effective = state
            .project_bindings
            .effective_config(&config, &task)
            .unwrap();
        let chat = state
            .chats
            .chat(
                &effective,
                Some(&task),
                &crate::exec_sessions::SessionState::new(),
            )
            .unwrap();
        chat.ensure().await.unwrap();
        let store = AgentAutomationStore::for_config(&effective).unwrap();
        store
            .set_selection(
                &task,
                "recovery-model",
                "high",
                "original-catalog",
                "recovery-account",
                1,
            )
            .unwrap();
        let pair = store
            .issue_pairing(&task, "https://chatgpt.com/c/broken-recovery", 0, 2)
            .unwrap();
        store
            .record_pairing_canonical_url(
                &task,
                pair.generation,
                "https://chatgpt.com/c/broken-recovery",
                3,
            )
            .unwrap();
        store
            .verify_pairing(&task, pair.generation, 0, &pair.reference, 4)
            .unwrap();
        let goal = store
            .set_goal(&task, "Preserve the exact original work", 5)
            .unwrap();
        store.run_goal(&task, goal.revision, 6).unwrap();
        store
            .set_conversation_recovery_enabled(&task, true)
            .unwrap();
        let now = crate::markdown_chat::now_ms();
        for (index, at) in [now - 120_000, now - 90_000, now - 60_000]
            .into_iter()
            .enumerate()
        {
            let id = format!("failed-{index}");
            store
                .queue_action(
                    &task,
                    &id,
                    AutomationActionKind::OfflineGoal,
                    pair.generation,
                    Some(goal.revision),
                    at,
                )
                .unwrap();
            store.update_action_status(&task,&id,AutomationActionStatus::ConversationUnavailable,Some(crate::agent_automation::conversation_recovery::UNAVAILABLE_CONVERSATION_MESSAGE),None,at).unwrap();
            store.observe_unavailable_conversation(&task, at).unwrap();
        }
        let located = locate(&state.config, &state.project_bindings)
            .into_iter()
            .find(|c| c.owner == task.stable_key())
            .unwrap();
        let id = located.id.clone();
        let context = Arc::new(automation_context(&state, &located).await.unwrap());
        Fixture {
            _root: root,
            state,
            context,
            executor,
            id,
            server,
        }
    }

    #[tokio::test]
    async fn recovery_lease_prepares_before_passive_health_and_waits_for_real_account_proof() {
        let f = fixture().await;
        let original = f.context.store.load(&f.context.task).unwrap();
        queue_replacement(&f.state, &f.located(), original.action.id.as_deref(), true)
            .await
            .unwrap();
        let saved = f.context.store.load(&f.context.task).unwrap();
        assert_eq!(f.executor.preparations.load(Ordering::SeqCst), 1);
        assert!(
            f.executor.health_reads.load(Ordering::SeqCst) >= 2,
            "warming is not sign-out"
        );
        assert_eq!(saved.conversation_recovery.failed_chat_count, 1);
        assert!(saved.pairing.creating_chat);
        assert_eq!(saved.goal, original.goal);
        assert_eq!(saved.selection.model, original.selection.model);
        assert_eq!(saved.selection.effort, original.selection.effort);
        assert_eq!(f.state.automation.as_ref().unwrap().queued_count().await, 1);
    }

    #[tokio::test]
    async fn recovery_lease_skips_its_task_while_unrelated_scans_continue() {
        let f = fixture().await;
        let other = ConversationIdentity::from_openai_session("other-recovery-task").unwrap();
        f.state
            .project_bindings
            .select_project_root(
                &f.state.config,
                &other,
                &f.state.config.work_dir.to_string_lossy(),
            )
            .await
            .unwrap();
        let effective = f
            .state
            .project_bindings
            .effective_config(&f.state.config, &other)
            .unwrap();
        let chat = f
            .state
            .chats
            .chat(
                &effective,
                Some(&other),
                &crate::exec_sessions::SessionState::new(),
            )
            .unwrap();
        chat.ensure().await.unwrap();
        let store = Arc::new(AgentAutomationStore::for_config(&effective).unwrap());
        store
            .set_selection(
                &other,
                "recovery-model",
                "high",
                "catalog",
                "recovery-account",
                1,
            )
            .unwrap();
        let pair = store
            .issue_pairing(&other, "https://chatgpt.com/c/unrelated-recovery", 0, 2)
            .unwrap();
        store
            .record_pairing_canonical_url(
                &other,
                pair.generation,
                "https://chatgpt.com/c/unrelated-recovery",
                3,
            )
            .unwrap();
        store
            .verify_pairing(&other, pair.generation, 0, &pair.reference, 4)
            .unwrap();
        store
            .set_goal(&other, "Unrelated work must keep scheduling", 5)
            .unwrap();
        store.run_goal(&other, 1, 6).unwrap();
        let supervisor = f.state.automation.as_ref().unwrap();
        supervisor
            .register_task(AutomationTaskHandle::new(other.clone(), 0, store, chat))
            .await
            .unwrap();
        let failed = f.context.store.load(&f.context.task).unwrap().action;
        f.executor.hold.store(true, Ordering::SeqCst);
        let recovery = f.queue();
        tokio::time::timeout(Duration::from_secs(2), f.executor.entered.notified())
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(2),
            supervisor.scan_once(crate::markdown_chat::now_ms() + 600_000),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            !recovery.is_finished(),
            "preparation remains deliberately blocked"
        );
        assert_eq!(
            f.context.store.load(&f.context.task).unwrap().action,
            failed,
            "scan skips the leased failed task"
        );
        assert!(
            supervisor.queue_position(&other).await.is_some(),
            "unrelated work still enters the queue"
        );
        f.executor.release.notify_one();
        recovery.await.unwrap().unwrap();
        let saved = f.context.store.load(&f.context.task).unwrap();
        assert_eq!(saved.conversation_recovery.failed_chat_count, 1);
        assert!(saved.pairing.creating_chat);
        assert_eq!(f.state.automation.as_ref().unwrap().queued_count().await, 2);
    }

    #[tokio::test]
    async fn recovery_lease_real_owner_pause_and_cancel_apply_before_preparation_finishes() {
        for operation in ["pause-retries", "cancel"] {
            let f = fixture().await;
            f.executor.hold.store(true, Ordering::SeqCst);
            let action = f
                .context
                .store
                .load(&f.context.task)
                .unwrap()
                .action
                .id
                .unwrap();
            let recovery = f.queue();
            tokio::time::timeout(Duration::from_secs(2), f.executor.entered.notified())
                .await
                .unwrap();
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let state = f.state.clone();
            let server = tokio::spawn(async move {
                axum::serve(listener, router(state)).await.unwrap();
            });
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap();
            let response = client
                .post(format!(
                    "http://{address}/api/chats/{}/automation/action/{operation}",
                    f.id
                ))
                .bearer_auth("recovery-fixture")
                .json(&json!({"requestId":format!("owner-{operation}"),"actionId":action}))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let body = response.text().await.unwrap();
            server.abort();
            assert_eq!(status, StatusCode::OK, "{operation}: {body}");
            assert!(
                !recovery.is_finished(),
                "owner control must not wait for browser preparation"
            );
            let paused = f.context.store.load(&f.context.task).unwrap();
            assert!(paused.action.automatic_retry_paused, "{operation}");
            if operation == "cancel" {
                assert_eq!(paused.action.status, AutomationActionStatus::Cancelled);
            }
            f.executor.release.notify_one();
            assert!(recovery.await.unwrap().is_err(), "{operation}");
            let saved = f.context.store.load(&f.context.task).unwrap();
            assert_eq!(
                saved.conversation_recovery.failed_chat_count, 0,
                "{operation}"
            );
            assert!(
                saved.conversation_recovery.last_recovery_at_ms.is_none(),
                "{operation}"
            );
            assert!(!saved.pairing.creating_chat, "{operation}");
            assert_eq!(
                f.state.automation.as_ref().unwrap().queued_count().await,
                0,
                "{operation}"
            );
        }
    }

    #[tokio::test]
    async fn recovery_lease_rechecks_changes_after_preparation_without_reserving_or_overwriting() {
        for change in [
            "action",
            "accepted",
            "uncertain",
            "stop",
            "goal",
            "selection",
            "workspace",
            "recovery-optout",
            "retry-pause",
            "retry-optout",
            "activity",
            "browser-account",
            "model-unavailable",
            "owner",
        ] {
            let f = fixture().await;
            f.executor.hold.store(true, Ordering::SeqCst);
            let before = f.context.store.load(&f.context.task).unwrap();
            let recovery = f.queue();
            tokio::time::timeout(Duration::from_secs(2), f.executor.entered.notified())
                .await
                .unwrap();
            match change {
                "stop" => f
                    .context
                    .chat
                    .request_agent_stop("owner-stop".into())
                    .await
                    .unwrap(),
                "goal" => {
                    f.context
                        .store
                        .stop_goal(
                            &f.context.task,
                            before.goal.as_ref().unwrap().revision,
                            crate::markdown_chat::now_ms(),
                        )
                        .unwrap();
                }
                "workspace" => {
                    let current = f
                        .state
                        .project_bindings
                        .selected_project_root(&f.state.config, &f.context.task)
                        .unwrap()
                        .unwrap();
                    f.state
                        .project_bindings
                        .switch_to_picker(&f.state.config, &f.context.task, &current)
                        .await
                        .unwrap();
                }
                "recovery-optout" => {
                    f.context
                        .store
                        .set_conversation_recovery_enabled(&f.context.task, false)
                        .unwrap();
                }
                "activity" => {
                    f.context.chat.record_agent_call(1).await.unwrap();
                }
                "browser-account" => {
                    f.executor.wrong_account.store(true, Ordering::SeqCst);
                }
                "model-unavailable" => {
                    f.executor.missing_model.store(true, Ordering::SeqCst);
                }
                "retry-optout" => {
                    std::fs::write(
                        f.state.config.loaded_config_path.as_ref().unwrap(),
                        json!({"agentChat":{"automation":{"automaticRetries":false}}}).to_string(),
                    )
                    .unwrap();
                }
                "owner" => {
                    let root = f
                        .state
                        .project_bindings
                        .selected_project_root(&f.state.config, &f.context.task)
                        .unwrap()
                        .unwrap();
                    let token = f
                        .state
                        .continuations
                        .issue_recovery_token(&f.context.task, &root)
                        .unwrap();
                    let owner = ConversationIdentity::from_openai_session("changed-recovery-owner")
                        .unwrap();
                    f.state
                        .continuations
                        .claim(&owner, &token, |_, _| Ok(()))
                        .unwrap();
                }
                _ => {
                    f.context
                        .store
                        .mutate_test_state(&f.context.task, |saved| {
                            match change {
                                "action" => saved.action.id = Some("different-action".into()),
                                "accepted" => saved.action.send_accepted = Some(true),
                                "uncertain" => saved.action.send_accepted = None,
                                "selection" => {
                                    saved.selection.effort = Some("changed-effort".into())
                                }
                                "retry-pause" => saved.action.automatic_retry_paused = true,
                                _ => unreachable!(),
                            }
                            Ok(())
                        })
                        .unwrap();
                }
            }
            f.executor.release.notify_one();
            assert!(recovery.await.unwrap().is_err(), "{change}");
            let saved = f.context.store.load(&f.context.task).unwrap();
            assert_eq!(saved.conversation_recovery.failed_chat_count, 0, "{change}");
            assert!(
                saved.conversation_recovery.last_recovery_at_ms.is_none(),
                "{change}"
            );
            assert_eq!(
                saved.pairing.canonical_url, before.pairing.canonical_url,
                "{change}"
            );
            assert!(!saved.pairing.creating_chat, "{change}");
            assert_eq!(
                f.state.automation.as_ref().unwrap().queued_count().await,
                0,
                "{change}"
            );
        }
    }
}
