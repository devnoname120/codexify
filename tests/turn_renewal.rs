use async_trait::async_trait;
use codexify::agent_automation::turn_renewal::{TurnRenewalConfig, TurnSend};
use codexify::{
    agent_automation::*, config::default_config, exec_sessions::SessionState,
    markdown_chat::MarkdownChatStore, project_bindings::ConversationIdentity,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Executor {
    complete: AtomicBool,
    accepted_at_ms: AtomicU64,
    sent: Mutex<Vec<AutomationExecution>>,
    fail: AtomicBool,
    ambiguous: AtomicBool,
    probe_fail: AtomicBool,
    snapshot_only: AtomicBool,
    verified_only: AtomicBool,
    actual_selection: Mutex<Option<CompanionSelection>>,
    before_send: Mutex<Option<Box<dyn Fn() + Send>>>,
}
#[async_trait]
impl AutomationExecutor for Executor {
    async fn turn_handoff_complete(&self, _: &str, _: &str, _: &str) -> Result<bool, String> {
        if self.probe_fail.load(Ordering::SeqCst) { return Err("Observation failed".into()); }
        Ok(self.complete.load(Ordering::SeqCst))
    }
    async fn execute(
        &self,
        e: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        _: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        self.sent.lock().unwrap().push(e.clone());
        if self.fail.load(Ordering::SeqCst) {
            return Err(AutomationExecutionError {
                message: "No send".into(),
                send_accepted: self.ambiguous.load(Ordering::SeqCst),
                failure_status: None,
            });
        }
        let selected =
            self.actual_selection
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(CompanionSelection {
                    model: "gpt-5.6-sol".into(),
                    effort: "max".into(),
                });
        if !self.snapshot_only.load(Ordering::SeqCst) {
            events
                .send(CompanionEvent {
                    sequence: 1,
                    action_id: e.request.action_id.clone(),
                    at_ms: 90_000,
                    kind: CompanionEventKind::ModelVerified {
                        model: selected.model.clone(),
                        effort: selected.effort.clone(),
                    },
                })
                .unwrap();
            if self.verified_only.load(Ordering::SeqCst) {
                return Err(AutomationExecutionError {
                    message: "Model verified, but nothing was sent".into(),
                    send_accepted: false,
                    failure_status: None,
                });
            }
        }
        if let Some(change) = self.before_send.lock().unwrap().take() {
            change();
        }
        if !self.snapshot_only.load(Ordering::SeqCst) {
            events
                .send(CompanionEvent {
                    sequence: 2,
                    action_id: e.request.action_id.clone(),
                    at_ms: match self.accepted_at_ms.load(Ordering::SeqCst) {
                        0 => 100_000,
                        at => at,
                    },
                    kind: CompanionEventKind::SendAccepted,
                })
                .unwrap();
        }
        Ok(serde_json::from_value(serde_json::json!({"protocolVersion":1,"build":format!("test{}",PROTOCOL_BUILD_SUFFIX),"actionId":e.request.action_id,"state":"idle","sendAccepted":true,"canonicalUrl":e.request.url,"selected":{"model":selected.model,"effort":selected.effort},"lastEventSequence":2,"attention":null})).unwrap())
    }
}
struct Fixture {
    _root: tempfile::TempDir,
    config: codexify::types::AppConfig,
    task: ConversationIdentity,
    chat: Arc<codexify::markdown_chat::ChatFile>,
    store: Arc<AgentAutomationStore>,
    supervisor: Arc<AgentAutomationSupervisor>,
    executor: Arc<Executor>,
}
async fn fixture(enabled: bool) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let mut config = default_config(project);
    config.memory.dir = Some(root.path().join("state").display().to_string());
    config.markdown_chat.enabled = true;
    config.markdown_chat.automation.enabled = true;
    config.markdown_chat.automation.recovery_enhancements = true;
    config
        .markdown_chat
        .automation
        .retry_timings
        .infinite_retries = false;
    config.markdown_chat.automation.connector_name = "Codexify".into();
    config.markdown_chat.turn_renewal.enabled = enabled;
    let task = ConversationIdentity::from_openai_session(&format!("renewal-test-{}", root.path().file_name().unwrap().to_string_lossy())).unwrap();
    let chat = MarkdownChatStore::default()
        .chat(&config, Some(&task), &SessionState::new())
        .unwrap();
    chat.ensure().await.unwrap();
    let store = Arc::new(AgentAutomationStore::for_config(&config).unwrap());
    store
        .set_selection(&task, "gpt-6-astra", "max", "catalog", "account", 100)
        .unwrap();
    let pair = store
        .issue_pairing(&task, "https://chatgpt.com/c/renewal", 0, 100)
        .unwrap();
    store
        .record_pairing_canonical_url(&task, pair.generation, "https://chatgpt.com/c/renewal", 101)
        .unwrap();
    store
        .verify_pairing(&task, pair.generation, 0, &pair.reference, 102)
        .unwrap();
    let executor = Arc::new(Executor::default());
    let supervisor =
        AgentAutomationSupervisor::new(config.markdown_chat.automation.clone(), executor.clone());
    supervisor
        .register_task(AutomationTaskHandle::new(
            task.clone(),
            0,
            store.clone(),
            chat.clone(),
        ))
        .await
        .unwrap();
    let pair = store.load(&task).unwrap().pairing.generation;
    store
        .queue_action(
            &task,
            "original",
            AutomationActionKind::OfflineChat,
            pair,
            None,
            1000,
        )
        .unwrap();
    store
        .record_turn_send(
            &task,
            TurnSend {
                action_id: "original".into(),
                at_ms: 1000,
                model: Some("gpt-6-astra".into()),
                effort: Some("max".into()),
                pairing_generation: pair,
                owner_generation: 0,
            },
        )
        .unwrap();
    store
        .update_action_status(
            &task,
            "original",
            AutomationActionStatus::Idle,
            None,
            None,
            1000,
        )
        .unwrap();
    Fixture {
        _root: root,
        config,
        task,
        chat,
        store,
        supervisor,
        executor,
    }
}
const DUE: u64 = 1000 + 55 * 60_000;
#[tokio::test]
async fn missing_handoff_report_cannot_hold_an_idle_active_goal_forever() {
  for newer_goal in [false,true] {
    let f = fixture(true).await;
    let goal = f.store.set_goal(&f.task, "Continue the real work", 1000).unwrap();
    f.store.run_goal(&f.task, goal.revision, 1000).unwrap();
    let pair = f.store.load(&f.task).unwrap().pairing.generation;
    f.store.queue_action(&f.task, "goal-before-handoff", AutomationActionKind::OfflineGoal, pair, Some(goal.revision), 2000).unwrap();
    f.store.update_action_status(&f.task,"goal-before-handoff",AutomationActionStatus::Ambiguous,None,None,3000).unwrap();
    f.store.update_action_status(&f.task,"goal-before-handoff",AutomationActionStatus::Idle,None,None,3001).unwrap();
    f.store.turn_renewal_notice(&f.task, DUE).unwrap();
    if newer_goal {
        // Auto mode preserves an accepted older action while atomically
        // installing a newer active owner goal. Model that durable fixture.
        let mut state=f.store.load(&f.task).unwrap();
        let updated=state.goal.as_mut().unwrap();
        updated.revision+=1;updated.markdown="Updated owner objective".into();updated.updated_at_ms=DUE;
        std::fs::write(f.store.task_directory(&f.task).unwrap().join("automation.json"),serde_json::to_vec(&state).unwrap()).unwrap();
    }
    f.supervisor.scan_once(DUE + 10 * 60_000 - 1).await.unwrap();
    assert!(f.store.load(&f.task).unwrap().turn_renewal.handoff.is_some());
    f.supervisor.scan_once(DUE + 10 * 60_000).await.unwrap();
    let recovered = f.store.load(&f.task).unwrap();
    assert!(recovered.turn_renewal.handoff.is_none(), "a verified incomplete report must yield to a distinct goal continuation after the inactivity deadline");
    assert_eq!(recovered.retry_mode(), Some("continue_goal"));
    assert_eq!(recovered.action.send_accepted, Some(true), "accepted prior messages are never relabeled unsent");
    assert_eq!(recovered.goal.as_ref().unwrap().status, GoalStatus::Active);
    assert_eq!(recovered.action.goal_revision,Some(if newer_goal {2}else{1}));
    let due = recovered.automatic_retry_at_ms().unwrap();
    f.supervisor.scan_once(due + 1).await.unwrap();
    assert_eq!(f.supervisor.next_queued_kind().await, Some(AutomationActionKind::OfflineGoal));
  }
}

