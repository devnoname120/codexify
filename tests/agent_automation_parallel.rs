use async_trait::async_trait;
use codexify::agent_automation::*;
use codexify::config::default_config;
use codexify::exec_sessions::SessionState;
use codexify::markdown_chat::{AUTOMATION_OFFLINE_AFTER_MS, MarkdownChatStore};
use codexify::project_bindings::ConversationIdentity;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

struct ParallelExecutor {
    started: mpsc::UnboundedSender<String>,
    prepare_started: mpsc::UnboundedSender<()>,
    release: Semaphore,
    prepare_release: Semaphore,
    active: AtomicUsize,
    maximum: AtomicUsize,
    calls: Mutex<HashMap<String, usize>>,
    ambiguous: bool,
}

struct Active<'a>(&'a AtomicUsize);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl AutomationExecutor for ParallelExecutor {
    fn max_parallel_actions(&self) -> usize {
        2
    }
    async fn prepare_new_chat(
        &self,
        _: Option<&str>,
        _: CancellationToken,
    ) -> Result<CapabilityCatalog, String> {
        self.prepare_started.send(()).unwrap();
        self.prepare_release.acquire().await.unwrap().forget();
        Ok(CapabilityCatalog {
            protocol_version: 1,
            build: "fixture".into(),
            account_fingerprint: "account".into(),
            catalog_revision: "catalog".into(),
            models: Vec::new(),
        })
    }
    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        _: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        *self
            .calls
            .lock()
            .unwrap()
            .entry(execution.task_key.clone())
            .or_default() += 1;
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _active = Active(&self.active);
        self.maximum.fetch_max(active, Ordering::SeqCst);
        self.started.send(execution.task_key.clone()).unwrap();
        self.release.acquire().await.unwrap().forget();
        for (index, kind) in [
            CompanionEventKind::SendAccepted,
            CompanionEventKind::SubmissionComplete {
                canonical_url: execution.request.expected_url.clone(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            events
                .send(CompanionEvent {
                    sequence: index as u64 + 1,
                    action_id: execution.request.action_id.clone(),
                    at_ms: 1001,
                    kind,
                })
                .unwrap();
        }
        if self.ambiguous {
            return Err(AutomationExecutionError {
                message: "The accepted request lost its response transport".into(),
                send_accepted: true,
                failure_status: None,
            });
        }
        Ok(CompanionActionSnapshot {
            protocol_version: 1,
            build: "fixture".into(),
            action_id: execution.request.action_id,
            state: CompanionActionState::Idle,
            send_accepted: true,
            canonical_url: Some(execution.request.expected_url),
            selected: Some(CompanionSelection {
                model: execution.request.model.unwrap(),
                effort: execution.request.effort.unwrap(),
            }),
            last_event_sequence: 2,
            cooldown_until_ms: None,
            attention: None,
        })
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    supervisor: Arc<AgentAutomationSupervisor>,
    executor: Arc<ParallelExecutor>,
    started: mpsc::UnboundedReceiver<String>,
    prepared: mpsc::UnboundedReceiver<()>,
    tasks: Vec<ConversationIdentity>,
    now: u64,
}

async fn fixture(count: usize, shared_url: bool, ambiguous: bool) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mut config = default_config(project);
    config.memory.dir = Some(root.path().join("state").display().to_string());
    config.markdown_chat.enabled = true;
    config.markdown_chat.automation.enabled = true;
    config.markdown_chat.automation.auto_approve_tool_calls = false;
    config.markdown_chat.automation.automatic_retries = false;
    config.markdown_chat.automation.connector_name = "Fixture".into();
    config.markdown_chat.automation.descriptor_path = root
        .path()
        .join("browser/descriptor.json")
        .display()
        .to_string();
    let (started_tx, started) = mpsc::unbounded_channel();
    let (prepared_tx, prepared) = mpsc::unbounded_channel();
    let executor = Arc::new(ParallelExecutor {
        started: started_tx,
        prepare_started: prepared_tx,
        release: Semaphore::new(0),
        prepare_release: Semaphore::new(0),
        active: AtomicUsize::new(0),
        maximum: AtomicUsize::new(0),
        calls: Mutex::new(HashMap::new()),
        ambiguous,
    });
    let supervisor =
        AgentAutomationSupervisor::new(config.markdown_chat.automation.clone(), executor.clone());
    let store = Arc::new(AgentAutomationStore::for_config(&config).unwrap());
    let mut tasks = Vec::new();
    for index in 0..count {
        let task = ConversationIdentity::from_openai_session(&format!("parallel-{index}")).unwrap();
        let chat = MarkdownChatStore::default()
            .chat(&config, Some(&task), &SessionState::new())
            .unwrap();
        chat.ensure().await.unwrap();
        store
            .set_selection(&task, "gpt-6-sol", "high", "catalog", "account", 100)
            .unwrap();
        let url = format!(
            "https://chatgpt.com/c/parallel-{}",
            if shared_url { 0 } else { index }
        );
        let pairing = store.issue_pairing(&task, &url, 0, 100).unwrap();
        store
            .record_pairing_canonical_url(&task, pairing.generation, &url, 101)
            .unwrap();
        store
            .verify_pairing(&task, pairing.generation, 0, &pairing.reference, 102)
            .unwrap();
        chat.record_agent_call(1000).await.unwrap();
        chat.append_user(
            format!("message-{index}"),
            "Implement and verify the owned module".into(),
        )
        .await
        .unwrap();
        supervisor
            .register_task(AutomationTaskHandle::new(
                task.clone(),
                0,
                store.clone(),
                chat,
            ))
            .await
            .unwrap();
        tasks.push(task);
    }
    let now = 1000 + AUTOMATION_OFFLINE_AFTER_MS;
    supervisor.scan_once(now).await.unwrap();
    assert_eq!(supervisor.queued_count().await, count);
    Fixture {
        _root: root,
        supervisor,
        executor,
        started,
        prepared,
        tasks,
        now,
    }
}

async fn next_start(receiver: &mut mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}

fn run(f: &Fixture) -> tokio::task::JoinHandle<Result<Option<CompanionActionSnapshot>, String>> {
    let supervisor = f.supervisor.clone();
    let now = f.now + 1;
    tokio::spawn(async move { supervisor.run_next(now).await })
}

#[tokio::test]
async fn independent_chats_prepare_in_two_bounded_lanes() {
    let mut f = fixture(3, false, false).await;
    let first = run(&f);
    let second = run(&f);
    let third = run(&f);
    let a = next_start(&mut f.started).await;
    let b = next_start(&mut f.started).await;
    assert_ne!(a, b);
    assert!(
        tokio::time::timeout(Duration::from_millis(60), f.started.recv())
            .await
            .is_err(),
        "a third action must wait for actual capacity"
    );
    assert_eq!(f.executor.active.load(Ordering::SeqCst), 2);
    f.executor.release.add_permits(1);
    let c = next_start(&mut f.started).await;
    assert!(c != a && c != b);
    f.executor.release.add_permits(2);
    for action in [first, second, third] {
        action.await.unwrap().unwrap().unwrap();
    }
    assert_eq!(f.executor.maximum.load(Ordering::SeqCst), 2);
    assert!(
        f.executor
            .calls
            .lock()
            .unwrap()
            .values()
            .all(|calls| *calls == 1)
    );
    assert_eq!(f.supervisor.queued_count().await, 0);
}

#[tokio::test]
async fn background_dispatcher_fills_two_pages_before_either_action_finishes() {
    let mut f = fixture(2, false, false).await;
    let worker = f.supervisor.start();
    let first = next_start(&mut f.started).await;
    let second = next_start(&mut f.started).await;
    assert_ne!(first, second);
    assert_eq!(f.executor.active.load(Ordering::SeqCst), 2);
    f.executor.release.add_permits(2);
    f.executor.prepare_release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), f.supervisor.prepare_new_chat(None))
        .await
        .unwrap()
        .unwrap();
    drop(worker);
    assert_eq!(f.executor.maximum.load(Ordering::SeqCst), 2);
    assert!(
        f.executor
            .calls
            .lock()
            .unwrap()
            .values()
            .all(|calls| *calls == 1)
    );
}