#[tokio::test]
async fn missing_report_after_accepted_renewal_continues_goal_without_replaying_renewal() {
  for newer_goal in [false,true] {
    let f=fixture(true).await;
    let goal=f.store.set_goal(&f.task,"Continue verified development",1000).unwrap();
    f.store.run_goal(&f.task,goal.revision,1000).unwrap();
    let pair=f.store.load(&f.task).unwrap().pairing.generation;
    f.store.queue_action(&f.task,"accepted-renewal",AutomationActionKind::TurnRenewal,pair,None,1000).unwrap();
    f.store.record_turn_send(&f.task,TurnSend{action_id:"accepted-renewal".into(),at_ms:1000,model:Some("gpt-6-astra".into()),effort:Some("max".into()),pairing_generation:pair,owner_generation:0}).unwrap();
    f.store.update_action_status(&f.task,"accepted-renewal",AutomationActionStatus::Ambiguous,None,None,3000).unwrap();
    f.store.update_action_status(&f.task,"accepted-renewal",AutomationActionStatus::Idle,None,None,3001).unwrap();
    if newer_goal {
        let next=f.store.set_goal(&f.task,"New explicit owner objective",4000).unwrap();
        f.store.run_goal(&f.task,next.revision,4000).unwrap();
    }
    f.store.turn_renewal_notice(&f.task,DUE).unwrap();
    f.supervisor.scan_once(DUE+10*60_000).await.unwrap();
    let recovered=f.store.load(&f.task).unwrap();
    assert!(recovered.turn_renewal.handoff.is_none(),"a renewal's missing handoff must not hold the active goal forever");
    assert_eq!(recovered.action.id.as_deref(),Some("accepted-renewal"));
    assert_eq!(recovered.action.kind,Some(AutomationActionKind::TurnRenewal));
    assert_eq!(recovered.action.send_accepted,Some(true));
    assert_eq!(recovered.retry_mode(),Some("continue_goal"));
    assert_eq!(recovered.action.goal_revision,Some(if newer_goal {2}else{1}));
    let due=recovered.automatic_retry_at_ms().unwrap();
    f.supervisor.scan_once(due+1).await.unwrap();
    assert_eq!(f.supervisor.next_queued_kind().await,Some(AutomationActionKind::OfflineGoal));
    assert_ne!(f.store.load(&f.task).unwrap().action.id.as_deref(),Some("accepted-renewal"));
  }
}