#[tokio::test]
async fn aliases_of_one_chat_are_serialized_without_blocking_the_queue_forever() {
    let mut f = fixture(2, true, false).await;
    let first = run(&f);
    let second = run(&f);
    next_start(&mut f.started).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(60), f.started.recv())
            .await
            .is_err()
    );
    f.executor.release.add_permits(1);
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    let last = run(&f);
    next_start(&mut f.started).await;
    f.executor.release.add_permits(1);
    last.await.unwrap().unwrap().unwrap();
    assert_eq!(f.executor.maximum.load(Ordering::SeqCst), 1);
    assert_eq!(f.supervisor.queued_count().await, 0);
}

#[tokio::test]
async fn dropping_one_runner_releases_capacity_without_cancelling_its_peer() {
    let mut f = fixture(3, false, false).await;
    let first = run(&f);
    next_start(&mut f.started).await;
    let second = run(&f);
    next_start(&mut f.started).await;
    first.abort();
    assert!(matches!(first.await,Err(error) if error.is_cancelled()));
    let third = run(&f);
    next_start(&mut f.started).await;
    assert_eq!(f.executor.active.load(Ordering::SeqCst), 2);
    f.executor.release.add_permits(2);
    second.await.unwrap().unwrap().unwrap();
    third.await.unwrap().unwrap().unwrap();
    assert_eq!(f.executor.maximum.load(Ordering::SeqCst), 2);
    assert_eq!(f.executor.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn exclusive_preparation_drains_both_lanes_before_new_dispatch() {
    let mut f = fixture(3, false, false).await;
    let first = run(&f);
    let second = run(&f);
    next_start(&mut f.started).await;
    next_start(&mut f.started).await;
    let prep = tokio::spawn({
        let s = f.supervisor.clone();
        async move { s.prepare_new_chat(None).await }
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(60), f.prepared.recv())
            .await
            .is_err()
    );
    let third = run(&f);
    f.executor.release.add_permits(1);
    assert!(
        tokio::time::timeout(Duration::from_millis(60), f.prepared.recv())
            .await
            .is_err(),
        "one active page still owns its lease"
    );
    f.executor.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), f.prepared.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(60), f.started.recv())
            .await
            .is_err(),
        "pending setup owns the exclusive write lease"
    );
    f.executor.prepare_release.add_permits(1);
    prep.await.unwrap().unwrap();
    next_start(&mut f.started).await;
    f.executor.release.add_permits(1);
    for action in [first, second, third] {
        action.await.unwrap().unwrap().unwrap();
    }
}

#[tokio::test]
async fn parallel_uncertain_accepted_sends_are_not_replayed() {
    let mut f = fixture(2, false, true).await;
    let first = run(&f);
    let second = run(&f);
    next_start(&mut f.started).await;
    next_start(&mut f.started).await;
    f.executor.release.add_permits(2);
    assert!(first.await.unwrap().is_err());
    assert!(second.await.unwrap().is_err());
    f.supervisor.scan_once(f.now + 1000).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    assert!(f.supervisor.run_next(f.now + 1001).await.unwrap().is_none());
    assert_eq!(f.executor.calls.lock().unwrap().len(), f.tasks.len());
    assert!(
        f.executor
            .calls
            .lock()
            .unwrap()
            .values()
            .all(|calls| *calls == 1)
    );
}