#[tokio::test]
async fn stalled_handoff_recovery_respects_pauses_activity_commands_and_failed_observations() {
  for kind in [AutomationActionKind::OfflineGoal,AutomationActionKind::TurnRenewal] {
    for guard in ["paused", "stop", "activity", "resident", "probe"] {
        let f = fixture(true).await;
        let goal = f.store.set_goal(&f.task, "Continue work", 1000).unwrap();
        f.store.run_goal(&f.task, goal.revision, 1000).unwrap();
        let pair = f.store.load(&f.task).unwrap().pairing.generation;
        f.store.queue_action(&f.task,"goal",kind,pair,(kind==AutomationActionKind::OfflineGoal).then_some(goal.revision),2000).unwrap();
        if kind==AutomationActionKind::TurnRenewal {
            f.store.record_turn_send(&f.task,TurnSend{action_id:"goal".into(),at_ms:1000,model:Some("gpt-6-astra".into()),effort:Some("max".into()),pairing_generation:pair,owner_generation:0}).unwrap();
        }
        f.store.update_action_status(&f.task,"goal",AutomationActionStatus::Ambiguous,None,None,3000).unwrap();
        f.store.update_action_status(&f.task,"goal",AutomationActionStatus::Idle,None,None,3001).unwrap();
        f.store.turn_renewal_notice(&f.task,DUE).unwrap();
        let mut command = None;
        let exec_store = codexify::exec_sessions::ConversationExecSessionStore::new();
        let session = exec_store.session_for(&f.task,&SessionState::new());
        match guard {
            "paused" => { f.store.stop_goal(&f.task,goal.revision,DUE+1).unwrap(); },
            "stop" => { f.chat.request_agent_stop("test-stop".into()).await.unwrap(); },
            "activity" => { f.chat.record_agent_call(DUE+9*60_000).await.unwrap(); },
            "resident" => { let (shell,cmd) = if cfg!(windows) {("powershell","Start-Sleep -Seconds 60")} else {("sh","sleep 60")}; command=Some(codexify::exec_sessions::start_exec_session(&session,&f.config,cmd,&f._root.path().join("project"),Some(shell)).unwrap()); },
            "probe" => { f.executor.probe_fail.store(true,Ordering::SeqCst); },
            _ => unreachable!(),
        }
        f.supervisor.scan_once(DUE+10*60_000).await.unwrap();
        if !["stop", "paused"].contains(&guard) { assert!(f.store.load(&f.task).unwrap().turn_renewal.handoff.is_some(),"guard {guard}"); }
        assert_eq!(f.supervisor.queued_count().await,0,"guard {guard}");
        if guard == "resident" {
            f.store.clear_turn_renewal(&f.task).unwrap();
            f.supervisor.scan_once(DUE+20*60_000).await.unwrap();
            assert_eq!(f.supervisor.queued_count().await,0,"a long resident command also protects ordinary goal continuation");
        }
        if let Some(command)=command { codexify::exec_sessions::kill_exec_session(&command); }
    }
  }
}
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[tokio::test]
async fn accepted_fallback_updates_saved_picker_and_statistics_with_or_without_events() {
    for (kind, preferred, actual, snapshot_only) in [
        (
            AutomationActionKind::OfflineChat,
            ("gpt-6-astra", "max"),
            ("gpt-5.6-sol", "max"),
            false,
        ),
        (
            AutomationActionKind::OfflineGoal,
            ("gpt-6-astra", "max"),
            ("gpt-5.6-sol", "max"),
            true,
        ),
        (
            AutomationActionKind::OfflineCombined,
            ("gpt-6-sol", "xhigh"),
            ("gpt-6-sol", "high"),
            false,
        ),
    ] {
        let f = fixture(false).await;
        let now = wall_ms();
        f.store
            .set_selection(&f.task, preferred.0, preferred.1, "catalog", "account", now)
            .unwrap();
        *f.executor.actual_selection.lock().unwrap() = Some(CompanionSelection {
            model: actual.0.into(),
            effort: actual.1.into(),
        });
        f.executor
            .snapshot_only
            .store(snapshot_only, Ordering::SeqCst);
        f.executor.accepted_at_ms.store(now, Ordering::SeqCst);
        if kind != AutomationActionKind::OfflineChat {
            let goal = f
                .store
                .set_goal(&f.task, "Complete the work", now - 60_000)
                .unwrap();
            f.store
                .run_goal(&f.task, goal.revision, now - 60_000)
                .unwrap();
        }
        if kind != AutomationActionKind::OfflineGoal {
            f.chat
                .append_user("fallback-request".into(), "Continue the task".into())
                .await
                .unwrap();
        }
        f.supervisor.scan_once(now).await.unwrap();
        assert_eq!(f.supervisor.next_queued_kind().await, Some(kind));
        f.supervisor.run_next(now).await.unwrap();
        let state = f.store.load(&f.task).unwrap();
        assert_eq!(
            (
                state.selection.model.as_deref(),
                state.selection.effort.as_deref()
            ),
            (Some(actual.0), Some(actual.1))
        );
        let send = state.turn_renewal.send.unwrap();
        assert_eq!(
            (send.model.as_deref(), send.effort.as_deref()),
            (Some(actual.0), Some(actual.1))
        );
        assert_eq!(
            AgentAutomationStore::for_config(&f.config)
                .unwrap()
                .load(&f.task)
                .unwrap()
                .selection,
            state.selection,
            "the picker must stay on the fallback after restart"
        );

        let analytics = f
            .store
            .task_directory(&f.task)
            .unwrap()
            .join("usage-analytics.json");
        let expected_key = format!("{}\u{1f}{}", actual.0, actual.1);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(7);
        loop {
            if let Ok(bytes) = std::fs::read(&analytics) {
                let data: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                let days = data["days"].as_object().unwrap();
                let turns: u64 = days
                    .values()
                    .filter_map(|day| day[&expected_key]["turns"].as_u64())
                    .sum();
                if turns == 1 {
                    assert_eq!(
                        days.values()
                            .flat_map(|day| day.as_object().unwrap().values())
                            .map(|totals| totals["turns"].as_u64().unwrap())
                            .sum::<u64>(),
                        1,
                        "event and snapshot proof must count one turn"
                    );
                    break;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "fallback turn was not attributed to {expected_key}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
}

#[tokio::test]
async fn fallback_proof_does_not_replace_a_newer_manual_selection_or_an_unsent_choice() {
    for manual_change in [false, true] {
        let f = fixture(false).await;
        let now = wall_ms();
        f.executor.accepted_at_ms.store(now, Ordering::SeqCst);
        if manual_change {
            let store = f.store.clone();
            let task = f.task.clone();
            *f.executor.before_send.lock().unwrap() = Some(Box::new(move || {
                store
                    .set_selection(&task, "gpt-6-sol", "high", "catalog", "account", now + 1)
                    .unwrap();
            }));
        } else {
            f.executor.verified_only.store(true, Ordering::SeqCst);
        }
        f.chat
            .queue_user_message(
                "fallback-request".into(),
                "Continue the task".into(),
                Vec::new(),
            )
            .await
            .unwrap();
        f.supervisor.scan_once(now).await.unwrap();
        let result = f.supervisor.run_next(now).await;
        let state = f.store.load(&f.task).unwrap();
        if manual_change {
            result.unwrap();
            assert_eq!(
                (
                    state.selection.model.as_deref(),
                    state.selection.effort.as_deref()
                ),
                (Some("gpt-6-sol"), Some("high"))
            );
            assert_eq!(
                state.turn_renewal.send.unwrap().model.as_deref(),
                Some("gpt-5.6-sol"),
                "statistics still use the actual send, independently of the next user selection"
            );
        } else {
            assert!(result.is_err());
            assert_eq!(state.selection.model.as_deref(), Some("gpt-6-astra"));
            assert_eq!(state.turn_renewal.send.unwrap().action_id, "original");
        }
    }
}

#[test]
fn opt_in_defaults_and_strict_model_limits() {
    let mut c = TurnRenewalConfig::default();
    assert!(!c.enabled);
    assert_eq!(c.minutes(Some("gpt-5.6-sol"), Some("max")), Some(115));
    assert_eq!(c.minutes(Some("gpt-6-astra"), Some("max")), Some(55));
    assert_eq!(c.minutes(Some("gpt-5.6-sol"), Some("high")), Some(55));
    assert_eq!(c.minutes(Some("new-model"), Some("max")), None);
    c.limits[0].minutes = 50;
    assert_eq!(c.minutes(Some("gpt-6-astra"), Some("max")), Some(50));
    assert!(c.validate().is_ok());
    c.limits.push(c.limits[0].clone());
    assert!(c.validate().is_err());
    c.limits.pop();
    c.limits[0].minutes = 0;
    assert!(c.validate().is_err());
}
#[tokio::test]
async fn exact_deadline_and_restart_keep_first_actual_send() {
    let f = fixture(true).await;
    assert!(
        f.store
            .turn_renewal_notice(&f.task, DUE - 1)
            .unwrap()
            .is_none()
    );
    let note = f.store.turn_renewal_notice(&f.task, DUE).unwrap().unwrap();
    assert!(note.contains("FINAL CHATGPT"));
    assert!(note.contains("Do not use chat_write"));
    let original = f.store.load(&f.task).unwrap();
    let mut duplicate = original.turn_renewal.send.clone().unwrap();
    duplicate.at_ms = DUE + 10;
    f.store.record_turn_send(&f.task, duplicate).unwrap();
    let reopened = AgentAutomationStore::for_config(&f.config).unwrap();
    assert_eq!(
        reopened.load(&f.task).unwrap().turn_renewal,
        original.turn_renewal
    );
    assert_eq!(
        reopened
            .turn_renewal_notice(&f.task, DUE + 50)
            .unwrap()
            .unwrap(),
        note
    );
}

#[test]
fn model_switches_preserve_legacy_entries_and_leave_unlisted_selections_alone() {
    let mut config: TurnRenewalConfig = serde_json::from_value(serde_json::json!({
        "enabled":true,"limits":[{"model":"gpt-5.6-sol","effort":"max","minutes":115}]
    }))
    .unwrap();
    assert!(config.enabled_for(Some("gpt-5.6-sol"), Some("max")));
    assert!(!config.enabled_for(Some("gpt-5.6-sol"), Some("high")));
    assert!(!config.enabled_for(Some("gpt-6-astra"), Some("max")));
    assert!(!config.enabled_for(None, None));
    config.limits[0].enabled = false;
    assert_eq!(config.minutes(Some("gpt-5.6-sol"), Some("max")), None);
    assert!(config.validate().is_ok());
    config.limits.clear();
    assert!(!config.enabled_for(Some("gpt-5.6-sol"), Some("max")));
}

#[tokio::test]
async fn selected_model_renews_at_115_minutes_using_actual_fallback_proof() {
    for (model, effort) in [
        (Some("gpt-5.6-sol"), Some("max")),
        (Some("gpt-5.6-sol"), Some("high")),
        (Some("gpt-6-astra"), Some("max")),
        (Some("unlisted-model"), Some("max")),
        (None, None),
    ] {
        let f = fixture(true).await;
        let mut config = f.config.clone();
        config.markdown_chat.turn_renewal = serde_json::from_value(serde_json::json!({
            "enabled":true,"limits":[{"model":"gpt-5.6-sol","effort":"max","minutes":115,"enabled":true}]
        })).unwrap();
        let store = AgentAutomationStore::for_config(&config).unwrap();
        let generation = store.load(&f.task).unwrap().pairing.generation;
        store
            .queue_action(
                &f.task,
                "actual-send",
                AutomationActionKind::OfflineChat,
                generation,
                None,
                1000,
            )
            .unwrap();
        store
            .record_turn_send(
                &f.task,
                TurnSend {
                    action_id: "actual-send".into(),
                    at_ms: 1000,
                    model: model.map(str::to_owned),
                    effort: effort.map(str::to_owned),
                    pairing_generation: generation,
                    owner_generation: 0,
                },
            )
            .unwrap();
        assert_eq!(
            store.load(&f.task).unwrap().selection.model.as_deref(),
            Some("gpt-6-astra")
        );
        assert!(
            store
                .turn_renewal_notice(&f.task, 1000 + 115 * 60_000 - 1)
                .unwrap()
                .is_none()
        );
        let selected = model == Some("gpt-5.6-sol") && effort == Some("max");
        assert_eq!(
            store
                .turn_renewal_notice(&f.task, 1000 + 115 * 60_000)
                .unwrap()
                .is_some(),
            selected
        );
        assert_eq!(
            store
                .turn_renewal_notice(&f.task, u64::MAX)
                .unwrap()
                .is_some(),
            selected
        );
    }
}

#[tokio::test]
async fn disabling_only_a_model_cancels_its_queued_renewal_before_send() {
    let f = fixture(true).await;
    let path = f._root.path().join("config.json");
    let document = |enabled| {
        serde_json::json!({"agentChat":{"automation":{"connectorName":"Codexify"},"turnRenewal":{
            "enabled":true,"limits":[{"model":"gpt-6-astra","effort":"max","minutes":55,"enabled":enabled}]
        }}})
    };
    std::fs::write(&path, document(true).to_string()).unwrap();
    let mut config = f.config.clone();
    config.loaded_config_path = Some(path.clone());
    let store = Arc::new(AgentAutomationStore::for_config(&config).unwrap());
    let supervisor =
        AgentAutomationSupervisor::new(config.markdown_chat.automation.clone(), f.executor.clone());
    supervisor
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    store.turn_renewal_notice(&f.task, DUE).unwrap().unwrap();
    f.executor.complete.store(true, Ordering::SeqCst);
    supervisor.scan_once(DUE + 10_000).await.unwrap();
    assert_eq!(supervisor.queued_count().await, 1);
    std::fs::write(&path, document(false).to_string()).unwrap();
    // No scan between the settings change and dispatch: the final guard must
    // reread the model switch rather than send the previously queued action.
    assert!(supervisor.run_next(DUE + 10_001).await.unwrap().is_none());
    assert!(f.executor.sent.lock().unwrap().is_empty());
    supervisor.scan_once(DUE + 20_000).await.unwrap();
    assert_eq!(supervisor.queued_count().await, 0);
    assert!(store.load(&f.task).unwrap().turn_renewal.handoff.is_none());
    assert!(store.turn_renewal_settings().unwrap().enabled);
}
#[tokio::test]
async fn no_goal_renewal_waits_for_final_report_and_sends_once_without_queue() {
    let f = fixture(true).await;
    f.chat
        .queue_user_message("queued".into(), "Private next request".into(), Vec::new())
        .await
        .unwrap();
    f.store.turn_renewal_notice(&f.task, DUE).unwrap();
    assert!(
        f.chat
            .release_queued_message(DUE + 10_000)
            .await
            .unwrap()
            .is_none()
    );
    f.supervisor.scan_once(DUE + 10_000).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    f.executor.complete.store(true, Ordering::SeqCst);
    f.supervisor.scan_once(DUE + 11_000).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 1);
    f.supervisor.run_next(DUE + 11_000).await.unwrap();
    let sent = f.executor.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert!(matches!(
        sent[0].request.prompt,
        ActionPrompt::TurnRenewal { .. }
    ));
    assert!(sent[0].unread_message_ids.is_empty());
    drop(sent);
    assert!(
        f.store
            .load(&f.task)
            .unwrap()
            .turn_renewal
            .handoff
            .is_none()
    );
    let send = f.store.load(&f.task).unwrap().turn_renewal.send.unwrap();
    assert_eq!(send.at_ms, 100_000);
    assert_eq!(send.model.as_deref(), Some("gpt-5.6-sol"));
    assert!(
        f.store
            .turn_renewal_notice(&f.task, 100_000 + 55 * 60_000)
            .unwrap()
            .is_none()
    );
    let selection = f.store.load(&f.task).unwrap().selection;
    assert_eq!(selection.model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(selection.effort.as_deref(), Some("max"));
    // Renewal and the saved picker selection both follow the actual fallback.
    assert!(
        f.store
            .turn_renewal_notice(&f.task, 100_000 + 115 * 60_000)
            .unwrap()
            .is_some()
    );
}
#[tokio::test]
async fn active_goal_and_queue_survive_handoff() {
    let f = fixture(true).await;
    let goal = f.store.set_goal(&f.task, "Keep working", 1000).unwrap();
    f.store.run_goal(&f.task, goal.revision, 1000).unwrap();
    f.store.turn_renewal_notice(&f.task, DUE).unwrap();
    f.executor.complete.store(true, Ordering::SeqCst);
    f.supervisor.scan_once(DUE + 10_000).await.unwrap();
    f.supervisor.run_next(DUE + 10_000).await.unwrap();
    let now = f.store.load(&f.task).unwrap();
    assert_eq!(now.goal.as_ref().unwrap().revision, goal.revision);
    assert_eq!(now.goal.as_ref().unwrap().status, GoalStatus::Active);
}

#[tokio::test]
async fn new_conversation_owner_drops_old_renewal_but_keeps_goal_and_queued_chat() {
    // Both an unexpired send and an already-requested renewal belong to the
    // previous physical conversation, even though the task itself continues.
    // This remains true if the old conversation unlinked its browser pairing.
    for (handoff_pending, unlinked) in [(false, false), (true, false), (false, true), (true, true)]
    {
        let f = fixture(true).await;
        let goal = f
            .store
            .set_goal(&f.task, "Continue after handoff", 1_000)
            .unwrap();
        f.store.run_goal(&f.task, goal.revision, 1_000).unwrap();
        f.chat
            .queue_user_message(
                "pending-after-handoff".into(),
                "Keep this request private until the new owner is ready".into(),
                vec![],
            )
            .await
            .unwrap();
        if handoff_pending {
            assert!(f.store.turn_renewal_notice(&f.task, DUE).unwrap().is_some());
        }
        assert!(!f.store.load(&f.task).unwrap().turn_renewal.is_empty());
        if unlinked {
            f.store.unlink_pairing(&f.task, 2_000).unwrap();
        }

        f.supervisor
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                1,
                f.store.clone(),
                f.chat.clone(),
            ))
            .await
            .unwrap();

        let state = f.store.load(&f.task).unwrap();
        assert!(
            state.turn_renewal.is_empty(),
            "stale owner renewal remained"
        );
        assert_eq!(state.pairing.verified_owner_generation, None);
        assert_eq!(state.action.status, AutomationActionStatus::Unverified);
        assert_eq!(state.goal.as_ref().unwrap().status, GoalStatus::Active);
        assert_eq!(state.goal.as_ref().unwrap().revision, goal.revision);
        assert_eq!(f.chat.queued_messages().await.unwrap().len(), 1);
        assert!(
            f.store
                .turn_renewal_notice(&f.task, DUE + 1_000_000)
                .unwrap()
                .is_none()
        );
        assert_eq!(f.supervisor.queued_count().await, 0);
    }
}

#[tokio::test]
async fn feature_disabled_never_nudges_and_stop_prevents_continuation() {
    let f = fixture(false).await;
    assert!(
        f.store
            .turn_renewal_notice(&f.task, u64::MAX)
            .unwrap()
            .is_none()
    );
    let f = fixture(true).await;
    f.store.turn_renewal_notice(&f.task, DUE).unwrap();
    f.chat.request_agent_stop("stop".into()).await.unwrap();
    f.executor.complete.store(true, Ordering::SeqCst);
    f.supervisor.scan_once(DUE + 10_000).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    assert!(
        f.store
            .load(&f.task)
            .unwrap()
            .turn_renewal
            .handoff
            .is_none()
    );
}
#[tokio::test]
async fn ambiguous_continuation_never_replays() {
    let f = fixture(true).await;
    f.store.turn_renewal_notice(&f.task, DUE).unwrap();
    f.executor.complete.store(true, Ordering::SeqCst);
    f.executor.fail.store(true, Ordering::SeqCst);
    f.executor.ambiguous.store(true, Ordering::SeqCst);
    f.supervisor.scan_once(DUE + 10_000).await.unwrap();
    let _ = f.supervisor.run_next(DUE + 10_000).await;
    f.supervisor.scan_once(DUE + 1_000_000).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    assert_eq!(f.executor.sent.lock().unwrap().len(), 1);
    assert!(
        f.store
            .load(&f.task)
            .unwrap()
            .turn_renewal
            .handoff
            .is_some()
    );
}

#[tokio::test]
async fn hot_settings_disable_pending_handoff_without_restart() {
    let f = fixture(true).await;
    let path = f._root.path().join("config.json");
    std::fs::write(
        &path,
        r#"{"agentChat":{"automation":{"connectorName":"Codexify"},"rolloutEnabled":false,"turnRenewal":{"enabled":true}}}"#,
    )
    .unwrap();
    let mut config = f.config.clone();
    config.loaded_config_path = Some(path.clone());
    let store = Arc::new(AgentAutomationStore::for_config(&config).unwrap());
    assert!(!codexify::global_settings::rollout_enabled(&config).unwrap());
    assert!(store.turn_renewal_notice(&f.task, DUE).unwrap().is_some());
    let supervisor =
        AgentAutomationSupervisor::new(config.markdown_chat.automation.clone(), f.executor.clone());
    supervisor
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    std::fs::write(
        &path,
        r#"{"agentChat":{"automation":{"connectorName":"Codexify"},"rolloutEnabled":true,"turnRenewal":{"enabled":false}}}"#,
    )
    .unwrap();
    assert!(codexify::global_settings::rollout_enabled(&config).unwrap());
    f.executor.complete.store(true, Ordering::SeqCst);
    supervisor.scan_once(DUE + 10_000).await.unwrap();
    assert_eq!(supervisor.queued_count().await, 0);
    assert!(store.load(&f.task).unwrap().turn_renewal.handoff.is_none());
}

#[tokio::test]
async fn pending_handoff_resumes_after_restart_without_a_queue_file() {
    let f = fixture(true).await;
    assert!(!f.chat.path().with_file_name("message-queue.json").exists());
    f.store.turn_renewal_notice(&f.task, DUE).unwrap();
    let restarted = AgentAutomationSupervisor::new(
        f.config.markdown_chat.automation.clone(),
        f.executor.clone(),
    );
    let store = Arc::new(AgentAutomationStore::for_config(&f.config).unwrap());
    restarted
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            store,
            f.chat.clone(),
        ))
        .await
        .unwrap();
    f.executor.complete.store(true, Ordering::SeqCst);
    restarted.scan_once(DUE + 10_000).await.unwrap();
    assert_eq!(restarted.queued_count().await, 1);
    restarted.run_next(DUE + 10_000).await.unwrap();
    assert_eq!(f.executor.sent.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn completed_handoff_supersedes_old_browser_error_but_pausing_goal_cancels_renewal() {
    let f = fixture(true).await;
    f.store.turn_renewal_notice(&f.task, DUE).unwrap();
    f.store
        .update_action_status(
            &f.task,
            "original",
            AutomationActionStatus::Ambiguous,
            Some("Old interface error"),
            None,
            DUE,
        )
        .unwrap();
    f.executor.complete.store(true, Ordering::SeqCst);
    f.supervisor.scan_once(DUE + 10_000).await.unwrap();
    assert_eq!(
        f.supervisor.next_queued_kind().await,
        Some(AutomationActionKind::TurnRenewal)
    );
    f.supervisor.run_next(DUE + 10_000).await.unwrap();
    assert_eq!(f.executor.sent.lock().unwrap().len(), 1);
    let f = fixture(true).await;
    let g = f.store.set_goal(&f.task, "Work", 1000).unwrap();
    f.store.run_goal(&f.task, g.revision, 1000).unwrap();
    f.store.turn_renewal_notice(&f.task, DUE).unwrap();
    f.store.stop_goal(&f.task, g.revision, DUE + 1).unwrap();
    f.executor.complete.store(true, Ordering::SeqCst);
    f.supervisor.scan_once(DUE + 10_000).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
}
#[tokio::test]
async fn unknown_actual_selection_is_not_renewed() {
    let f = fixture(true).await;
    let pair = f.store.load(&f.task).unwrap().pairing.generation;
    f.store
        .queue_action(
            &f.task,
            "unknown-selection",
            AutomationActionKind::OfflineChat,
            pair,
            None,
            1000,
        )
        .unwrap();
    f.store
        .record_turn_send(
            &f.task,
            TurnSend {
                action_id: "unknown-selection".into(),
                at_ms: 1000,
                model: None,
                effort: None,
                pairing_generation: pair,
                owner_generation: 0,
            },
        )
        .unwrap();
    f.store
        .update_action_status(
            &f.task,
            "unknown-selection",
            AutomationActionStatus::Idle,
            None,
            None,
            1000,
        )
        .unwrap();
    assert!(
        f.store
            .turn_renewal_notice(&f.task, DUE - 1)
            .unwrap()
            .is_none()
    );
    assert!(f.store.turn_renewal_notice(&f.task, DUE).unwrap().is_none());
    assert!(
        f.store
            .turn_renewal_notice(&f.task, u64::MAX)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn native_activity_after_verified_handoff_clears_uncertain_receipt_and_late_errors() {
    let f = fixture(true).await;
    f.store.turn_renewal_notice(&f.task, DUE).unwrap();
    f.executor.complete.store(true, Ordering::SeqCst);
    f.executor.fail.store(true, Ordering::SeqCst);
    f.executor.ambiguous.store(true, Ordering::SeqCst);
    f.supervisor.scan_once(DUE + 10_000).await.unwrap();
    let _ = f.supervisor.run_next(DUE + 10_000).await;
    let state = f.store.load(&f.task).unwrap();
    let at = state.action.updated_at_ms.unwrap() + 1;
    assert!(f.store.record_turn_activity(&f.task, 0, at).unwrap());
    let id = state.action.id.unwrap();
    let state = f.store.load(&f.task).unwrap();
    assert!(state.turn_renewal.handoff.is_none());
    assert_eq!(state.action.status, AutomationActionStatus::Idle);
    assert_eq!(state.action.send_accepted, Some(true));
    f.store
        .update_action_status(
            &f.task,
            &id,
            AutomationActionStatus::Ambiguous,
            Some("Late browser error"),
            None,
            at + 1,
        )
        .unwrap();
    assert!(f.store.load(&f.task).unwrap().action.last_error.is_none());
    let before = f.store.load(&f.task).unwrap();
    f.store
        .record_turn_send(
            &f.task,
            TurnSend {
                action_id: id,
                at_ms: at - 10,
                model: Some("gpt-5.6-sol".into()),
                effort: Some("max".into()),
                pairing_generation: before.pairing.generation,
                owner_generation: 0,
            },
        )
        .unwrap();
    let confirmed = f.store.load(&f.task).unwrap().turn_renewal.send.unwrap();
    assert_eq!(confirmed.at_ms, at - 10);
    assert_eq!(confirmed.model.as_deref(), Some("gpt-5.6-sol"));
    assert!(
        f.store
            .turn_renewal_notice(&f.task, at + 55 * 60_000)
            .unwrap()
            .is_none()
    );
    assert!(
        f.store
            .turn_renewal_notice(&f.task, at + 1)
            .unwrap()
            .is_none()
    );
}
#[tokio::test]
async fn first_native_call_releases_continuation_grace_for_normal_completion() {
    let f = fixture(true).await;
    f.executor.accepted_at_ms.store(wall_ms(), Ordering::SeqCst);
    f.store.turn_renewal_notice(&f.task, DUE).unwrap();
    f.chat
        .queue_user_message(
            "after-renewal".into(),
            "Private later request".into(),
            vec![],
        )
        .await
        .unwrap();
    f.executor.complete.store(true, Ordering::SeqCst);
    f.supervisor.scan_once(DUE + 10_000).await.unwrap();
    f.supervisor.run_next(DUE + 10_000).await.unwrap();
    assert!(
        f.store
            .load(&f.task)
            .unwrap()
            .turn_renewal
            .resume_wait_until_ms
            .is_some()
    );
    let now = wall_ms();
    f.chat.record_agent_call(now).await.unwrap();
    f.store.record_turn_activity(&f.task, 0, now).unwrap();
    assert!(
        f.store
            .load(&f.task)
            .unwrap()
            .turn_renewal
            .resume_wait_until_ms
            .is_none()
    );
    assert!(f.chat.queue_turn_completed().await.unwrap());
    assert_eq!(
        f.chat
            .release_queued_message(now)
            .await
            .unwrap()
            .unwrap()
            .id,
        "after-renewal"
    );
}

#[tokio::test]
async fn stalled_goal_renewal_retries_after_one_minute_only_with_fresh_report_proof() {
    let f = fixture(true).await;
    let goal = f.store.set_goal(&f.task, "Keep working", 1000).unwrap();
    f.store.run_goal(&f.task, goal.revision, 1000).unwrap();
    f.store.turn_renewal_notice(&f.task, DUE).unwrap();
    f.executor.complete.store(true, Ordering::SeqCst);
    f.executor.fail.store(true, Ordering::SeqCst);
    f.executor.ambiguous.store(true, Ordering::SeqCst);
    f.supervisor.scan_once(DUE + 10_000).await.unwrap();
    let _ = f.supervisor.run_next(DUE + 10_000).await;
    let failure = f.store.load(&f.task).unwrap();
    let failed_at = failure.action.updated_at_ms.unwrap();
    let retry_at = failure.automatic_retry_at_ms().unwrap();
    f.supervisor
        .scan_once(failed_at + 60_000 - 1)
        .await
        .unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    f.executor.complete.store(false, Ordering::SeqCst);
    f.supervisor.scan_once(retry_at).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    f.executor.complete.store(true, Ordering::SeqCst);
    f.supervisor.scan_once(retry_at + 1).await.unwrap();
    assert_eq!(
        f.supervisor.next_queued_kind().await,
        Some(AutomationActionKind::TurnRenewal)
    );
    f.executor.fail.store(false, Ordering::SeqCst);
    f.supervisor.run_next(failed_at + 60_001).await.unwrap();
    assert_eq!(f.executor.sent.lock().unwrap().len(), 2);
    assert!(
        f.store
            .load(&f.task)
            .unwrap()
            .turn_renewal
            .handoff
            .is_none()
    );
}

#[tokio::test]
async fn stalled_goal_renewal_stops_after_three_retries_across_restart() {
    let f = fixture(true).await;
    let goal = f.store.set_goal(&f.task, "Keep working", 1000).unwrap();
    f.store.run_goal(&f.task, goal.revision, 1000).unwrap();
    f.store.turn_renewal_notice(&f.task, DUE).unwrap();
    f.executor.complete.store(true, Ordering::SeqCst);
    f.executor.fail.store(true, Ordering::SeqCst);
    f.executor.ambiguous.store(true, Ordering::SeqCst);
    f.supervisor.scan_once(DUE + 10_000).await.unwrap();
    assert!(f.supervisor.run_next(DUE + 10_000).await.is_err());
    for (attempt, delay) in [60_000, 120_000, 240_000].into_iter().enumerate() {
        let failed = f.store.load(&f.task).unwrap();
        assert_eq!(failed.automatic_retry_attempts(), attempt as u32);
        assert_eq!(failed.automatic_retry_limit(), 3);
        let due = failed.automatic_retry_at_ms().unwrap();
        let earliest = failed.action.updated_at_ms.unwrap() + delay;
        assert!((earliest..=earliest + delay / 10).contains(&due));
        let supervisor = AgentAutomationSupervisor::new(
            f.config.markdown_chat.automation.clone(),
            f.executor.clone(),
        );
        supervisor
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                Arc::new(AgentAutomationStore::for_config(&f.config).unwrap()),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        supervisor.scan_once(due - 1).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 0);
        supervisor.scan_once(due).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 1);
        assert!(supervisor.run_next(due + 1).await.is_err());
    }
    let exhausted = f.store.load(&f.task).unwrap();
    assert_eq!(exhausted.automatic_retry_attempts(), 3);
    assert!(exhausted.automatic_retry_at_ms().is_none());
    f.supervisor
        .scan_once(exhausted.action.updated_at_ms.unwrap() + 86_400_000)
        .await
        .unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    assert_eq!(f.executor.sent.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn combined_message_goal_handoff_recovers_without_replaying_messages() {
  for newer_goal in [false,true] {
    let f = fixture(true).await;
    let goal = f.store.set_goal(&f.task, "Continue the real work", 1000).unwrap();
    f.store.run_goal(&f.task, goal.revision, 1000).unwrap();
    let pair = f.store.load(&f.task).unwrap().pairing.generation;
    f.store.queue_action(&f.task, "goal-before-handoff", AutomationActionKind::OfflineCombined, pair, Some(goal.revision), 2000).unwrap();
    f.store.update_action_status(&f.task,"goal-before-handoff",AutomationActionStatus::Ambiguous,None,None,3000).unwrap();
    f.store.update_action_status(&f.task,"goal-before-handoff",AutomationActionStatus::Idle,None,None,3001).unwrap();
    let mut accepted=f.store.load(&f.task).unwrap();
    accepted.action.messages_fenced=true;
    std::fs::write(f.store.task_directory(&f.task).unwrap().join("automation.json"),serde_json::to_vec(&accepted).unwrap()).unwrap();
    f.store.turn_renewal_notice(&f.task, DUE).unwrap();
    if newer_goal {
        // Auto mode preserves an accepted older action while atomically
        // installing a newer active owner goal. Model that durable fixture.
        let mut state=f.store.load(&f.task).unwrap();
        let updated=state.goal.as_mut().unwrap();
        updated.revision+=1;updated.markdown="Updated owner objective".into();updated.updated_at_ms=DUE;
        std::fs::write(f.store.task_directory(&f.task).unwrap().join("automation.json"),serde_json::to_vec(&state).unwrap()).unwrap();
    }
    f.supervisor.scan_once(DUE + 10 * 60_000 - 1).await.unwrap();
    assert!(f.store.load(&f.task).unwrap().turn_renewal.handoff.is_some());
    f.supervisor.scan_once(DUE + 10 * 60_000).await.unwrap();
    let recovered = f.store.load(&f.task).unwrap();
    assert!(recovered.turn_renewal.handoff.is_none(), "a verified incomplete report must yield to a distinct goal continuation after the inactivity deadline");
    assert_eq!(recovered.retry_mode(), Some("continue_goal"));
    assert_eq!(recovered.action.send_accepted, Some(true), "accepted prior messages are never relabeled unsent");
    assert_eq!(recovered.goal.as_ref().unwrap().status, GoalStatus::Active);
    assert_eq!(recovered.action.goal_revision,Some(if newer_goal {2}else{1}));
    let due = recovered.automatic_retry_at_ms().unwrap();
    f.supervisor.scan_once(due + 1).await.unwrap();
    assert_eq!(f.supervisor.next_queued_kind().await, Some(AutomationActionKind::OfflineGoal));
  }
}


#[tokio::test]
async fn combined_handoff_without_message_fence_never_replays_unconfirmed_messages() {
    let f=fixture(true).await;
    let goal=f.store.set_goal(&f.task,"Continue",1000).unwrap();
    f.store.run_goal(&f.task,goal.revision,1000).unwrap();
    let pair=f.store.load(&f.task).unwrap().pairing.generation;
    f.store.queue_action(&f.task,"unfenced-combined",AutomationActionKind::OfflineCombined,pair,Some(goal.revision),2000).unwrap();
    f.store.update_action_status(&f.task,"unfenced-combined",AutomationActionStatus::Ambiguous,None,None,3000).unwrap();
    f.store.update_action_status(&f.task,"unfenced-combined",AutomationActionStatus::Idle,None,None,3001).unwrap();
    f.store.turn_renewal_notice(&f.task,DUE).unwrap();
    f.supervisor.scan_once(DUE+10*60_000).await.unwrap();
    let state=f.store.load(&f.task).unwrap();
    assert!(state.turn_renewal.handoff.is_some());
    assert_eq!(state.action.id.as_deref(),Some("unfenced-combined"));
    assert_eq!(state.action.send_accepted,Some(true));
    assert_eq!(state.retry_mode(),None);
}

#[tokio::test]
async fn cleared_native_action_does_not_leave_an_active_goal_behind_a_missing_handoff() {
  for guard in ["none","timestamp","probe","recent","paused","stop","no_activity"] {
    let f=fixture(true).await;
    let first=f.store.set_goal(&f.task,"Original implementation",1100).unwrap();
    f.store.run_goal(&f.task,first.revision,1100).unwrap();
    let pair=f.store.load(&f.task).unwrap().pairing.generation;
    f.store.queue_action(&f.task,"accepted-native-turn",AutomationActionKind::OfflineGoal,pair,Some(first.revision),1200).unwrap();
    f.store.record_turn_send(&f.task,TurnSend{action_id:"accepted-native-turn".into(),at_ms:1200,model:Some("gpt-6-astra".into()),effort:Some("max".into()),pairing_generation:pair,owner_generation:0}).unwrap();
    f.store.update_action_status(&f.task,"accepted-native-turn",AutomationActionStatus::Idle,None,None,1300).unwrap();
    let next=f.store.set_goal(&f.task,"Current owner goal after native progress",1400).unwrap();
    f.store.run_goal(&f.task,next.revision,1400).unwrap();
    assert!(f.store.load(&f.task).unwrap().action.id.is_none());
    if guard == "timestamp" {
      let mut state = f.store.load(&f.task).unwrap();
      state.action.updated_at_ms = Some(1450);
      std::fs::write(f.store.task_directory(&f.task).unwrap().join("automation.json"), serde_json::to_vec(&state).unwrap()).unwrap();
    }
    if guard!="no_activity" {f.chat.record_agent_call(1500).await.unwrap();}
    let due=DUE+1200;
    assert!(f.store.turn_renewal_notice(&f.task,due).unwrap().is_some());
    match guard {
      "probe"=>{f.executor.probe_fail.store(true,Ordering::SeqCst);},
      "recent"=>{f.chat.record_agent_call(due+9*60_000).await.unwrap();},
      "paused"=>{f.store.stop_goal(&f.task,next.revision,due+1).unwrap();},
      "stop"=>{f.chat.request_agent_stop("explicit-stop".into()).await.unwrap();},
      _=>{}
    }
    f.supervisor.scan_once(due+10*60_000-1).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await,0);
    f.supervisor.scan_once(due+10*60_000).await.unwrap();
    let after=f.store.load(&f.task).unwrap();
    if guard=="none" || guard=="timestamp" {
      assert!(after.turn_renewal.handoff.is_none(),"native progress cleared the old action, not the active goal's right to a distinct continuation");
      assert_eq!(f.supervisor.next_queued_kind().await,Some(AutomationActionKind::OfflineGoal));
      assert_ne!(after.action.id.as_deref(),Some("accepted-native-turn"));
      assert_eq!(after.action.send_accepted,Some(false));
      assert_eq!(after.goal.unwrap().revision,next.revision);
    } else {assert_eq!(f.supervisor.queued_count().await,0,"guard {guard}");}
  }
}
