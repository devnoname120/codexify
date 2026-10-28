use std::io::Write as _;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use codexify::agent_automation::{
    AcceptedChatBatch, ActionPrompt, AgentAutomationStore, AgentAutomationSupervisor,
    AutomationActionStatus, AutomationExecution, AutomationExecutionError, AutomationExecutor,
    AutomationTaskHandle, BrowserAutomationExecutor, CompanionActionSnapshot, CompanionActionState,
    CompanionEvent, CompanionEventKind, GoalFinish, GoalStatus, PairingCommit,
};
use codexify::config::default_config;
use codexify::exec_sessions::SessionState;
use codexify::markdown_chat::{AUTOMATION_OFFLINE_AFTER_MS, MarkdownChatStore};
use codexify::project_bindings::ConversationIdentity;
use tokio::sync::Notify;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const COMPANION_TOKEN: &str = "YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE";
const COMPANION_BUILD: &str = "supervisor-fake-build+protocol.2f7852fd999d639b18e829bc273b2e38021c40c839ec0feb0ff2c793cfeb9e03";

struct FakeExecutor {
    calls: AtomicUsize,
    executions: Mutex<Vec<AutomationExecution>>,
    events: Mutex<Vec<CompanionEventKind>>,
    retained: Mutex<Option<CompanionActionSnapshot>>,
    inspections: AtomicUsize,
}

impl Default for FakeExecutor {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            executions: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            retained: Mutex::new(None),
            inspections: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl AutomationExecutor for FakeExecutor {
    async fn retained_action(
        &self,
        _action_id: &str,
        _account: &str,
    ) -> Result<Option<CompanionActionSnapshot>, String> {
        self.inspections.fetch_add(1, Ordering::SeqCst);
        Ok(self.retained.lock().unwrap().clone())
    }
    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        _cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.executions.lock().unwrap().push(execution.clone());
        let configured = self.events.lock().unwrap().clone();
        for (index, kind) in configured.into_iter().enumerate() {
            events
                .send(CompanionEvent {
                    sequence: index as u64 + 1,
                    action_id: execution.request.action_id.clone(),
                    at_ms: index as u64 + 1,
                    kind,
                })
                .unwrap();
        }
        Ok(snapshot_for(execution))
    }
}

fn snapshot_for(execution: AutomationExecution) -> CompanionActionSnapshot {
    CompanionActionSnapshot {
        protocol_version: codexify::agent_automation::PROTOCOL_VERSION,
        build: "fake-build".into(),
        action_id: execution.request.action_id,
        state: CompanionActionState::Idle,
        send_accepted: true,
        canonical_url: Some(execution.request.expected_url),
        selected: execution
            .request
            .model
            .zip(execution.request.effort)
            .map(
                |(model, effort)| codexify::agent_automation::CompanionSelection { model, effort },
            ),
        last_event_sequence: 2,
        cooldown_until_ms: None,
        attention: None,
    }
}

struct ChatWritingExecutor {
    chat: Arc<codexify::markdown_chat::ChatFile>,
}

struct PairVerifyingExecutor {
    store: Arc<AgentAutomationStore>,
    task: ConversationIdentity,
    generation: u64,
    reference: String,
}

struct PairVerifyingFailureExecutor {
    store: Arc<AgentAutomationStore>,
    task: ConversationIdentity,
    generation: u64,
    reference: String,
}

struct AmbiguousExecutor;

struct PreSendFailureExecutor {
    calls: AtomicUsize,
}

struct StopFailureThenSuccessExecutor {
    calls: AtomicUsize,
    snapshot_failure: bool,
}

#[async_trait]
impl AutomationExecutor for StopFailureThenSuccessExecutor {
    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        _cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let message =
                "ChatGPT did not stop the previous response; the new message was not sent";
            if self.snapshot_failure {
                let mut snapshot = snapshot_for(execution);
                snapshot.state = CompanionActionState::Failed;
                snapshot.send_accepted = false;
                snapshot.attention = Some(codexify::agent_automation::CompanionAttention {
                    code: "selector_drift".into(),
                    message: Some(message.into()),
                });
                return Ok(snapshot);
            }
            return Err(AutomationExecutionError {
                message: message.into(),
                send_accepted: false,
                failure_status: None,
            });
        }
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
                    at_ms: index as u64 + 1,
                    kind,
                })
                .unwrap();
        }
        Ok(snapshot_for(execution))
    }
}

struct AcceptedTerminalFailureExecutor;

struct AcceptedEventThenTransportErrorExecutor;

impl Default for PreSendFailureExecutor {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl AutomationExecutor for AmbiguousExecutor {
    async fn execute(
        &self,
        _execution: AutomationExecution,
        _events: mpsc::UnboundedSender<CompanionEvent>,
        _cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        Err(AutomationExecutionError {
            message: "Send may have been accepted".into(),
            send_accepted: true,
            failure_status: None,
        })
    }
}

#[async_trait]
impl AutomationExecutor for PreSendFailureExecutor {
    async fn execute(
        &self,
        _execution: AutomationExecution,
        _events: mpsc::UnboundedSender<CompanionEvent>,
        _cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(AutomationExecutionError {
            message: "Selector verification failed before Send".into(),
            send_accepted: false,
            failure_status: None,
        })
    }
}

#[async_trait]
impl AutomationExecutor for AcceptedTerminalFailureExecutor {
    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        _cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        events
            .send(CompanionEvent {
                sequence: 1,
                action_id: execution.request.action_id.clone(),
                at_ms: 1,
                kind: CompanionEventKind::SendAccepted,
            })
            .unwrap();
        events
            .send(CompanionEvent {
                sequence: 2,
                action_id: execution.request.action_id.clone(),
                at_ms: 2,
                kind: CompanionEventKind::Failed {
                    code: "response_failed".into(),
                    message: "ChatGPT failed after accepting the message".into(),
                },
            })
            .unwrap();
        let mut snapshot = snapshot_for(execution);
        snapshot.state = CompanionActionState::Failed;
        Ok(snapshot)
    }
}

#[async_trait]
impl AutomationExecutor for AcceptedEventThenTransportErrorExecutor {
    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        _cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        events
            .send(CompanionEvent {
                sequence: 1,
                action_id: execution.request.action_id,
                at_ms: 1,
                kind: CompanionEventKind::SendAccepted,
            })
            .unwrap();
        Err(AutomationExecutionError {
            message: "Browser companion status reconciliation failed".into(),
            send_accepted: false,
            failure_status: None,
        })
    }
}

struct StreamingExecutor {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

struct LargeTransientExecutor;

struct LargeFinalAnswerExecutor;

struct SerialExecutor {
    calls: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    started: mpsc::UnboundedSender<usize>,
    release: Arc<Semaphore>,
}

#[async_trait]
impl AutomationExecutor for SerialExecutor {
    async fn execute(
        &self,
        execution: AutomationExecution,
        _events: mpsc::UnboundedSender<CompanionEvent>,
        _cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        self.started.send(call).unwrap();
        let permit = self.release.acquire().await.unwrap();
        permit.forget();
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(snapshot_for(execution))
    }
}

#[async_trait]
impl AutomationExecutor for StreamingExecutor {
    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        _cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        for (sequence, kind) in [
            CompanionEventKind::StatusDelta {
                text: "Using the browser".into(),
                continuation: false,
            },
            CompanionEventKind::CommentaryDelta {
                markdown: "Visible commentary".into(),
                continuation: false,
            },
            CompanionEventKind::AnswerDelta {
                markdown: "Partial answer".into(),
                continuation: false,
            },
        ]
        .into_iter()
        .enumerate()
        {
            events
                .send(CompanionEvent {
                    sequence: sequence as u64 + 1,
                    action_id: execution.request.action_id.clone(),
                    at_ms: sequence as u64 + 1,
                    kind,
                })
                .unwrap();
        }
        self.started.notify_one();
        self.release.notified().await;
        events
            .send(CompanionEvent {
                sequence: 4,
                action_id: execution.request.action_id.clone(),
                at_ms: 4,
                kind: CompanionEventKind::GenerationComplete {
                    final_markdown: Some("Complete answer".into()),
                    canonical_url: Some(execution.request.expected_url.clone()),
                },
            })
            .unwrap();
        Ok(snapshot_for(execution))
    }
}

#[async_trait]
impl AutomationExecutor for LargeTransientExecutor {
    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        _cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        events
            .send(CompanionEvent {
                sequence: 1,
                action_id: execution.request.action_id.clone(),
                at_ms: 1,
                kind: CompanionEventKind::SendAccepted,
            })
            .unwrap();
        for index in 0..17_u64 {
            events
                .send(CompanionEvent {
                    sequence: index + 2,
                    action_id: execution.request.action_id.clone(),
                    at_ms: index + 2,
                    kind: CompanionEventKind::StatusDelta {
                        text: "x".repeat(64 * 1024),
                        continuation: index != 0,
                    },
                })
                .unwrap();
        }
        events
            .send(CompanionEvent {
                sequence: 19,
                action_id: execution.request.action_id.clone(),
                at_ms: 19,
                kind: CompanionEventKind::GenerationComplete {
                    final_markdown: Some("Complete after long transient output.".into()),
                    canonical_url: Some(execution.request.expected_url.clone()),
                },
            })
            .unwrap();
        Ok(snapshot_for(execution))
    }
}

#[async_trait]
impl AutomationExecutor for LargeFinalAnswerExecutor {
    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        _cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        events
            .send(CompanionEvent {
                sequence: 1,
                action_id: execution.request.action_id.clone(),
                at_ms: 1,
                kind: CompanionEventKind::SendAccepted,
            })
            .unwrap();
        for (index, markdown) in ["a".repeat(40 * 1024), "b".repeat(40 * 1024)]
            .into_iter()
            .enumerate()
        {
            events
                .send(CompanionEvent {
                    sequence: index as u64 + 2,
                    action_id: execution.request.action_id.clone(),
                    at_ms: index as u64 + 2,
                    kind: CompanionEventKind::AnswerDelta {
                        markdown,
                        continuation: index != 0,
                    },
                })
                .unwrap();
        }
        events
            .send(CompanionEvent {
                sequence: 4,
                action_id: execution.request.action_id.clone(),
                at_ms: 4,
                kind: CompanionEventKind::GenerationComplete {
                    final_markdown: None,
                    canonical_url: Some(execution.request.expected_url.clone()),
                },
            })
            .unwrap();
        Ok(snapshot_for(execution))
    }
}

#[async_trait]
impl AutomationExecutor for ChatWritingExecutor {
    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        _cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        self.chat
            .append("Authoritative output from chat_write.".into())
            .await
            .unwrap();
        events
            .send(CompanionEvent {
                sequence: 1,
                action_id: execution.request.action_id.clone(),
                at_ms: 1,
                kind: CompanionEventKind::SendAccepted,
            })
            .unwrap();
        events
            .send(CompanionEvent {
                sequence: 2,
                action_id: execution.request.action_id.clone(),
                at_ms: 2,
                kind: CompanionEventKind::GenerationComplete {
                    final_markdown: Some("Duplicate browser final answer.".into()),
                    canonical_url: Some(execution.request.expected_url.clone()),
                },
            })
            .unwrap();
        Ok(snapshot_for(execution))
    }
}

#[async_trait]
impl AutomationExecutor for PairVerifyingExecutor {
    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        _cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        self.store
            .record_pairing_canonical_url(
                &self.task,
                self.generation,
                &execution.request.expected_url,
                102,
            )
            .unwrap();
        self.store
            .verify_pairing(
                &self.task,
                self.generation,
                execution.owner_generation,
                &self.reference,
                103,
            )
            .unwrap();
        events
            .send(CompanionEvent {
                sequence: 1,
                action_id: execution.request.action_id.clone(),
                at_ms: 1,
                kind: CompanionEventKind::GenerationComplete {
                    final_markdown: None,
                    canonical_url: Some(execution.request.expected_url.clone()),
                },
            })
            .unwrap();
        Ok(snapshot_for(execution))
    }
}

#[async_trait]
impl AutomationExecutor for PairVerifyingFailureExecutor {
    async fn execute(
        &self,
        execution: AutomationExecution,
        _events: mpsc::UnboundedSender<CompanionEvent>,
        _cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        self.store
            .record_pairing_canonical_url(
                &self.task,
                self.generation,
                &execution.request.expected_url,
                102,
            )
            .unwrap();
        self.store
            .verify_pairing(
                &self.task,
                self.generation,
                execution.owner_generation,
                &self.reference,
                103,
            )
            .unwrap();
        Err(AutomationExecutionError {
            message: "Browser companion returned a different conversation URL".into(),
            send_accepted: true,
            failure_status: None,
        })
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    config: codexify::types::AppConfig,
    task: ConversationIdentity,
    chat: Arc<codexify::markdown_chat::ChatFile>,
    store: Arc<AgentAutomationStore>,
    supervisor: Arc<AgentAutomationSupervisor>,
    executor: Arc<FakeExecutor>,
}

struct ApprovalExecutor {
    inner: Arc<FakeExecutor>,
    pending: AtomicBool,
    checks: Mutex<Vec<(String, String, String)>>,
}

#[tokio::test]
async fn conversation_approval_defers_an_already_queued_send_without_fencing_its_messages() {
    let f = fixture().await;
    f.chat
        .append_user(
            "queued-for-approval".into(),
            "Do not acknowledge this just by approving".into(),
        )
        .await
        .unwrap();
    let executor = Arc::new(ApprovalExecutor {
        inner: f.executor.clone(),
        pending: AtomicBool::new(false),
        checks: Mutex::new(Vec::new()),
    });
    let supervisor =
        AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor.clone());
    supervisor
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    supervisor.scan_once(1000).await.unwrap();
    assert_eq!(supervisor.queued_count().await, 1);
    let before = f.store.load(&f.task).unwrap();
    executor.pending.store(true, Ordering::SeqCst);
    supervisor.scan_once(6000).await.unwrap();
    assert_eq!(supervisor.queued_count().await, 0);
    let after = f.store.load(&f.task).unwrap();
    assert_eq!(after.action.status, AutomationActionStatus::Idle);
    assert_eq!(after.chat_mirror, before.chat_mirror);
    assert!(!after.action.messages_fenced);
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 0);
    assert!(
        f.chat
            .read(false)
            .await
            .unwrap()
            .text
            .contains("Do not acknowledge this just by approving")
    );
}

#[tokio::test]
async fn conversation_approval_preserves_previously_accepted_message_fences() {
    let f = fixture().await;
    f.chat
        .append_user(
            "accepted-before-approval".into(),
            "Previously submitted".into(),
        )
        .await
        .unwrap();
    f.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::SubmissionComplete {
            canonical_url: "https://chatgpt.com/c/example".into(),
        },
    ]);
    f.supervisor.scan_once(1000).await.unwrap();
    f.supervisor.run_next(1001).await.unwrap();
    let accepted = f.store.load(&f.task).unwrap();
    assert!(accepted.action.messages_fenced);
    let executor = Arc::new(ApprovalExecutor {
        inner: f.executor.clone(),
        pending: AtomicBool::new(true),
        checks: Mutex::new(Vec::new()),
    });
    let supervisor =
        AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor);
    supervisor
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    supervisor.scan_once(6000).await.unwrap();
    let after = f.store.load(&f.task).unwrap();
    assert_eq!(after.action, accepted.action);
    assert_eq!(after.chat_mirror, accepted.chat_mirror);
    assert_eq!(supervisor.queued_count().await, 0);
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 1);
}

#[async_trait]
impl AutomationExecutor for ApprovalExecutor {
    async fn approve_pending_conversation(
        &self,
        url: &str,
        account: &str,
        connector: &str,
    ) -> Result<bool, String> {
        self.checks
            .lock()
            .unwrap()
            .push((url.into(), account.into(), connector.into()));
        Ok(self.pending.swap(false, Ordering::SeqCst))
    }
    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        self.inner.execute(execution, events, cancellation).await
    }
}

#[tokio::test]
async fn conversation_approval_resumes_the_existing_turn_without_sending_or_consuming_queued_messages()
 {
    let f = fixture().await;
    f.chat
        .append_user(
            "approval-pending-message".into(),
            "Keep my message unread".into(),
        )
        .await
        .unwrap();
    let goal = f
        .store
        .set_goal(&f.task, "Continue the current goal", 100)
        .unwrap();
    f.store.run_goal(&f.task, goal.revision, 101).unwrap();
    let executor = Arc::new(ApprovalExecutor {
        inner: f.executor.clone(),
        pending: AtomicBool::new(true),
        checks: Mutex::new(Vec::new()),
    });
    let supervisor =
        AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor.clone());
    supervisor
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    let before = f.store.load(&f.task).unwrap();
    supervisor.scan_once(1000).await.unwrap();
    supervisor.scan_once(1001).await.unwrap();
    assert_eq!(supervisor.queued_count().await, 0);
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        executor.checks.lock().unwrap().as_slice(),
        &[(
            "https://chatgpt.com/c/example".into(),
            "account-1".into(),
            "Codexify".into()
        )]
    );
    let after = f.store.load(&f.task).unwrap();
    assert_eq!(after.goal, before.goal);
    assert_eq!(after.chat_mirror, before.chat_mirror);
    assert_eq!(after.action.send_accepted, before.action.send_accepted);
    assert!(
        f.chat
            .read(false)
            .await
            .unwrap()
            .text
            .contains("Keep my message unread")
    );
    supervisor.scan_once(6001).await.unwrap();
    assert_eq!(supervisor.queued_count().await, 0);
    let until = f
        .store
        .load(&f.task)
        .unwrap()
        .approval_resume_until_ms()
        .unwrap();
    assert!(until >= 181_000);
    supervisor.scan_once(until - 1).await.unwrap();
    assert_eq!(supervisor.queued_count().await, 0);
    supervisor.scan_once(until).await.unwrap();
    assert_eq!(supervisor.queued_count().await, 1);
}

#[tokio::test]
async fn conversation_approval_honors_disabled_auto_approval_stopped_agents_and_unverified_pairings()
 {
    for mode in ["disabled", "stopped", "unverified"] {
        let f = fixture().await;
        if mode == "stopped" {
            f.chat
                .request_agent_stop("stop-approval".into())
                .await
                .unwrap();
        }
        if mode == "unverified" {
            f.store.unlink_pairing(&f.task, 103).unwrap();
        }
        let mut config = f.config.markdown_chat.automation.clone();
        config.auto_approve_tool_calls = mode != "disabled";
        let executor = Arc::new(ApprovalExecutor {
            inner: f.executor.clone(),
            pending: AtomicBool::new(true),
            checks: Mutex::new(Vec::new()),
        });
        let supervisor = AgentAutomationSupervisor::new(config, executor.clone());
        supervisor
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                f.store.clone(),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        supervisor.scan_once(1000).await.unwrap();
        assert!(executor.checks.lock().unwrap().is_empty(), "{mode}");
        assert_eq!(f.executor.calls.load(Ordering::SeqCst), 0);
    }
}

async fn fixture() -> Fixture {
    fixture_with_queue_limit(64).await
}

#[tokio::test]
async fn queued_chat_run_now_prioritizes_one_unsent_request_without_creating_a_goal_or_replaying_it()
 {
    let f = fixture().await;
    let (other, chat) = add_task(&f, "manual-now-other", "https://chatgpt.com/c/other").await;
    f.chat
        .append_user("manual-first".into(), "First pending request".into())
        .await
        .unwrap();
    chat.append_user("manual-second".into(), "Second pending request".into())
        .await
        .unwrap();
    f.supervisor.scan_once(1000).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 2);
    let (target, remaining) = if f.supervisor.queue_position(&f.task).await.unwrap().0 == 2 {
        (&f.task, &other)
    } else {
        (&other, &f.task)
    };
    let initial = f.store.load(target).unwrap();
    let id = initial.action.id.clone().unwrap();
    assert!(initial.goal.is_none());
    assert!(initial.can_run_queued_now());
    assert!(
        f.supervisor
            .run_queued_action_now(target, "outdated-action", 1001)
            .await
            .is_err()
    );
    let action = f
        .supervisor
        .run_queued_action_now(target, &id, 1001)
        .await
        .unwrap();
    assert_eq!(action.id.as_deref(), Some(id.as_str()));
    assert!(action.bypass_conversation_cooldown);
    assert_eq!(f.supervisor.queue_position(target).await, Some((1, 2)));
    f.supervisor.run_next(1001).await.unwrap().unwrap();
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 1);
    {
        let requests = f.executor.executions.lock().unwrap();
        assert_eq!(
            requests[0].request.url,
            f.store.load(target).unwrap().pairing.canonical_url.unwrap()
        );
        assert!(requests[0].request.bypass_conversation_cooldown);
        assert_eq!(requests[0].request.action_id, id);
    }
    assert!(f.store.load(target).unwrap().goal.is_none());
    assert!(
        f.supervisor
            .run_queued_action_now(target, &id, 1002)
            .await
            .is_err()
    );
    let remaining_id = f.store.load(remaining).unwrap().action.id.unwrap();
    f.supervisor
        .cancel_action(remaining, &remaining_id, 1003)
        .await
        .unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    assert!(f.supervisor.run_next(1003).await.unwrap().is_none());
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 1);
    assert!(!chat.unread_snapshot().await.unwrap().messages.is_empty());
}

async fn fixture_with_queue_limit(max_queued_actions: usize) -> Fixture {
    fixture_with_offline_timeout(max_queued_actions, 5).await
}

async fn fixture_with_offline_timeout(
    max_queued_actions: usize,
    offline_timeout_minutes: u64,
) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mut config = default_config(project);
    config.memory.dir = Some(root.path().join("state").display().to_string());
    config.markdown_chat.enabled = true;
    config.markdown_chat.offline_timeout_minutes = offline_timeout_minutes;
    config.markdown_chat.automation.enabled = true;
    config.markdown_chat.automation.connector_name = "Codexify".into();
    config.markdown_chat.automation.max_queued_actions = max_queued_actions;
    config.markdown_chat.automation.descriptor_path = root
        .path()
        .join("browser/descriptor.json")
        .display()
        .to_string();

    let task = ConversationIdentity::from_openai_session("supervisor-online").unwrap();
    let chat = MarkdownChatStore::default()
        .chat(&config, Some(&task), &SessionState::new())
        .unwrap();
    chat.ensure().await.unwrap();
    let store = Arc::new(AgentAutomationStore::for_config(&config).unwrap());
    store
        .set_selection(&task, "gpt-6-sol", "high", "catalog-1", "account-1", 100)
        .unwrap();
    let pairing = store
        .issue_pairing(&task, "https://chatgpt.com/c/example", 0, 100)
        .unwrap();
    store
        .record_pairing_canonical_url(
            &task,
            pairing.generation,
            "https://chatgpt.com/c/example",
            101,
        )
        .unwrap();
    store
        .verify_pairing(&task, pairing.generation, 0, &pairing.reference, 102)
        .unwrap();

    let executor = Arc::new(FakeExecutor::default());
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

async fn add_task(
    fixture: &Fixture,
    name: &str,
    url: &str,
) -> (ConversationIdentity, Arc<codexify::markdown_chat::ChatFile>) {
    let task = ConversationIdentity::from_openai_session(name).unwrap();
    let chat = MarkdownChatStore::default()
        .chat(&fixture.config, Some(&task), &SessionState::new())
        .unwrap();
    chat.ensure().await.unwrap();
    fixture
        .store
        .set_selection(&task, "gpt-6-sol", "high", "catalog-1", "account-1", 100)
        .unwrap();
    let pairing = fixture.store.issue_pairing(&task, url, 0, 100).unwrap();
    fixture
        .store
        .record_pairing_canonical_url(&task, pairing.generation, url, 101)
        .unwrap();
    fixture
        .store
        .verify_pairing(&task, pairing.generation, 0, &pairing.reference, 102)
        .unwrap();
    fixture
        .supervisor
        .register_task(AutomationTaskHandle::new(
            task.clone(),
            0,
            fixture.store.clone(),
            chat.clone(),
        ))
        .await
        .unwrap();
    (task, chat)
}

fn write_companion_descriptor(root: &std::path::Path, endpoint: &str) -> std::path::PathBuf {
    let directory = root.join("browser");
    std::fs::create_dir_all(&directory).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = directory.join("descriptor.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "version":1,
            "kind":"codexify-browser-companion",
            "pid":std::process::id(),
            "endpoint":endpoint,
            "token":COMPANION_TOKEN,
            "build":COMPANION_BUILD,
            "createdAt":chrono::Utc::now().to_rfc3339()
        })
        .to_string(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(&format!("Bearer {COMPANION_TOKEN}"))
}

#[tokio::test]
async fn unread_messages_wait_until_the_same_five_minute_threshold_as_public_presence() {
    assert_eq!(codexify::markdown_chat::AWAY_AFTER_MS, 180_000);
    assert_eq!(AUTOMATION_OFFLINE_AFTER_MS, 300_000);
    assert_eq!(codexify::markdown_chat::OFFLINE_AFTER_MS, 300_000);
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user("online-message".into(), "Do not automate this yet.".into())
        .await
        .unwrap();

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS - 1)
        .await
        .unwrap();

    assert_eq!(fixture.supervisor.queued_count().await, 0);
    assert_eq!(fixture.executor.calls.load(Ordering::SeqCst), 0);
    assert!(
        fixture
            .store
            .load(&fixture.task)
            .unwrap()
            .chat_mirror
            .message_ids
            .is_empty()
    );
    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(fixture.supervisor.queued_count().await, 1);
    assert_eq!(fixture.executor.calls.load(Ordering::SeqCst), 0);
}

fn assert_message_body(markdown: &str, body: &str) {
    let (time, text) = markdown
        .split_once('\n')
        .expect("timestamp precedes the full message");
    let time = time
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap();
    chrono::DateTime::parse_from_rfc3339(time).expect("original message time is readable");
    assert_eq!(text, body);
}

#[tokio::test]
async fn offline_attachment_prompts_include_metadata_and_tool_reminders_once() {
    let f = fixture().await;
    f.chat.record_agent_call(1000).await.unwrap();
    let store =
        codexify::markdown_chat::attachments::AttachmentStore::for_chat(f.chat.path()).unwrap();
    let file = store
        .upload(
            "offline-file",
            "analysis.txt",
            "text/plain",
            Some(1000),
            false,
            b"Attachment content".to_vec(),
        )
        .unwrap();
    f.chat
        .append_user_with_attachments(
            "attachment-prompt".into(),
            "Analyze the attachment".into(),
            vec![file.id.clone()],
        )
        .await
        .unwrap();
    f.supervisor.scan_once(1001).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    f.supervisor
        .scan_once(1000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    f.supervisor
        .run_next(1001 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap()
        .unwrap();
    {
        let executions = f.executor.executions.lock().unwrap();
        let ActionPrompt::OfflineChat { messages, .. } = &executions[0].request.prompt else {
            panic!("attachment prompt must wake the conversation")
        };
        for text in [
            "Analyze the attachment",
            file.path.as_str(),
            file.unique_name.as_str(),
            "analysis.txt",
            "createdAt",
            "lastModifiedAt",
            "submittedAt",
            "chat_list_attachments",
            "chat_search_attachments",
            "chat_get_attachment",
        ] {
            assert!(
                messages[0].markdown.contains(text),
                "offline prompts must mention {text}"
            );
        }
    }
    f.supervisor
        .scan_once(1002 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.chat.unread_snapshot().await.unwrap().messages.len(),
        1,
        "browser acceptance does not replace the agent's acknowledgement"
    );
}

#[tokio::test]
async fn an_online_message_left_unread_at_turn_end_is_relayed_once_after_inactivity() {
    // Preparing an opportunistic tool response is not a read acknowledgement.
    for response_prepared in [false, true] {
        let f = fixture().await;
        f.chat.record_agent_call(1_000).await.unwrap();
        let sent = f
            .chat
            .append_user(
                "last-tool-gap".into(),
                "This arrived after the final tool call.".into(),
            )
            .await
            .unwrap();
        if response_prepared {
            f.chat.mark_delivered(sent.end).await.unwrap();
        }
        f.supervisor.scan_once(1_001).await.unwrap();
        assert_eq!(f.supervisor.queued_count().await, 0);
        assert_eq!(f.executor.calls.load(Ordering::SeqCst), 0);
        assert_eq!(f.chat.unread_snapshot().await.unwrap().messages.len(), 1);

        let offline_at = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
        f.supervisor.scan_once(offline_at - 1).await.unwrap();
        assert_eq!(f.supervisor.queued_count().await, 0);
        f.supervisor.scan_once(offline_at).await.unwrap();
        assert_eq!(f.supervisor.queued_count().await, 1);
        f.supervisor
            .run_next(offline_at + 1)
            .await
            .unwrap()
            .unwrap();
        {
            let executions = f.executor.executions.lock().unwrap();
            assert_eq!(executions.len(), 1);
            match &executions[0].request.prompt {
                ActionPrompt::OfflineChat { messages, .. } => {
                    assert_eq!(messages.len(), 1);
                    assert_eq!(messages[0].id, "last-tool-gap");
                    assert_message_body(
                        &messages[0].markdown,
                        "This arrived after the final tool call.",
                    );
                }
                _ => panic!("a pending online message must wake its chat without requiring a goal"),
            }
        }
        assert_eq!(
            f.store.load(&f.task).unwrap().chat_mirror.message_ids,
            vec!["last-tool-gap"]
        );
        assert_eq!(f.chat.unread_snapshot().await.unwrap().messages.len(), 1);

        let reopened_store = Arc::new(AgentAutomationStore::for_config(&f.config).unwrap());
        let restarted = AgentAutomationSupervisor::new(
            f.config.markdown_chat.automation.clone(),
            f.executor.clone(),
        );
        restarted
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                reopened_store,
                f.chat.clone(),
            ))
            .await
            .unwrap();
        restarted.scan_once(offline_at + 2).await.unwrap();
        assert_eq!(restarted.queued_count().await, 0);
        // Another burst of agent activity must not resend the still-unread copy.
        f.chat.record_agent_call(500_000).await.unwrap();
        restarted
            .scan_once(500_000 + AUTOMATION_OFFLINE_AFTER_MS)
            .await
            .unwrap();
        assert_eq!(restarted.queued_count().await, 0);
        assert_eq!(f.executor.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn an_online_message_read_before_inactivity_is_not_relayed() {
    let f = fixture().await;
    f.chat.record_agent_call(1_000).await.unwrap();
    f.chat
        .append_user(
            "tool-read-message".into(),
            "Read this in the ongoing turn.".into(),
        )
        .await
        .unwrap();
    f.supervisor.scan_once(1_001).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    f.chat.record_agent_call(2_000).await.unwrap();
    assert!(
        f.chat
            .read(true)
            .await
            .unwrap()
            .text
            .contains("Read this in the ongoing turn.")
    );
    f.supervisor
        .scan_once(2_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_online_message_read_after_queueing_is_not_sent_to_chatgpt() {
    let f = fixture().await;
    f.chat.record_agent_call(1_000).await.unwrap();
    f.chat
        .append_user(
            "queue-read-race".into(),
            "Read this before the browser starts.".into(),
        )
        .await
        .unwrap();
    let offline_at = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
    f.supervisor.scan_once(offline_at).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 1);
    f.chat.read(true).await.unwrap();
    assert!(
        f.supervisor
            .run_next(offline_at + 1)
            .await
            .unwrap()
            .is_none()
    );
    f.supervisor.scan_once(offline_at + 2).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_blocked_offline_goal_releases_and_sends_one_queued_message_without_steering() {
    use codexify::agent_automation::{AutomationActionKind, OwnerGoalCommand};
    let f = fixture().await;
    let goal = f
        .store
        .apply_owner_goal_command(
            &f.task,
            "blocked-queue-goal",
            OwnerGoalCommand::Set {
                markdown: "Continue the project".into(),
            },
            1000,
        )
        .unwrap()
        .goal
        .unwrap();
    f.chat
        .queue_user_message(
            "older".into(),
            "An older task that must remain private".into(),
            vec![],
        )
        .await
        .unwrap();
    f.store
        .finish_goal(
            &f.task,
            goal.revision,
            GoalFinish::Blocked {
                reason: "Waiting for new user instructions".into(),
            },
            1001,
        )
        .unwrap();
    f.chat
        .queue_user_message(
            "first".into(),
            "New instructions for the blocked goal".into(),
            vec![],
        )
        .await
        .unwrap();
    f.chat
        .queue_user_message("second".into(), "A separate later task".into(), vec![])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if f.chat.queued_messages().await.unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        f.chat
            .queued_messages()
            .await
            .unwrap()
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["older", "second"]
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    f.supervisor.scan_once(now).await.unwrap();
    assert_eq!(
        f.supervisor.next_queued_kind().await,
        Some(AutomationActionKind::OfflineChat)
    );
    f.supervisor.run_next(now + 1).await.unwrap().unwrap();
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 1);
    let executions = f.executor.executions.lock().unwrap().clone();
    let ActionPrompt::OfflineChat { messages, .. } = &executions[0].request.prompt else {
        panic!("expected a chat message wakeup")
    };
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, "first");
    assert!(!messages.iter().any(|m| {
        m.markdown
            .contains("An older task that must remain private")
    }));
    assert_eq!(
        f.store.load(&f.task).unwrap().goal.unwrap().status,
        GoalStatus::Blocked
    );
    f.supervisor.scan_once(now + 2).await.unwrap();
    assert_eq!(
        f.supervisor.queued_count().await,
        0,
        "accepted delivery must not be replayed"
    );
    assert_eq!(
        f.chat
            .queued_messages()
            .await
            .unwrap()
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["older", "second"]
    );
}

#[tokio::test]
async fn offline_unread_messages_enqueue_without_a_goal_and_remain_unread() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "offline-message".into(),
            "Please continue from the standalone chat.".into(),
        )
        .await
        .unwrap();

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();

    assert_eq!(fixture.supervisor.queued_count().await, 1);
    assert_eq!(fixture.executor.calls.load(Ordering::SeqCst), 0);
    let state = fixture.store.load(&fixture.task).unwrap();
    assert_eq!(
        state.action.kind,
        Some(codexify::agent_automation::AutomationActionKind::OfflineChat)
    );
    assert_eq!(
        state.action.status,
        codexify::agent_automation::AutomationActionStatus::Queued
    );
    assert!(
        fixture
            .chat
            .read(false)
            .await
            .unwrap()
            .text
            .contains("Please continue from the standalone chat.")
    );
}

#[tokio::test]
async fn goal_only_action_waits_before_retry_when_no_tool_activity() {
    let fixture = fixture().await;
    let goal = fixture
        .store
        .set_goal(&fixture.task, "Finish the current implementation.", 100)
        .unwrap();
    fixture
        .store
        .run_goal(&fixture.task, goal.revision, 101)
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::GenerationComplete {
            final_markdown: None,
            canonical_url: Some("https://chatgpt.com/c/example".into()),
        },
    ]);

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    fixture
        .supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap()
        .unwrap();

    {
        let executions = fixture.executor.executions.lock().unwrap();
        assert_eq!(executions.len(), 1);
        assert!(matches!(
            executions[0].request.prompt,
            ActionPrompt::OfflineGoal { goal_revision } if goal_revision == goal.revision
        ));
    }
    assert_eq!(fixture.supervisor.queued_count().await, 0);
    assert_eq!(
        fixture.store.load(&fixture.task).unwrap().action.status,
        AutomationActionStatus::Stalled
    );

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 2)
        .await
        .unwrap();
    assert_eq!(fixture.supervisor.queued_count().await, 0);
}

#[tokio::test]
async fn accepted_unread_batch_is_not_acknowledged_or_automatically_mirrored_twice() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "dedup-message".into(),
            "Mirror me once, but leave me unread.".into(),
        )
        .await
        .unwrap();
    fixture.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::GenerationComplete {
            final_markdown: None,
            canonical_url: Some("https://chatgpt.com/c/example".into()),
        },
    ]);

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    fixture
        .supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap()
        .unwrap();

    let state = fixture.store.load(&fixture.task).unwrap();
    assert_eq!(state.chat_mirror.message_ids, vec!["dedup-message"]);
    assert!(state.chat_mirror.last_accepted_batch_id.is_some());
    assert!(
        fixture
            .chat
            .read(false)
            .await
            .unwrap()
            .text
            .contains("Mirror me once, but leave me unread.")
    );

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 2)
        .await
        .unwrap();
    assert_eq!(fixture.supervisor.queued_count().await, 0);
    assert_eq!(fixture.executor.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn accepted_unread_batch_is_rearmed_for_a_new_physical_owner_generation() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "owner-generation-message".into(),
            "Mirror me again after the physical owner changes.".into(),
        )
        .await
        .unwrap();
    fixture.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::GenerationComplete {
            final_markdown: None,
            canonical_url: Some("https://chatgpt.com/c/example".into()),
        },
    ]);
    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    fixture
        .supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fixture.supervisor.queued_count().await, 0);

    fixture
        .supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            1,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    let replacement = fixture
        .store
        .issue_pairing(
            &fixture.task,
            "https://chatgpt.com/c/replacement-owner",
            1,
            2_000,
        )
        .unwrap();
    fixture
        .store
        .record_pairing_canonical_url(
            &fixture.task,
            replacement.generation,
            "https://chatgpt.com/c/replacement-owner",
            2_001,
        )
        .unwrap();
    fixture
        .store
        .verify_pairing(
            &fixture.task,
            replacement.generation,
            1,
            &replacement.reference,
            2_002,
        )
        .unwrap();

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 2_000)
        .await
        .unwrap();

    assert_eq!(fixture.supervisor.queued_count().await, 1);
}

#[tokio::test]
async fn consumed_accepted_message_ids_are_pruned_before_the_history_limit_blocks_delivery() {
    let fixture = fixture().await;
    let pairing_generation = fixture
        .store
        .load(&fixture.task)
        .unwrap()
        .pairing
        .generation;
    let stale_ids = (0..1024)
        .map(|index| format!("consumed-{index:04}"))
        .collect::<Vec<_>>();
    fixture
        .store
        .queue_action(
            &fixture.task,
            "consumed-history-action",
            codexify::agent_automation::AutomationActionKind::OfflineChat,
            pairing_generation,
            None,
            900,
        )
        .unwrap();
    fixture
        .store
        .record_accepted_batch(
            &fixture.task,
            "consumed-history-action",
            AcceptedChatBatch {
                owner_generation: 0,
                batch_id: "consumed-history-batch",
                message_ids: &stale_ids,
                transcript_revision: "consumed-history-revision",
            },
            901,
        )
        .unwrap();
    fixture
        .store
        .update_action_status(
            &fixture.task,
            "consumed-history-action",
            AutomationActionStatus::Idle,
            None,
            None,
            902,
        )
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "new-after-consumed-history".into(),
            "This unread message must still reach the replacement browser turn.".into(),
        )
        .await
        .unwrap();
    fixture.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::GenerationComplete {
            final_markdown: None,
            canonical_url: Some("https://chatgpt.com/c/example".into()),
        },
    ]);

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    fixture
        .supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        fixture
            .store
            .load(&fixture.task)
            .unwrap()
            .chat_mirror
            .message_ids,
        vec!["new-after-consumed-history"]
    );
}

#[tokio::test]
async fn full_unread_accepted_history_blocks_a_new_send_before_browser_execution() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    let accepted_ids = (0..1024)
        .map(|index| format!("retained-{index:04}"))
        .collect::<Vec<_>>();
    let mut blocks = String::new();
    for (index, id) in accepted_ids.iter().enumerate() {
        blocks.push_str(&format!(
            "\n\n<!-- codexify-user-message:v1:start id=\"{id}\" created_at_ms=\"{}\" tool_call_count=\"0\" -->\n\n## User\n\nPending accepted message {index}.\n\n<!-- codexify-user-message:v1:end id=\"{id}\" -->\n",
            1_001 + index as u64,
        ));
    }
    std::fs::OpenOptions::new()
        .append(true)
        .open(fixture.chat.path())
        .unwrap()
        .write_all(blocks.as_bytes())
        .unwrap();
    assert_eq!(
        fixture.chat.unread_snapshot().await.unwrap().messages.len(),
        1024
    );

    let pairing_generation = fixture
        .store
        .load(&fixture.task)
        .unwrap()
        .pairing
        .generation;
    fixture
        .store
        .queue_action(
            &fixture.task,
            "full-history-action",
            codexify::agent_automation::AutomationActionKind::OfflineChat,
            pairing_generation,
            None,
            2_100,
        )
        .unwrap();
    fixture
        .store
        .record_accepted_batch(
            &fixture.task,
            "full-history-action",
            AcceptedChatBatch {
                owner_generation: 0,
                batch_id: "full-history-batch",
                message_ids: &accepted_ids,
                transcript_revision: "full-history-revision",
            },
            2_101,
        )
        .unwrap();
    fixture
        .store
        .update_action_status(
            &fixture.task,
            "full-history-action",
            AutomationActionStatus::Idle,
            None,
            None,
            2_102,
        )
        .unwrap();
    fixture
        .chat
        .append_user(
            "new-after-full-history".into(),
            "This message must remain unread until a mirror receipt can be persisted.".into(),
        )
        .await
        .unwrap();

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();

    assert_eq!(fixture.supervisor.queued_count().await, 0);
    assert_eq!(fixture.executor.calls.load(Ordering::SeqCst), 0);
    let state = fixture.store.load(&fixture.task).unwrap();
    assert_eq!(state.action.status, AutomationActionStatus::Failed);
    assert!(
        state
            .action
            .last_error
            .as_deref()
            .is_some_and(|message| message.contains("accepted chat history"))
    );
    assert!(
        fixture
            .chat
            .unread_snapshot()
            .await
            .unwrap()
            .messages
            .iter()
            .any(|message| message.id == "new-after-full-history")
    );
}

#[tokio::test]
async fn queued_unread_action_coalesces_messages_arriving_before_execution() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user("first-message".into(), "First pending message.".into())
        .await
        .unwrap();
    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    fixture
        .chat
        .append_user("second-message".into(), "Second pending message.".into())
        .await
        .unwrap();
    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap();
    fixture.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::GenerationComplete {
            final_markdown: None,
            canonical_url: Some("https://chatgpt.com/c/example".into()),
        },
    ]);

    fixture
        .supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 2)
        .await
        .unwrap()
        .unwrap();

    let executions = fixture.executor.executions.lock().unwrap();
    assert_eq!(executions.len(), 1);
    let ActionPrompt::OfflineChat { messages, .. } = &executions[0].request.prompt else {
        panic!("expected an offline chat action");
    };
    assert_eq!(
        messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        vec!["first-message", "second-message"]
    );
}

#[tokio::test]
async fn queued_unread_action_is_rebuilt_when_message_markdown_changes() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "edited-before-send".into(),
            "Original pending message.".into(),
        )
        .await
        .unwrap();
    let offline_at = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
    fixture.supervisor.scan_once(offline_at).await.unwrap();
    assert_eq!(fixture.supervisor.queued_count().await, 1);

    let path = fixture.chat.path().to_path_buf();
    let transcript = std::fs::read_to_string(&path).unwrap();
    assert!(transcript.contains("Original pending message."));
    std::fs::write(
        &path,
        transcript.replace("Original pending message.", "Revised pending message."),
    )
    .unwrap();

    assert!(
        fixture
            .supervisor
            .run_next(offline_at + 1)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.executor.calls.load(Ordering::SeqCst), 0);

    fixture.supervisor.scan_once(offline_at + 2).await.unwrap();
    fixture.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::GenerationComplete {
            final_markdown: None,
            canonical_url: Some("https://chatgpt.com/c/example".into()),
        },
    ]);
    fixture
        .supervisor
        .run_next(offline_at + 3)
        .await
        .unwrap()
        .unwrap();

    let executions = fixture.executor.executions.lock().unwrap();
    assert_eq!(executions.len(), 1);
    let ActionPrompt::OfflineChat { messages, .. } = &executions[0].request.prompt else {
        panic!("expected an offline chat action");
    };
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, "edited-before-send");
    assert_message_body(&messages[0].markdown, "Revised pending message.");
}

#[tokio::test]
async fn queued_unread_action_is_rebuilt_when_message_order_changes() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user("reordered-first".into(), "First pending message.".into())
        .await
        .unwrap();
    fixture
        .chat
        .append_user("reordered-second".into(), "Second pending message.".into())
        .await
        .unwrap();
    let offline_at = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
    fixture.supervisor.scan_once(offline_at).await.unwrap();
    assert_eq!(fixture.supervisor.queued_count().await, 1);

    let path = fixture.chat.path().to_path_buf();
    let transcript = std::fs::read_to_string(&path).unwrap();
    let first_marker = "<!-- codexify-user-message:v1:start id=\"reordered-first\"";
    let second_marker = "<!-- codexify-user-message:v1:start id=\"reordered-second\"";
    let second_end_marker = "<!-- codexify-user-message:v1:end id=\"reordered-second\" -->\n";
    let first_start = transcript.find(first_marker).unwrap();
    let second_start = transcript.find(second_marker).unwrap();
    let second_end = transcript[second_start..]
        .find(second_end_marker)
        .map(|offset| second_start + offset + second_end_marker.len())
        .unwrap();
    let reordered = format!(
        "{}{}{}{}",
        &transcript[..first_start],
        &transcript[second_start..second_end],
        &transcript[first_start..second_start],
        &transcript[second_end..]
    );
    std::fs::write(&path, reordered).unwrap();
    assert_eq!(
        fixture
            .chat
            .unread_snapshot()
            .await
            .unwrap()
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        vec!["reordered-second", "reordered-first"]
    );

    assert!(
        fixture
            .supervisor
            .run_next(offline_at + 1)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.executor.calls.load(Ordering::SeqCst), 0);

    fixture.supervisor.scan_once(offline_at + 2).await.unwrap();
    fixture.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::GenerationComplete {
            final_markdown: None,
            canonical_url: Some("https://chatgpt.com/c/example".into()),
        },
    ]);
    fixture
        .supervisor
        .run_next(offline_at + 3)
        .await
        .unwrap()
        .unwrap();

    let executions = fixture.executor.executions.lock().unwrap();
    assert_eq!(executions.len(), 1);
    let ActionPrompt::OfflineChat { messages, .. } = &executions[0].request.prompt else {
        panic!("expected an offline chat action");
    };
    assert_eq!(
        messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        vec!["reordered-second", "reordered-first"]
    );
}

#[tokio::test]
async fn unread_chat_is_queued_ahead_of_goal_only_work_across_tasks() {
    let fixture = fixture().await;
    let goal = fixture
        .store
        .set_goal(&fixture.task, "Continue the goal-only task.", 100)
        .unwrap();
    fixture
        .store
        .run_goal(&fixture.task, goal.revision, 101)
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();

    let (_chat_task, chat) = add_task(
        &fixture,
        "priority-unread-task",
        "https://chatgpt.com/c/priority-unread",
    )
    .await;
    chat.record_agent_call(1_000).await.unwrap();
    chat.append_user(
        "priority-message".into(),
        "Handle this owner message first.".into(),
    )
    .await
    .unwrap();

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();

    assert_eq!(fixture.supervisor.queued_count().await, 2);
    assert_eq!(
        fixture.supervisor.next_queued_kind().await,
        Some(codexify::agent_automation::AutomationActionKind::OfflineChat)
    );
}

#[tokio::test]
async fn concurrent_run_next_calls_still_execute_only_one_global_action_at_a_time() {
    let fixture = fixture().await;
    let (other_task, other_chat) = add_task(
        &fixture,
        "serialized-second-task",
        "https://chatgpt.com/c/serialized-second",
    )
    .await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    other_chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user("serialized-first".into(), "Run first.".into())
        .await
        .unwrap();
    other_chat
        .append_user("serialized-second".into(), "Run second.".into())
        .await
        .unwrap();

    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let executor = Arc::new(SerialExecutor {
        calls: AtomicUsize::new(0),
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
        started: started_tx,
        release: release.clone(),
    });
    let supervisor = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        executor.clone(),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    supervisor
        .register_task(AutomationTaskHandle::new(
            other_task,
            0,
            fixture.store.clone(),
            other_chat,
        ))
        .await
        .unwrap();
    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(supervisor.queued_count().await, 2);

    let first = tokio::spawn({
        let supervisor = supervisor.clone();
        async move {
            supervisor
                .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
                .await
        }
    });
    let second = tokio::spawn({
        let supervisor = supervisor.clone();
        async move {
            supervisor
                .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
                .await
        }
    });
    assert_eq!(started_rx.recv().await, Some(1));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), started_rx.recv())
            .await
            .is_err()
    );

    release.add_permits(1);
    assert_eq!(started_rx.recv().await, Some(2));
    release.add_permits(1);
    first.await.unwrap().unwrap().unwrap();
    second.await.unwrap().unwrap().unwrap();
    assert_eq!(executor.max_active.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stable_browser_final_is_persisted_once_when_chat_write_produces_no_output() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "fallback-message".into(),
            "Produce a browser fallback answer.".into(),
        )
        .await
        .unwrap();
    fixture.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::GenerationComplete {
            final_markdown: Some("Stable browser fallback answer.".into()),
            canonical_url: Some("https://chatgpt.com/c/example".into()),
        },
    ]);

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    fixture
        .supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap()
        .unwrap();

    let transcript = fixture.chat.clean_markdown_transcript().await.unwrap();
    assert_eq!(
        transcript
            .matches("Stable browser fallback answer.")
            .count(),
        1
    );
    assert_eq!(
        fixture
            .store
            .load(&fixture.task)
            .unwrap()
            .action
            .last_stable_answer
            .as_deref(),
        Some("Stable browser fallback answer.")
    );
}

#[tokio::test]
async fn oversized_transient_browser_status_is_bounded_without_orphaning_the_action() {
    let fixture = fixture().await;
    let supervisor = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        Arc::new(LargeTransientExecutor),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "large-transient-message".into(),
            "Complete despite verbose transient browser status.".into(),
        )
        .await
        .unwrap();

    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap()
        .unwrap();

    let live = supervisor.live_state(&fixture.task).await.unwrap();
    assert!(
        live.status
            .as_ref()
            .is_some_and(|value| value.len() <= 1024 * 1024)
    );
    assert_eq!(live.phase, AutomationActionStatus::Idle);
    assert!(
        fixture
            .chat
            .clean_markdown_transcript()
            .await
            .unwrap()
            .contains("Complete after long transient output.")
    );
}

#[tokio::test]
async fn streamed_answer_deltas_reconstruct_a_final_omitted_from_the_terminal_event() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "streamed-fallback-message".into(),
            "Produce a browser answer whose terminal event omits the full body.".into(),
        )
        .await
        .unwrap();
    fixture.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::AnswerDelta {
            markdown: "Stable streamed browser ".into(),
            continuation: false,
        },
        CompanionEventKind::AnswerDelta {
            markdown: "fallback answer.".into(),
            continuation: true,
        },
        CompanionEventKind::GenerationComplete {
            final_markdown: None,
            canonical_url: Some("https://chatgpt.com/c/example".into()),
        },
    ]);

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    fixture
        .supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap()
        .unwrap();

    let expected = "Stable streamed browser fallback answer.";
    let transcript = fixture.chat.clean_markdown_transcript().await.unwrap();
    assert_eq!(transcript.matches(expected).count(), 1);
    assert_eq!(
        fixture
            .store
            .load(&fixture.task)
            .unwrap()
            .action
            .last_stable_answer
            .as_deref(),
        Some(expected)
    );
}

#[tokio::test]
async fn large_stable_browser_final_does_not_orphan_the_persisted_action() {
    let fixture = fixture().await;
    let supervisor = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        Arc::new(LargeFinalAnswerExecutor),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "large-stable-final".into(),
            "Persist a stable browser answer larger than its diagnostic preview.".into(),
        )
        .await
        .unwrap();

    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap()
        .unwrap();

    let expected = format!("{}{}", "a".repeat(40 * 1024), "b".repeat(40 * 1024));
    assert!(
        fixture
            .chat
            .clean_markdown_transcript()
            .await
            .unwrap()
            .contains(&expected)
    );
    let state = fixture.store.load(&fixture.task).unwrap();
    assert_eq!(state.action.status, AutomationActionStatus::Idle);
    assert!(
        state
            .action
            .last_stable_answer
            .as_ref()
            .is_none_or(|answer| answer.len() <= 64 * 1024)
    );
}

#[tokio::test]
async fn chat_write_output_suppresses_duplicate_browser_final_persistence() {
    let fixture = fixture().await;
    let executor = Arc::new(ChatWritingExecutor {
        chat: fixture.chat.clone(),
    });
    let supervisor =
        AgentAutomationSupervisor::new(fixture.config.markdown_chat.automation.clone(), executor);
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user("racing-message".into(), "Trigger both output paths.".into())
        .await
        .unwrap();

    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap()
        .unwrap();

    let transcript = fixture.chat.clean_markdown_transcript().await.unwrap();
    assert!(transcript.contains("Authoritative output from chat_write."));
    assert!(!transcript.contains("Duplicate browser final answer."));
    assert_eq!(
        fixture
            .store
            .load(&fixture.task)
            .unwrap()
            .action
            .last_stable_answer
            .as_deref(),
        Some("Duplicate browser final answer.")
    );
}

#[tokio::test]
async fn changing_selection_invalidates_queued_work_instead_of_mutating_its_identity() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "selection-message".into(),
            "Use the newly selected effort.".into(),
        )
        .await
        .unwrap();
    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    let old_action = fixture
        .store
        .load(&fixture.task)
        .unwrap()
        .action
        .id
        .unwrap();

    fixture
        .store
        .set_selection(
            &fixture.task,
            "gpt-6-sol",
            "medium",
            "catalog-2",
            "account-1",
            2_000,
        )
        .unwrap();
    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap();

    let new_action = fixture
        .store
        .load(&fixture.task)
        .unwrap()
        .action
        .id
        .unwrap();
    assert_ne!(new_action, old_action);
    assert_eq!(fixture.supervisor.queued_count().await, 1);
    fixture.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::GenerationComplete {
            final_markdown: None,
            canonical_url: Some("https://chatgpt.com/c/example".into()),
        },
    ]);
    fixture
        .supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fixture.executor.executions.lock().unwrap()[0]
            .request
            .effort
            .as_deref(),
        Some("medium")
    );
}

#[tokio::test]
async fn owner_generation_change_invalidates_pairing_and_queued_work_before_send() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "owner-change-message".into(),
            "Do not send this to the previous physical owner.".into(),
        )
        .await
        .unwrap();
    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(fixture.supervisor.queued_count().await, 1);

    fixture
        .supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            1,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();

    assert_eq!(fixture.supervisor.queued_count().await, 0);
    let state = fixture.store.load(&fixture.task).unwrap();
    assert!(state.pairing.verified_owner_generation.is_none());
    assert_eq!(state.action.status, AutomationActionStatus::Unverified);
    assert!(
        fixture
            .supervisor
            .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.executor.calls.load(Ordering::SeqCst), 0);
    assert!(!fixture.chat.read(false).await.unwrap().text.is_empty());
}

#[tokio::test]
async fn queue_capacity_leaves_messages_unread_and_records_one_bounded_failure() {
    let fixture = fixture_with_queue_limit(1).await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user("capacity-first".into(), "First capacity message.".into())
        .await
        .unwrap();
    let (other_task, other_chat) = add_task(
        &fixture,
        "capacity-second-task",
        "https://chatgpt.com/c/capacity-second",
    )
    .await;
    other_chat.record_agent_call(1_000).await.unwrap();
    other_chat
        .append_user("capacity-second".into(), "Second capacity message.".into())
        .await
        .unwrap();

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();

    assert_eq!(fixture.supervisor.queued_count().await, 1);
    let states = [
        fixture.store.load(&fixture.task).unwrap(),
        fixture.store.load(&other_task).unwrap(),
    ];
    assert_eq!(
        states
            .iter()
            .filter(|state| state.action.status == AutomationActionStatus::Failed)
            .count(),
        1
    );
    let failure = states
        .iter()
        .find(|state| state.action.status == AutomationActionStatus::Failed)
        .unwrap();
    let failed_action_id = failure
        .action
        .id
        .as_deref()
        .expect("queue-local failures need an explicit retry identity")
        .to_string();
    assert!(
        failure
            .action
            .last_error
            .as_deref()
            .unwrap()
            .contains("queue capacity")
    );
    assert!(!fixture.chat.read(false).await.unwrap().text.is_empty());
    assert!(!other_chat.read(false).await.unwrap().text.is_empty());

    fixture
        .supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap()
        .unwrap();
    let failed_task = if states[0].action.status == AutomationActionStatus::Failed {
        fixture.task.clone()
    } else {
        other_task
    };
    let retried = fixture
        .supervisor
        .retry_failed_action(
            &failed_task,
            &failed_action_id,
            1_000 + AUTOMATION_OFFLINE_AFTER_MS + 2,
        )
        .await
        .unwrap();
    assert_eq!(retried.status, AutomationActionStatus::Queued);
    assert_ne!(retried.id.as_deref(), Some(failed_action_id.as_str()));
    assert_eq!(fixture.supervisor.queued_count().await, 1);
}

#[tokio::test]
async fn pre_send_failures_retry_with_persisted_backoff_then_require_manual_retry() {
    let fixture = fixture().await;
    let executor = Arc::new(PreSendFailureExecutor::default());
    let mut supervisor = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        executor.clone(),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "pre-send-failure".into(),
            "Keep this unread until the browser is repaired.".into(),
        )
        .await
        .unwrap();

    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    let error = match supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
    {
        Ok(_) => panic!("pre-Send failure must remain an error"),
        Err(error) => error,
    };
    assert!(error.contains("before Send"));
    assert_eq!(
        fixture.store.load(&fixture.task).unwrap().action.status,
        AutomationActionStatus::Failed
    );
    assert!(!fixture.chat.read(false).await.unwrap().text.is_empty());

    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 2)
        .await
        .unwrap();
    assert_eq!(supervisor.queued_count().await, 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    let mut last_retry_at = 0;
    for (attempt, delay) in [
        (0, 30_000),
        (1, 60_000),
        (2, 120_000),
        (3, 240_000),
        (4, 480_000),
    ] {
        let failed = fixture.store.load(&fixture.task).unwrap();
        assert_eq!(failed.action.automatic_retry_attempts, attempt);
        assert!(failed.is_automatically_retrying());
        let retry_at = failed.automatic_retry_at_ms().unwrap();
        assert!(
            (failed.action.updated_at_ms.unwrap() + delay
                ..=failed.action.updated_at_ms.unwrap() + delay + delay / 10)
                .contains(&retry_at)
        );
        if attempt == 1 {
            // Reloading the supervisor/store cannot reset the retry episode.
            let reopened = Arc::new(AgentAutomationStore::for_config(&fixture.config).unwrap());
            supervisor = AgentAutomationSupervisor::new(
                fixture.config.markdown_chat.automation.clone(),
                executor.clone(),
            );
            supervisor
                .register_task(AutomationTaskHandle::new(
                    fixture.task.clone(),
                    0,
                    reopened,
                    fixture.chat.clone(),
                ))
                .await
                .unwrap();
        }
        supervisor.scan_once(retry_at - 1).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 0);
        supervisor.scan_once(retry_at).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 1);
        assert!(
            fixture
                .store
                .load(&fixture.task)
                .unwrap()
                .is_automatically_retrying()
        );
        assert!(supervisor.run_next(retry_at + 1).await.is_err());
        last_retry_at = retry_at;
    }
    let exhausted = fixture.store.load(&fixture.task).unwrap();
    assert_eq!(exhausted.action.automatic_retry_attempts, 5);
    assert!(exhausted.automatic_retry_at_ms().is_none());
    assert!(!exhausted.is_automatically_retrying());
    assert_eq!(exhausted.retry_mode(), Some("retry"));
    supervisor
        .scan_once(last_retry_at + 86_400_000)
        .await
        .unwrap();
    assert_eq!(supervisor.queued_count().await, 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 6);
    assert_eq!(
        fixture.chat.unread_snapshot().await.unwrap().messages.len(),
        1
    );
    supervisor
        .retry_failed_action(
            &fixture.task,
            exhausted.action.id.as_deref().unwrap(),
            last_retry_at + 86_400_001,
        )
        .await
        .unwrap();
    assert_eq!(
        fixture
            .store
            .load(&fixture.task)
            .unwrap()
            .action
            .automatic_retry_attempts,
        0
    );
    assert!(
        supervisor
            .run_next(last_retry_at + 86_400_002)
            .await
            .is_err()
    );
    assert!(
        fixture
            .store
            .load(&fixture.task)
            .unwrap()
            .automatic_retry_at_ms()
            .is_some()
    );
}

#[tokio::test]
#[ignore = "requires CODEXIFY_TEST_APPRISE_PYTHON; local notification receiver only"]
async fn failed_send_notifies_once_across_scans_and_reopening_the_chat() {
    let mut f = fixture().await;
    let (sender, mut received) = mpsc::unbounded_channel();
    let app = Router::new().route(
        "/",
        post(move |body: String| {
            let sender = sender.clone();
            async move {
                sender
                    .send(serde_json::from_str::<serde_json::Value>(&body).unwrap())
                    .unwrap();
                axum::Json(serde_json::json!({"id":"test","event":"message"}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    f.config.markdown_chat.notifications = Some(
        serde_json::from_value(serde_json::json!({
            "urls":[format!("ntfy://{address}/test?image=no")],
            "pythonPath":std::env::var("CODEXIFY_TEST_APPRISE_PYTHON").unwrap(),
            "events":{"messages":false,"offline":false}
        }))
        .unwrap(),
    );
    let store = Arc::new(AgentAutomationStore::for_config(&f.config).unwrap());
    let supervisor = AgentAutomationSupervisor::new(
        f.config.markdown_chat.automation.clone(),
        Arc::new(PreSendFailureExecutor::default()),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    f.chat.record_agent_call(1000).await.unwrap();
    f.chat
        .append_user("failed-send".into(), "Please continue".into())
        .await
        .unwrap();
    supervisor
        .scan_once(1000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert!(
        supervisor
            .run_next(1001 + AUTOMATION_OFFLINE_AFTER_MS)
            .await
            .is_err()
    );
    assert!(received.try_recv().is_err());
    for _ in 0..3 {
        let retry_at = store
            .load(&f.task)
            .unwrap()
            .automatic_retry_at_ms()
            .unwrap();
        supervisor.scan_once(retry_at).await.unwrap();
        assert!(supervisor.run_next(retry_at + 1).await.is_err());
    }
    let alert = tokio::time::timeout(Duration::from_secs(10), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        alert["message"]
            .as_str()
            .unwrap()
            .contains("Selector verification failed before Send")
    );
    for _ in 0..3 {
        supervisor
            .scan_once(1002 + AUTOMATION_OFFLINE_AFTER_MS)
            .await
            .unwrap();
    }
    let reopened = MarkdownChatStore::default()
        .chat(&f.config, Some(&f.task), &SessionState::new())
        .unwrap();
    supervisor
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            store,
            reopened,
        ))
        .await
        .unwrap();
    supervisor
        .scan_once(1003 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(received.try_recv().is_err());
    assert!(
        !f.chat.read(false).await.unwrap().text.is_empty(),
        "a notification must not consume the unsent user message"
    );
    server.abort();
}

#[tokio::test]
async fn confirmed_stop_failures_retry_after_backoff_without_losing_or_duplicating_messages() {
    for snapshot_failure in [false, true] {
        let f = fixture().await;
        let executor = Arc::new(StopFailureThenSuccessExecutor {
            calls: AtomicUsize::new(0),
            snapshot_failure,
        });
        let supervisor = AgentAutomationSupervisor::new(
            f.config.markdown_chat.automation.clone(),
            executor.clone(),
        );
        supervisor
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                f.store.clone(),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        let goal = f
            .store
            .set_goal(&f.task, "Continue the active goal.", 100)
            .unwrap();
        f.store.run_goal(&f.task, goal.revision, 101).unwrap();
        f.chat.record_agent_call(1_000).await.unwrap();
        f.chat
            .append_user(
                "stop-recovery-message".into(),
                "Deliver this once after stopping.".into(),
            )
            .await
            .unwrap();
        let began = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
        supervisor.scan_once(began).await.unwrap();
        let _ = supervisor.run_next(began + 1).await;
        let failed = f.store.load(&f.task).unwrap();
        assert_eq!(failed.action.status, AutomationActionStatus::Failed);
        assert!(failed.is_automatically_retrying());
        assert_eq!(failed.action.send_accepted, Some(false));
        assert_eq!(failed.goal.as_ref().unwrap().turn_count, 1);
        let retry_at = failed.automatic_retry_at_ms().unwrap();
        supervisor.scan_once(retry_at - 1).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 0);
        assert_eq!(f.chat.unread_snapshot().await.unwrap().messages.len(), 1);
        supervisor.scan_once(retry_at).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 1);
        assert!(f.store.load(&f.task).unwrap().is_automatically_retrying());
        supervisor.run_next(retry_at + 1).await.unwrap();
        let saved = f.store.load(&f.task).unwrap();
        assert!(saved.action.messages_fenced);
        assert_eq!(saved.goal.as_ref().unwrap().turn_count, 2);
        assert_eq!(
            saved.goal.as_ref().unwrap().status,
            codexify::agent_automation::GoalStatus::Active
        );
        assert!(saved.automatic_retry_at_ms().is_none());
        assert!(!saved.is_automatically_retrying());
        supervisor.scan_once(retry_at + 2).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 0);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn ambiguous_combined_send_is_never_replayed_in_the_same_activity_epoch() {
    let fixture = fixture().await;
    let goal = fixture
        .store
        .set_goal(&fixture.task, "Continue after the owner message.", 100)
        .unwrap();
    fixture
        .store
        .run_goal(&fixture.task, goal.revision, 101)
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "ambiguous-message".into(),
            "This send must not be replayed.".into(),
        )
        .await
        .unwrap();
    let supervisor = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        Arc::new(AmbiguousExecutor),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();

    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    let error = match supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
    {
        Ok(_) => panic!("ambiguous send must fail closed"),
        Err(error) => error,
    };
    assert!(error.contains("may have been accepted"));
    let state = fixture.store.load(&fixture.task).unwrap();
    assert_eq!(state.action.status, AutomationActionStatus::Ambiguous);
    assert_eq!(state.chat_mirror.message_ids, vec!["ambiguous-message"]);
    assert!(state.goal_wake.nudged_epoch.is_some());

    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 2)
        .await
        .unwrap();
    assert_eq!(supervisor.queued_count().await, 0);
}

#[tokio::test]
async fn browser_deltas_are_visible_live_but_not_persisted_before_completion() {
    let fixture = fixture().await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let supervisor = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        Arc::new(StreamingExecutor {
            started: started.clone(),
            release: release.clone(),
        }),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user("stream-message".into(), "Show browser progress.".into())
        .await
        .unwrap();
    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();

    let runner = tokio::spawn({
        let supervisor = supervisor.clone();
        async move {
            supervisor
                .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    let live = supervisor.live_state(&fixture.task).await.unwrap();
    assert_eq!(live.status.as_deref(), Some("Using the browser"));
    assert_eq!(live.commentary.as_deref(), Some("Visible commentary"));
    assert_eq!(live.answer.as_deref(), Some("Partial answer"));
    assert!(
        !fixture
            .chat
            .clean_markdown_transcript()
            .await
            .unwrap()
            .contains("Partial answer")
    );

    release.notify_one();
    runner.await.unwrap().unwrap().unwrap();
    assert!(
        fixture
            .chat
            .clean_markdown_transcript()
            .await
            .unwrap()
            .contains("Complete answer")
    );
}

#[tokio::test]
async fn replacing_a_goal_during_generation_does_not_apply_the_old_terminal_receipt() {
    let fixture = fixture().await;
    let goal = fixture
        .store
        .set_goal(&fixture.task, "Original browser goal.", 100)
        .unwrap();
    fixture
        .store
        .run_goal(&fixture.task, goal.revision, 101)
        .unwrap();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let supervisor = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        Arc::new(StreamingExecutor {
            started: started.clone(),
            release: release.clone(),
        }),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    let runner = tokio::spawn({
        let supervisor = supervisor.clone();
        async move {
            supervisor
                .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .unwrap();

    let replacement = fixture
        .store
        .set_goal(&fixture.task, "Replacement browser goal.", 2_000)
        .unwrap();
    release.notify_one();

    assert!(runner.await.unwrap().unwrap().is_some());
    let state = fixture.store.load(&fixture.task).unwrap();
    assert_eq!(state.goal.as_ref().unwrap().revision, replacement.revision);
    assert_eq!(
        state.goal.as_ref().unwrap().markdown,
        "Replacement browser goal."
    );
    assert!(state.action.id.is_none());
    assert!(
        !fixture
            .chat
            .clean_markdown_transcript()
            .await
            .unwrap()
            .contains("Complete answer")
    );
}

#[tokio::test]
async fn owner_generation_change_during_generation_discards_old_browser_output() {
    let fixture = fixture().await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let supervisor = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        Arc::new(StreamingExecutor {
            started: started.clone(),
            release: release.clone(),
        }),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "owner-changed-during-generation".into(),
            "Do not persist output from the previous physical owner.".into(),
        )
        .await
        .unwrap();
    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    let runner = tokio::spawn({
        let supervisor = supervisor.clone();
        async move {
            supervisor
                .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .unwrap();

    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            1,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    release.notify_one();

    assert!(runner.await.unwrap().unwrap().is_some());
    let state = fixture.store.load(&fixture.task).unwrap();
    assert!(state.pairing.verified_owner_generation.is_none());
    assert_eq!(state.action.status, AutomationActionStatus::Unverified);
    assert!(
        !fixture
            .chat
            .clean_markdown_transcript()
            .await
            .unwrap()
            .contains("Complete answer")
    );
    assert!(
        fixture
            .chat
            .unread_snapshot()
            .await
            .unwrap()
            .messages
            .iter()
            .any(|message| message.id == "owner-changed-during-generation")
    );
}

#[tokio::test]
async fn account_change_during_generation_discards_old_browser_output() {
    let fixture = fixture().await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let supervisor = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        Arc::new(StreamingExecutor {
            started: started.clone(),
            release: release.clone(),
        }),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "account-changed-during-generation".into(),
            "Do not persist output from the previous ChatGPT account.".into(),
        )
        .await
        .unwrap();
    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    let runner = tokio::spawn({
        let supervisor = supervisor.clone();
        async move {
            supervisor
                .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .unwrap();

    fixture
        .store
        .set_selection(
            &fixture.task,
            "gpt-6-sol",
            "high",
            "catalog-2",
            "account-2",
            2_000,
        )
        .unwrap();
    release.notify_one();

    assert!(runner.await.unwrap().unwrap().is_some());
    let state = fixture.store.load(&fixture.task).unwrap();
    assert!(state.pairing.verified_owner_generation.is_none());
    assert_eq!(state.action.status, AutomationActionStatus::Unverified);
    assert!(
        !fixture
            .chat
            .clean_markdown_transcript()
            .await
            .unwrap()
            .contains("Complete answer")
    );
}

#[tokio::test]
async fn superseded_combined_action_still_fences_its_accepted_unread_batch() {
    let fixture = fixture().await;
    let goal = fixture
        .store
        .set_goal(&fixture.task, "Original combined goal.", 100)
        .unwrap();
    fixture
        .store
        .run_goal(&fixture.task, goal.revision, 101)
        .unwrap();
    fixture
        .chat
        .append_user(
            "superseded-combined-message".into(),
            "Do not mirror this accepted message twice.".into(),
        )
        .await
        .unwrap();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let supervisor = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        Arc::new(StreamingExecutor {
            started: started.clone(),
            release: release.clone(),
        }),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    let runner = tokio::spawn({
        let supervisor = supervisor.clone();
        async move {
            supervisor
                .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .unwrap();

    fixture
        .store
        .set_goal(&fixture.task, "Replacement combined goal.", 2_000)
        .unwrap();
    release.notify_one();

    assert!(runner.await.unwrap().unwrap().is_some());
    let state = fixture.store.load(&fixture.task).unwrap();
    assert_eq!(
        state.chat_mirror.message_ids,
        vec!["superseded-combined-message"]
    );
    assert!(state.action.id.is_none());
    assert!(!fixture.chat.read(false).await.unwrap().text.is_empty());
}

#[tokio::test]
async fn real_executor_defers_only_an_unowned_busy_pairing_window_to_companion_validation() {
    for (pairing, session_status, active_id, authenticated, expected_execute_calls) in [
        (true, "busy", None, true, 1),
        (false, "busy", None, true, 0),
        (true, "busy", Some("other-action"), true, 0),
        (true, "unavailable", None, true, 0),
        (true, "unavailable", None, false, 0),
    ] {
        let execute_calls = Arc::new(AtomicUsize::new(0));
        let execute_counter = execute_calls.clone();
        let app = Router::new()
            // The process is responsive even when the ChatGPT session is unavailable.
            .route("/v1/liveness", get(|| async {
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD, "available":true
                }))
            }))
            .route("/v1/health", get(move || async move {
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "kind":"codexify-browser-companion", "build":COMPANION_BUILD,
                    "status":session_status, "authenticated":authenticated,
                    "accountFingerprint":if authenticated {Some("account-1")} else {None}, "activeActionId":active_id
                }))
            }))
            .route("/v1/actions/execute", post(move || {
                let execute_counter = execute_counter.clone();
                async move {
                    execute_counter.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({
                        "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                        "build":COMPANION_BUILD, "actionId":"draft-replacement",
                        "state":"external_interference", "sendAccepted":false,
                        "canonicalUrl":null, "selected":null, "lastEventSequence":0,
                        "attention":{"code":"external_interference","message":"User draft is not a pairing verification"}
                    }))
                }
            }));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let descriptor = write_companion_descriptor(root.path(), &endpoint);
        let executor =
            BrowserAutomationExecutor::new(codexify::markdown_chat::AgentAutomationConfig {
                enabled: true,
                descriptor_path: descriptor.display().to_string(),
                ..Default::default()
            });
        let execution = AutomationExecution {
            task_key: "a".repeat(64),
            owner_generation: 0,
            account_fingerprint: "account-1".into(),
            catalog_revision: "conversation-current".into(),
            activity_epoch: "pair-verification".into(),
            activity_sequence: 1,
            tool_call_count_at_start: 0,
            unread_message_ids: Vec::new(),
            transcript_revision: String::new(),
            pairing_commit: None,
            request: codexify::agent_automation::ExecuteActionRequest {
                action_id: "draft-replacement".into(),
                idempotency_key: "draft-replacement-key".into(),
                url: "https://chatgpt.com/c/example".into(),
                expected_url: "https://chatgpt.com/c/example".into(),
                pairing_generation: 1,
                connector_name: "Codexify".into(),
                expected_account_fingerprint: "account-1".into(),
                model: None,
                effort: None,
                auto_approve_tool_calls: false,
                bypass_conversation_cooldown: false,
                retain_pairing_window: false,
                pairing_continuation_of: None,
                agent_ticket: None,
                prompt: if pairing {
                    ActionPrompt::PairVerify {
                        reference: "A".repeat(43),
                    }
                } else {
                    ActionPrompt::OfflineGoal { goal_revision: 1 }
                },
            },
        };
        let (events, _events_rx) = mpsc::unbounded_channel();
        let outcome = executor
            .execute(execution, events, CancellationToken::new())
            .await;
        assert_eq!(execute_calls.load(Ordering::SeqCst), expected_execute_calls);
        if expected_execute_calls == 1 {
            let snapshot = outcome.unwrap();
            assert!(!snapshot.send_accepted);
            assert_eq!(snapshot.state, CompanionActionState::ExternalInterference);
        } else {
            let error = outcome.err().unwrap();
            assert!(!error.send_accepted);
            assert!(error.message.contains(if active_id.is_some() {
                "busy with another action"
            } else {
                "not ready"
            }));
        }
        server.abort();
    }
}

#[tokio::test]
async fn real_executor_delegates_authentication_recovery_but_rejects_known_account_mismatch() {
    for (authenticated, legacy_rejection) in [(false, false), (false, true), (true, false)] {
        let execute_calls = Arc::new(AtomicUsize::new(0));
        let execute_counter = execute_calls.clone();
        let app = Router::new()
            .route(
                "/v1/health",
                get(move |headers: HeaderMap| async move {
                    if !authorized(&headers) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    axum::Json(serde_json::json!({
                        "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                        "kind":"codexify-browser-companion",
                        "build":COMPANION_BUILD,
                        "status":if authenticated { "idle" } else { "authentication_required" },
                        "authenticated":authenticated,
                        "accountFingerprint":authenticated.then_some("different-account"),
                        "activeActionId":null
                    }))
                    .into_response()
                }),
            )
            .route(
                "/v1/session/capabilities",
                post(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
            )
            .route(
                "/v1/actions/execute",
                post(move || {
                    let execute_counter = execute_counter.clone();
                    async move {
                        execute_counter.fetch_add(1, Ordering::SeqCst);
                        if legacy_rejection {
                            return StatusCode::CONFLICT.into_response();
                        }
                        axum::Json(serde_json::json!({
                            "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                            "build":COMPANION_BUILD, "actionId":"action-account-check",
                            "state":"authentication_required", "sendAccepted":false,
                            "canonicalUrl":null, "selected":null, "lastEventSequence":0,
                            "attention":{"code":"authentication_required","message":"Sign in after recovery reload"}
                        })).into_response()
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let descriptor = write_companion_descriptor(root.path(), &endpoint);
        let config = codexify::markdown_chat::AgentAutomationConfig {
            enabled: true,
            descriptor_path: descriptor.display().to_string(),
            ..Default::default()
        };
        let executor = BrowserAutomationExecutor::new(config);
        let execution = AutomationExecution {
            task_key: "a".repeat(64),
            owner_generation: 0,
            account_fingerprint: "expected-account".into(),
            catalog_revision: "catalog-1".into(),
            activity_epoch: "epoch-1".into(),
            activity_sequence: 1,
            tool_call_count_at_start: 1,
            unread_message_ids: Vec::new(),
            transcript_revision: "revision-1".into(),
            pairing_commit: None,
            request: codexify::agent_automation::ExecuteActionRequest {
                action_id: "action-account-check".into(),
                idempotency_key: "execute-account-check".into(),
                url: "https://chatgpt.com/c/example".into(),
                pairing_generation: 1,
                connector_name: "Codexify".into(),
                expected_account_fingerprint: "account-1".into(),
                model: Some("gpt-6-sol".into()),
                effort: Some("high".into()),
                prompt: ActionPrompt::OfflineGoal { goal_revision: 1 },
                expected_url: "https://chatgpt.com/c/example".into(),
                auto_approve_tool_calls: false,
                bypass_conversation_cooldown: false,
                retain_pairing_window: false,
                pairing_continuation_of: None,
                agent_ticket: None,
            },
        };
        let (events, _events_rx) = mpsc::unbounded_channel();

        let outcome = executor
            .execute(execution, events, CancellationToken::new())
            .await;
        if authenticated || legacy_rejection {
            let error = outcome.err().unwrap();
            assert!(!error.send_accepted);
            assert_eq!(
                error.failure_status,
                Some(AutomationActionStatus::AuthenticationRequired)
            );
            assert!(error.message.contains(if authenticated {
                "account"
            } else {
                "authentication"
            }));
            assert_eq!(
                execute_calls.load(Ordering::SeqCst),
                usize::from(legacy_rejection)
            );
        } else {
            let snapshot = outcome.unwrap();
            assert!(!snapshot.send_accepted);
            assert_eq!(snapshot.state, CompanionActionState::AuthenticationRequired);
            assert_eq!(execute_calls.load(Ordering::SeqCst), 1);
        }
        server.abort();
    }
}

#[tokio::test]
async fn real_executor_marks_an_unreconciled_execute_receipt_as_ambiguous() {
    let app = Router::new()
        .route(
            "/v1/health",
            get(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "kind":"codexify-browser-companion",
                    "build":COMPANION_BUILD,
                    "status":"idle",
                    "authenticated":true,
                    "accountFingerprint":"account-1",
                    "activeActionId":null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/session/capabilities",
            post(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "accountFingerprint":"account-1",
                    "catalogRevision":"catalog-1",
                    "models":[{
                        "id":"gpt-6-sol",
                        "label":"GPT-6 Sol",
                        "efforts":[{
                            "id":"high",
                            "label":"High",
                            "available":true,
                            "reason":null
                        }]
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/actions/execute",
            post(|| async {
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from("{"))
                    .unwrap()
            }),
        )
        .route(
            "/v1/actions/{action_id}",
            get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
        );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let descriptor = write_companion_descriptor(root.path(), &endpoint);
    let config = codexify::markdown_chat::AgentAutomationConfig {
        enabled: true,
        descriptor_path: descriptor.display().to_string(),
        ..Default::default()
    };
    let executor = BrowserAutomationExecutor::new(config);
    let execution = AutomationExecution {
        task_key: "f".repeat(64),
        owner_generation: 0,
        account_fingerprint: "account-1".into(),
        catalog_revision: "catalog-1".into(),
        activity_epoch: "epoch-1".into(),
        activity_sequence: 1,
        tool_call_count_at_start: 1,
        unread_message_ids: vec!["ambiguous-execute-message".into()],
        transcript_revision: "revision-1".into(),
        pairing_commit: None,
        request: codexify::agent_automation::ExecuteActionRequest {
            action_id: "ambiguous-execute-action".into(),
            idempotency_key: "ambiguous-execute-idempotency".into(),
            url: "https://chatgpt.com/c/ambiguous-execute".into(),
            pairing_generation: 1,
            connector_name: "Codexify".into(),
            expected_account_fingerprint: "account-1".into(),
            model: Some("gpt-6-sol".into()),
            effort: Some("high".into()),
            prompt: ActionPrompt::OfflineChat {
                batch_id: "ambiguous-execute-batch".into(),
                messages: vec![codexify::agent_automation::CompanionUnreadMessage {
                    id: "ambiguous-execute-message".into(),
                    markdown: "Do not replay an action whose execute receipt was lost.".into(),
                }],
            },
            expected_url: "https://chatgpt.com/c/ambiguous-execute".into(),
            auto_approve_tool_calls: false,
            bypass_conversation_cooldown: false,
            retain_pairing_window: false,
            pairing_continuation_of: None,
            agent_ticket: None,
        },
    };
    let (events, _events_rx) = mpsc::unbounded_channel();

    let error = match executor
        .execute(execution, events, CancellationToken::new())
        .await
    {
        Ok(_) => panic!("an unreconciled execute receipt must fail closed"),
        Err(error) => error,
    };

    assert!(error.send_accepted);
    server.abort();
}

#[tokio::test]
async fn successful_execute_receipt_stays_ambiguous_when_status_returns_not_found() {
    let app = Router::new()
        .route(
            "/v1/health",
            get(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "kind":"codexify-browser-companion",
                    "build":COMPANION_BUILD,
                    "status":"idle",
                    "authenticated":true,
                    "accountFingerprint":"account-1",
                    "activeActionId":null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/session/capabilities",
            post(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "accountFingerprint":"account-1",
                    "catalogRevision":"catalog-1",
                    "models":[{
                        "id":"gpt-6-sol",
                        "label":"GPT-6 Sol",
                        "efforts":[{
                            "id":"high",
                            "label":"High",
                            "available":true,
                            "reason":null
                        }]
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/actions/execute",
            post(|| async {
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from("{"))
                    .unwrap()
            }),
        )
        .route(
            "/v1/actions/{action_id}",
            get(|| async { StatusCode::NOT_FOUND }),
        );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let descriptor = write_companion_descriptor(root.path(), &endpoint);
    let config = codexify::markdown_chat::AgentAutomationConfig {
        enabled: true,
        descriptor_path: descriptor.display().to_string(),
        ..Default::default()
    };
    let executor = BrowserAutomationExecutor::new(config);
    let execution = AutomationExecution {
        task_key: "e".repeat(64),
        owner_generation: 0,
        account_fingerprint: "account-1".into(),
        catalog_revision: "catalog-1".into(),
        activity_epoch: "epoch-1".into(),
        activity_sequence: 1,
        tool_call_count_at_start: 1,
        unread_message_ids: vec!["ambiguous-not-found-message".into()],
        transcript_revision: "revision-1".into(),
        pairing_commit: None,
        request: codexify::agent_automation::ExecuteActionRequest {
            action_id: "ambiguous-not-found-action".into(),
            idempotency_key: "ambiguous-not-found-idempotency".into(),
            url: "https://chatgpt.com/c/ambiguous-not-found".into(),
            pairing_generation: 1,
            connector_name: "Codexify".into(),
            expected_account_fingerprint: "account-1".into(),
            model: Some("gpt-6-sol".into()),
            effort: Some("high".into()),
            prompt: ActionPrompt::OfflineChat {
                batch_id: "ambiguous-not-found-batch".into(),
                messages: vec![codexify::agent_automation::CompanionUnreadMessage {
                    id: "ambiguous-not-found-message".into(),
                    markdown: "Do not trust a racy status 404 after a successful execute receipt."
                        .into(),
                }],
            },
            expected_url: "https://chatgpt.com/c/ambiguous-not-found".into(),
            auto_approve_tool_calls: false,
            bypass_conversation_cooldown: false,
            retain_pairing_window: false,
            pairing_continuation_of: None,
            agent_ticket: None,
        },
    };
    let (events, _events_rx) = mpsc::unbounded_channel();

    let error = match executor
        .execute(execution, events, CancellationToken::new())
        .await
    {
        Ok(_) => panic!("a malformed successful execute receipt remains ambiguous"),
        Err(error) => error,
    };

    assert!(error.send_accepted);
    server.abort();
}

async fn terminal_receipt_without_send_proof(
    terminal_state: &'static str,
    attention: serde_json::Value,
) -> AutomationExecutionError {
    let app = Router::new()
        .route(
            "/v1/health",
            get(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "kind":"codexify-browser-companion",
                    "build":COMPANION_BUILD,
                    "status":"idle",
                    "authenticated":true,
                    "accountFingerprint":"account-1",
                    "activeActionId":null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/session/capabilities",
            post(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "accountFingerprint":"account-1",
                    "catalogRevision":"catalog-1",
                    "models":[{
                        "id":"gpt-6-sol",
                        "label":"GPT-6 Sol",
                        "efforts":[{
                            "id":"high",
                            "label":"High",
                            "available":true,
                            "reason":null
                        }]
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/actions/execute",
            post(move |headers: HeaderMap| {
                let attention = attention.clone();
                async move {
                    if !authorized(&headers) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    axum::Json(serde_json::json!({
                        "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                        "build":COMPANION_BUILD,
                        "actionId":"idle-without-send-action",
                        "state":terminal_state,
                        "sendAccepted":false,
                        "canonicalUrl":null,
                        "selected":null,
                        "lastEventSequence":0,
                        "attention":attention
                    }))
                    .into_response()
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let descriptor = write_companion_descriptor(root.path(), &endpoint);
    let config = codexify::markdown_chat::AgentAutomationConfig {
        enabled: true,
        descriptor_path: descriptor.display().to_string(),
        ..Default::default()
    };
    let executor = BrowserAutomationExecutor::new(config);
    let execution = AutomationExecution {
        task_key: "d".repeat(64),
        owner_generation: 0,
        account_fingerprint: "account-1".into(),
        catalog_revision: "catalog-1".into(),
        activity_epoch: "epoch-1".into(),
        activity_sequence: 1,
        tool_call_count_at_start: 1,
        unread_message_ids: vec!["idle-without-send-message".into()],
        transcript_revision: "revision-1".into(),
        pairing_commit: None,
        request: codexify::agent_automation::ExecuteActionRequest {
            action_id: "idle-without-send-action".into(),
            idempotency_key: "idle-without-send-idempotency".into(),
            url: "https://chatgpt.com/c/idle-without-send".into(),
            pairing_generation: 1,
            connector_name: "Codexify".into(),
            expected_account_fingerprint: "account-1".into(),
            model: Some("gpt-6-sol".into()),
            effort: Some("high".into()),
            prompt: ActionPrompt::OfflineChat {
                batch_id: "idle-without-send-batch".into(),
                messages: vec![codexify::agent_automation::CompanionUnreadMessage {
                    id: "idle-without-send-message".into(),
                    markdown: "Do not accept a successful terminal receipt without Send proof."
                        .into(),
                }],
            },
            expected_url: "https://chatgpt.com/c/idle-without-send".into(),
            auto_approve_tool_calls: false,
            bypass_conversation_cooldown: false,
            retain_pairing_window: false,
            pairing_continuation_of: None,
            agent_ticket: None,
        },
    };
    let (events, _events_rx) = mpsc::unbounded_channel();

    let error = match executor
        .execute(execution, events, CancellationToken::new())
        .await
    {
        Ok(_) => panic!("idle without Send proof must fail closed"),
        Err(error) => error,
    };

    assert!(error.send_accepted);
    server.abort();
    error
}

#[tokio::test]
async fn terminal_idle_receipt_without_send_proof_is_ambiguous() {
    let error = terminal_receipt_without_send_proof("idle", serde_json::Value::Null).await;
    assert!(error.message.contains("Send acceptance"));
}

#[tokio::test]
async fn ambiguous_receipt_preserves_the_browser_failure_instead_of_a_generic_protocol_error() {
    let message = "Browser operation timed out after 20 seconds";
    let error = terminal_receipt_without_send_proof(
        "ambiguous",
        serde_json::json!({"code":"ambiguous_send", "message":message}),
    )
    .await;
    assert_eq!(error.message, message);
}

#[tokio::test]
async fn accepted_execute_with_unreconciled_event_stream_is_ambiguous() {
    let app = Router::new()
        .route(
            "/v1/health",
            get(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "kind":"codexify-browser-companion",
                    "build":COMPANION_BUILD,
                    "status":"idle",
                    "authenticated":true,
                    "accountFingerprint":"account-1",
                    "activeActionId":null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/session/capabilities",
            post(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "accountFingerprint":"account-1",
                    "catalogRevision":"catalog-1",
                    "models":[{
                        "id":"gpt-6-sol",
                        "label":"GPT-6 Sol",
                        "efforts":[{
                            "id":"high",
                            "label":"High",
                            "available":true,
                            "reason":null
                        }]
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/actions/execute",
            post(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "actionId":"unreconciled-stream-action",
                    "state":"queued",
                    "sendAccepted":false,
                    "canonicalUrl":null,
                    "selected":null,
                    "lastEventSequence":0,
                    "attention":null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/actions/{action_id}/events",
            get(|| async { StatusCode::NOT_FOUND }),
        )
        .route(
            "/v1/actions/{action_id}",
            get(|| async { StatusCode::NOT_FOUND }),
        );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let descriptor = write_companion_descriptor(root.path(), &endpoint);
    let config = codexify::markdown_chat::AgentAutomationConfig {
        enabled: true,
        descriptor_path: descriptor.display().to_string(),
        ..Default::default()
    };
    let executor = BrowserAutomationExecutor::new(config);
    let execution = AutomationExecution {
        task_key: "c".repeat(64),
        owner_generation: 0,
        account_fingerprint: "account-1".into(),
        catalog_revision: "catalog-1".into(),
        activity_epoch: "epoch-1".into(),
        activity_sequence: 1,
        tool_call_count_at_start: 1,
        unread_message_ids: vec!["unreconciled-stream-message".into()],
        transcript_revision: "revision-1".into(),
        pairing_commit: None,
        request: codexify::agent_automation::ExecuteActionRequest {
            action_id: "unreconciled-stream-action".into(),
            idempotency_key: "unreconciled-stream-idempotency".into(),
            url: "https://chatgpt.com/c/unreconciled-stream".into(),
            pairing_generation: 1,
            connector_name: "Codexify".into(),
            expected_account_fingerprint: "account-1".into(),
            model: Some("gpt-6-sol".into()),
            effort: Some("high".into()),
            prompt: ActionPrompt::OfflineChat {
                batch_id: "unreconciled-stream-batch".into(),
                messages: vec![codexify::agent_automation::CompanionUnreadMessage {
                    id: "unreconciled-stream-message".into(),
                    markdown: "Do not retry after losing observation of an accepted action.".into(),
                }],
            },
            expected_url: "https://chatgpt.com/c/unreconciled-stream".into(),
            auto_approve_tool_calls: false,
            bypass_conversation_cooldown: false,
            retain_pairing_window: false,
            pairing_continuation_of: None,
            agent_ticket: None,
        },
    };
    let (events, _events_rx) = mpsc::unbounded_channel();

    let error = match executor
        .execute(execution, events, CancellationToken::new())
        .await
    {
        Ok(_) => panic!("an unreconciled accepted action must fail closed"),
        Err(error) => error,
    };

    assert!(error.send_accepted);
    server.abort();
}

#[tokio::test]
async fn real_pair_executor_records_the_candidate_before_the_async_send_receipt() {
    let expected_url = "https://chatgpt.com/c/pair-production";
    let app = Router::new()
        .route(
            "/v1/health",
            get(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "kind":"codexify-browser-companion",
                    "build":COMPANION_BUILD,
                    "status":"idle",
                    "authenticated":true,
                    "accountFingerprint":"account-1",
                    "activeActionId":null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/session/capabilities",
            post(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "accountFingerprint":"account-1",
                    "catalogRevision":"catalog-1",
                    "models":[{
                        "id":"gpt-6-sol",
                        "label":"GPT-6 Sol",
                        "efforts":[{"id":"high","label":"High","available":true,"reason":null}]
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/actions/execute",
            post(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "actionId":"pair-production-action",
                    "state":"queued",
                    "sendAccepted":false,
                    "canonicalUrl":null,
                    "selected":null,
                    "lastEventSequence":0,
                    "attention":null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/actions/pair-production-action/events",
            get(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                let events = [
                    serde_json::json!({
                        "sequence":1,"actionId":"pair-production-action","atMs":1,
                        "type":"navigating"
                    }),
                    serde_json::json!({
                        "sequence":2,"actionId":"pair-production-action","atMs":2,
                        "type":"model_verified","model":"gpt-6-sol","effort":"high"
                    }),
                    serde_json::json!({
                        "sequence":3,"actionId":"pair-production-action","atMs":3,
                        "type":"connector_verified"
                    }),
                    serde_json::json!({
                        "sequence":4,"actionId":"pair-production-action","atMs":4,
                        "type":"send_accepted"
                    }),
                    serde_json::json!({
                        "sequence":5,"actionId":"pair-production-action","atMs":5,
                        "type":"generation_complete","finalMarkdown":null,
                        "canonicalUrl":"https://chatgpt.com/c/pair-production"
                    }),
                ];
                let body = events
                    .into_iter()
                    .map(|event| format!("{event}\n"))
                    .collect::<String>();
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/x-ndjson")
                    .body(Body::from(body))
                    .unwrap()
            }),
        )
        .route(
            "/v1/actions/pair-production-action",
            get(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "actionId":"pair-production-action",
                    "state":"idle",
                    "sendAccepted":true,
                    "canonicalUrl":"https://chatgpt.com/c/pair-production",
                    "selected":{"model":"gpt-6-sol","effort":"high"},
                    "lastEventSequence":5,
                    "attention":null
                }))
                .into_response()
            }),
        );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mut app_config = default_config(project);
    app_config.memory.dir = Some(root.path().join("state").display().to_string());
    app_config.markdown_chat.enabled = true;
    app_config.markdown_chat.automation.enabled = true;
    app_config.markdown_chat.automation.connector_name = "Codexify".into();
    app_config.markdown_chat.automation.descriptor_path =
        write_companion_descriptor(root.path(), &endpoint)
            .display()
            .to_string();
    let task = ConversationIdentity::from_openai_session("pair-production-task").unwrap();
    let store = Arc::new(AgentAutomationStore::for_config(&app_config).unwrap());
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    store
        .set_selection(&task, "gpt-6-sol", "high", "catalog-1", "account-1", now_ms)
        .unwrap();
    let pairing = store.issue_pairing(&task, expected_url, 0, now_ms).unwrap();
    let executor = BrowserAutomationExecutor::new(app_config.markdown_chat.automation.clone());
    let execution = AutomationExecution {
        task_key: "c".repeat(64),
        owner_generation: 0,
        account_fingerprint: "account-1".into(),
        catalog_revision: "catalog-1".into(),
        activity_epoch: "pair-verification".into(),
        activity_sequence: pairing.generation,
        tool_call_count_at_start: 0,
        unread_message_ids: Vec::new(),
        transcript_revision: String::new(),
        pairing_commit: Some(PairingCommit::new(
            task.clone(),
            store.clone(),
            pairing.generation,
        )),
        request: codexify::agent_automation::ExecuteActionRequest {
            action_id: "pair-production-action".into(),
            idempotency_key: "pair-production-execute".into(),
            url: expected_url.into(),
            pairing_generation: pairing.generation,
            connector_name: "Codexify".into(),
            expected_account_fingerprint: "account-1".into(),
            model: None,
            effort: None,
            prompt: ActionPrompt::PairVerify {
                reference: pairing.reference,
            },
            expected_url: expected_url.into(),
            auto_approve_tool_calls: false,
            bypass_conversation_cooldown: false,
            retain_pairing_window: false,
            pairing_continuation_of: None,
            agent_ticket: None,
        },
    };
    let (events, mut events_rx) = mpsc::unbounded_channel();

    let snapshot = executor
        .execute(execution, events, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(snapshot.state, CompanionActionState::Idle);
    while events_rx.try_recv().is_ok() {}
    assert_eq!(
        store.load(&task).unwrap().pairing.canonical_url.as_deref(),
        Some(expected_url)
    );
    server.abort();
}

#[tokio::test]
async fn real_executor_skips_catalog_refresh_and_reconnects_a_dropped_stream() {
    let capability_calls = Arc::new(AtomicUsize::new(0));
    let capability_counter = capability_calls.clone();
    let stream_calls = Arc::new(AtomicUsize::new(0));
    let event_counter = stream_calls.clone();
    let status_calls = Arc::new(AtomicUsize::new(0));
    let status_counter = status_calls.clone();
    let app = Router::new()
        .route(
            "/v1/health",
            get(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "kind":"codexify-browser-companion",
                    "build":COMPANION_BUILD,
                    "status":"idle",
                    "authenticated":true,
                    "accountFingerprint":"account-1",
                    "activeActionId":null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/session/capabilities",
            post(move || {
                let capability_counter = capability_counter.clone();
                async move {
                    capability_counter.fetch_add(1, Ordering::SeqCst);
                    StatusCode::SERVICE_UNAVAILABLE
                }
            }),
        )
        .route(
            "/v1/actions/execute",
            post(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "actionId":"stream-reconnect-action",
                    "state":"queued",
                    "sendAccepted":false,
                    "canonicalUrl":null,
                    "selected":null,
                    "lastEventSequence":0,
                    "attention":null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/actions/stream-reconnect-action/events",
            get(move |headers: HeaderMap| {
                let event_counter = event_counter.clone();
                async move {
                    if !authorized(&headers) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    let call = event_counter.fetch_add(1, Ordering::SeqCst);
                    let body = if call == 0 {
                        concat!(
                            "{\"sequence\":1,\"actionId\":\"stream-reconnect-action\",\"atMs\":1,\"type\":\"send_accepted\"}\n",
                            "{\"sequence\":2,\"actionId\":\"stream-reconnect-action\",\"atMs\":2,\"type\":\"answer_delta\",\"markdown\":\"Recovered answer\",\"continuation\":false}\n",
                            "{\"sequence\":3"
                        )
                        .to_string()
                    } else {
                        concat!(
                            "{\"sequence\":3,\"actionId\":\"stream-reconnect-action\",\"atMs\":3,",
                            "\"type\":\"generation_complete\",\"finalMarkdown\":\"Recovered answer\",",
                            "\"canonicalUrl\":\"https://chatgpt.com/c/stream-reconnect\"}\n"
                        )
                        .to_string()
                    };
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/x-ndjson")
                        .body(Body::from(body))
                        .unwrap()
                }
            }),
        )
        .route(
            "/v1/actions/stream-reconnect-action",
            get(move |headers: HeaderMap| {
                let status_counter = status_counter.clone();
                async move {
                    if !authorized(&headers) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    let generating = status_counter.fetch_add(1, Ordering::SeqCst) == 0;
                    axum::Json(serde_json::json!({
                        "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                        "build":COMPANION_BUILD,
                        "actionId":"stream-reconnect-action",
                        "state":if generating { "generating" } else { "idle" },
                        "sendAccepted":true,
                        "canonicalUrl":"https://chatgpt.com/c/stream-reconnect",
                        "selected":{"model":"gpt-6-sol","effort":"high"},
                        "lastEventSequence":if generating { 2 } else { 3 },
                        "attention":null
                    }))
                    .into_response()
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let descriptor = write_companion_descriptor(root.path(), &endpoint);
    let executor = BrowserAutomationExecutor::new(codexify::markdown_chat::AgentAutomationConfig {
        enabled: true,
        descriptor_path: descriptor.display().to_string(),
        ..Default::default()
    });
    let execution = AutomationExecution {
        task_key: "d".repeat(64),
        owner_generation: 0,
        account_fingerprint: "account-1".into(),
        catalog_revision: "previously-cached-catalog".into(),
        activity_epoch: "epoch-1".into(),
        activity_sequence: 1,
        tool_call_count_at_start: 1,
        unread_message_ids: vec!["stream-reconnect-message".into()],
        transcript_revision: "revision-1".into(),
        pairing_commit: None,
        request: codexify::agent_automation::ExecuteActionRequest {
            action_id: "stream-reconnect-action".into(),
            idempotency_key: "stream-reconnect-idempotency".into(),
            url: "https://chatgpt.com/c/stream-reconnect".into(),
            pairing_generation: 1,
            connector_name: "Codexify".into(),
            expected_account_fingerprint: "account-1".into(),
            model: Some("gpt-6-sol".into()),
            effort: Some("high".into()),
            prompt: ActionPrompt::OfflineChat {
                batch_id: "stream-reconnect-batch".into(),
                messages: vec![codexify::agent_automation::CompanionUnreadMessage {
                    id: "stream-reconnect-message".into(),
                    markdown: "Recover the retained browser stream.".into(),
                }],
            },
            expected_url: "https://chatgpt.com/c/stream-reconnect".into(),
            auto_approve_tool_calls: false,
            bypass_conversation_cooldown: false,
            retain_pairing_window: false,
            pairing_continuation_of: None,
            agent_ticket: None,
        },
    };
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();

    let snapshot = executor
        .execute(execution, events_tx, CancellationToken::new())
        .await
        .unwrap();
    let mut kinds = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        kinds.push(event.kind);
    }

    assert_eq!(snapshot.state, CompanionActionState::Idle);
    assert_eq!(capability_calls.load(Ordering::SeqCst), 0);
    assert_eq!(stream_calls.load(Ordering::SeqCst), 2);
    assert_eq!(status_calls.load(Ordering::SeqCst), 2);
    assert!(matches!(
        kinds.last(),
        Some(CompanionEventKind::GenerationComplete {
            final_markdown: Some(markdown),
            ..
        }) if markdown == "Recovered answer"
    ));
    server.abort();
}

#[tokio::test]
async fn real_executor_replays_retained_prefix_from_an_idempotent_generating_receipt() {
    let app = Router::new()
        .route(
            "/v1/health",
            get(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "kind":"codexify-browser-companion",
                    "build":COMPANION_BUILD,
                    "status":"idle",
                    "authenticated":true,
                    "accountFingerprint":"account-1",
                    "activeActionId":null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/session/capabilities",
            post(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "accountFingerprint":"account-1",
                    "catalogRevision":"catalog-1",
                    "models":[{
                        "id":"gpt-6-sol",
                        "label":"GPT-6 Sol",
                        "efforts":[{"id":"high","label":"High","available":true,"reason":null}]
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/actions/execute",
            post(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "actionId":"generating-replay-action",
                    "state":"generating",
                    "sendAccepted":true,
                    "canonicalUrl":"https://chatgpt.com/c/generating-replay",
                    "selected":{"model":"gpt-6-sol","effort":"high"},
                    "lastEventSequence":2,
                    "attention":null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/actions/generating-replay-action/events",
            get(
                |headers: HeaderMap,
                 Query(query): Query<std::collections::HashMap<String, String>>| async move {
                    if !authorized(&headers) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    let body = if query.get("after").map(String::as_str) == Some("0") {
                        concat!(
                            "{\"sequence\":1,\"actionId\":\"generating-replay-action\",\"atMs\":1,\"type\":\"send_accepted\"}\n",
                            "{\"sequence\":2,\"actionId\":\"generating-replay-action\",\"atMs\":2,\"type\":\"answer_delta\",\"markdown\":\"Retained prefix\",\"continuation\":false}\n",
                            "{\"sequence\":3,\"actionId\":\"generating-replay-action\",\"atMs\":3,\"type\":\"answer_delta\",\"markdown\":\" and suffix\",\"continuation\":true}\n",
                            "{\"sequence\":4,\"actionId\":\"generating-replay-action\",\"atMs\":4,\"type\":\"generation_complete\",\"finalMarkdown\":null,\"canonicalUrl\":\"https://chatgpt.com/c/generating-replay\"}\n"
                        )
                    } else {
                        concat!(
                            "{\"sequence\":3,\"actionId\":\"generating-replay-action\",\"atMs\":3,\"type\":\"answer_delta\",\"markdown\":\" and suffix\",\"continuation\":true}\n",
                            "{\"sequence\":4,\"actionId\":\"generating-replay-action\",\"atMs\":4,\"type\":\"generation_complete\",\"finalMarkdown\":null,\"canonicalUrl\":\"https://chatgpt.com/c/generating-replay\"}\n"
                        )
                    };
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/x-ndjson")
                        .body(Body::from(body))
                        .unwrap()
                },
            ),
        )
        .route(
            "/v1/actions/generating-replay-action",
            get(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "actionId":"generating-replay-action",
                    "state":"idle",
                    "sendAccepted":true,
                    "canonicalUrl":"https://chatgpt.com/c/generating-replay",
                    "selected":{"model":"gpt-6-sol","effort":"high"},
                    "lastEventSequence":4,
                    "attention":null
                }))
                .into_response()
            }),
        );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let descriptor = write_companion_descriptor(root.path(), &endpoint);
    let executor = BrowserAutomationExecutor::new(codexify::markdown_chat::AgentAutomationConfig {
        enabled: true,
        descriptor_path: descriptor.display().to_string(),
        ..Default::default()
    });
    let execution = AutomationExecution {
        task_key: "f".repeat(64),
        owner_generation: 0,
        account_fingerprint: "account-1".into(),
        catalog_revision: "catalog-1".into(),
        activity_epoch: "epoch-1".into(),
        activity_sequence: 1,
        tool_call_count_at_start: 1,
        unread_message_ids: vec!["generating-replay-message".into()],
        transcript_revision: "revision-1".into(),
        pairing_commit: None,
        request: codexify::agent_automation::ExecuteActionRequest {
            action_id: "generating-replay-action".into(),
            idempotency_key: "generating-replay-idempotency".into(),
            url: "https://chatgpt.com/c/generating-replay".into(),
            pairing_generation: 1,
            connector_name: "Codexify".into(),
            expected_account_fingerprint: "account-1".into(),
            model: Some("gpt-6-sol".into()),
            effort: Some("high".into()),
            prompt: ActionPrompt::OfflineChat {
                batch_id: "generating-replay-batch".into(),
                messages: vec![codexify::agent_automation::CompanionUnreadMessage {
                    id: "generating-replay-message".into(),
                    markdown: "Recover an in-progress retained browser stream.".into(),
                }],
            },
            expected_url: "https://chatgpt.com/c/generating-replay".into(),
            auto_approve_tool_calls: false,
            bypass_conversation_cooldown: false,
            retain_pairing_window: false,
            pairing_continuation_of: None,
            agent_ticket: None,
        },
    };
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();

    let snapshot = executor
        .execute(execution, events_tx, CancellationToken::new())
        .await
        .unwrap();
    let mut answer = String::new();
    while let Ok(event) = events_rx.try_recv() {
        if let CompanionEventKind::AnswerDelta { markdown, .. } = event.kind {
            answer.push_str(&markdown);
        }
    }

    assert_eq!(snapshot.state, CompanionActionState::Idle);
    assert_eq!(answer, "Retained prefix and suffix");
    server.abort();
}

#[tokio::test]
async fn real_executor_reconnects_retained_events_from_an_idempotent_terminal_receipt() {
    let stream_calls = Arc::new(AtomicUsize::new(0));
    let event_counter = stream_calls.clone();
    let app = Router::new()
        .route(
            "/v1/health",
            get(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "kind":"codexify-browser-companion",
                    "build":COMPANION_BUILD,
                    "status":"idle",
                    "authenticated":true,
                    "accountFingerprint":"account-1",
                    "activeActionId":null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/session/capabilities",
            post(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "accountFingerprint":"account-1",
                    "catalogRevision":"catalog-1",
                    "models":[{
                        "id":"gpt-6-sol",
                        "label":"GPT-6 Sol",
                        "efforts":[{"id":"high","label":"High","available":true,"reason":null}]
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/actions/execute",
            post(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "actionId":"terminal-replay-action",
                    "state":"idle",
                    "sendAccepted":true,
                    "canonicalUrl":"https://chatgpt.com/c/terminal-replay",
                    "selected":{"model":"gpt-6-sol","effort":"high"},
                    "lastEventSequence":3,
                    "attention":null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/actions/terminal-replay-action/events",
            get(move |headers: HeaderMap| {
                let event_counter = event_counter.clone();
                async move {
                    if !authorized(&headers) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    let call = event_counter.fetch_add(1, Ordering::SeqCst);
                    let body = if call == 0 {
                        concat!(
                            "{\"sequence\":1,\"actionId\":\"terminal-replay-action\",\"atMs\":1,\"type\":\"send_accepted\"}\n",
                            "{\"sequence\":2,\"actionId\":\"terminal-replay-action\",\"atMs\":2,\"type\":\"answer_delta\",\"markdown\":\"Terminal replay answer\",\"continuation\":false}\n",
                            "{\"sequence\":3"
                        )
                    } else {
                        concat!(
                            "{\"sequence\":3,\"actionId\":\"terminal-replay-action\",\"atMs\":3,",
                            "\"type\":\"generation_complete\",\"finalMarkdown\":\"Terminal replay answer\",",
                            "\"canonicalUrl\":\"https://chatgpt.com/c/terminal-replay\"}\n"
                        )
                    };
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/x-ndjson")
                        .body(Body::from(body))
                        .unwrap()
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let descriptor = write_companion_descriptor(root.path(), &endpoint);
    let executor = BrowserAutomationExecutor::new(codexify::markdown_chat::AgentAutomationConfig {
        enabled: true,
        descriptor_path: descriptor.display().to_string(),
        ..Default::default()
    });
    let execution = AutomationExecution {
        task_key: "e".repeat(64),
        owner_generation: 0,
        account_fingerprint: "account-1".into(),
        catalog_revision: "catalog-1".into(),
        activity_epoch: "epoch-1".into(),
        activity_sequence: 1,
        tool_call_count_at_start: 1,
        unread_message_ids: vec!["terminal-replay-message".into()],
        transcript_revision: "revision-1".into(),
        pairing_commit: None,
        request: codexify::agent_automation::ExecuteActionRequest {
            action_id: "terminal-replay-action".into(),
            idempotency_key: "terminal-replay-idempotency".into(),
            url: "https://chatgpt.com/c/terminal-replay".into(),
            pairing_generation: 1,
            connector_name: "Codexify".into(),
            expected_account_fingerprint: "account-1".into(),
            model: Some("gpt-6-sol".into()),
            effort: Some("high".into()),
            prompt: ActionPrompt::OfflineChat {
                batch_id: "terminal-replay-batch".into(),
                messages: vec![codexify::agent_automation::CompanionUnreadMessage {
                    id: "terminal-replay-message".into(),
                    markdown: "Recover an already completed action.".into(),
                }],
            },
            expected_url: "https://chatgpt.com/c/terminal-replay".into(),
            auto_approve_tool_calls: false,
            bypass_conversation_cooldown: false,
            retain_pairing_window: false,
            pairing_continuation_of: None,
            agent_ticket: None,
        },
    };
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();

    let snapshot = executor
        .execute(execution, events_tx, CancellationToken::new())
        .await
        .unwrap();
    let mut kinds = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        kinds.push(event.kind);
    }

    assert_eq!(snapshot.state, CompanionActionState::Idle);
    assert_eq!(stream_calls.load(Ordering::SeqCst), 2);
    assert!(matches!(
        kinds.last(),
        Some(CompanionEventKind::GenerationComplete {
            final_markdown: Some(markdown),
            ..
        }) if markdown == "Terminal replay answer"
    ));
    server.abort();
}

#[tokio::test]
async fn failed_pre_send_action_is_not_retried_and_chat_remains_usable() {
    let fixture = fixture().await;
    let executor = Arc::new(PreSendFailureExecutor::default());
    let supervisor = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        executor.clone(),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "pre-send-failure".into(),
            "Keep this message pending after browser failure.".into(),
        )
        .await
        .unwrap();

    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert!(
        supervisor
            .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
            .await
            .is_err()
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.store.load(&fixture.task).unwrap().action.status,
        AutomationActionStatus::Failed
    );

    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 2)
        .await
        .unwrap();
    assert_eq!(supervisor.queued_count().await, 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert!(
        fixture
            .chat
            .read(false)
            .await
            .unwrap()
            .text
            .contains("Keep this message pending after browser failure.")
    );
    fixture
        .chat
        .append_user("after-failure".into(), "Ordinary chat still works.".into())
        .await
        .unwrap();
    assert!(
        fixture
            .chat
            .read(false)
            .await
            .unwrap()
            .text
            .contains("Ordinary chat still works.")
    );
}

#[tokio::test]
async fn continuing_an_accepted_combined_turn_keeps_its_messages_out_of_the_browser_retry() {
    use codexify::agent_automation::OwnerGoalCommand;
    let f = fixture().await;
    f.store
        .apply_owner_goal_command(
            &f.task,
            "combined-goal",
            OwnerGoalCommand::Set {
                markdown: "Keep working".into(),
            },
            1000,
        )
        .unwrap();
    f.chat
        .append_user(
            "accepted-user-message".into(),
            "Do not send this twice".into(),
        )
        .await
        .unwrap();
    f.supervisor.scan_once(1001).await.unwrap();
    f.supervisor.run_next(1002).await.unwrap().unwrap();
    let saved = f.store.load(&f.task).unwrap();
    assert_eq!(saved.retry_mode(), Some("continue_goal"));
    assert!(saved.action.messages_fenced);
    let action_id = saved.action.id.unwrap();
    f.supervisor
        .retry_failed_action(&f.task, &action_id, 1003)
        .await
        .unwrap();
    assert_eq!(
        f.store.load(&f.task).unwrap().chat_mirror,
        saved.chat_mirror
    );
    f.supervisor.run_next(1004).await.unwrap().unwrap();
    let executions = f.executor.executions.lock().unwrap();
    assert!(matches!(
        executions[0].request.prompt,
        ActionPrompt::OfflineCombined { .. }
    ));
    assert!(matches!(
        executions[1].request.prompt,
        ActionPrompt::OfflineGoal { .. }
    ));
    assert_eq!(f.store.load(&f.task).unwrap().goal.unwrap().turn_count, 3);
}

#[tokio::test]
async fn explicit_retry_rearms_only_a_pre_send_failed_action() {
    let fixture = fixture().await;
    let executor = Arc::new(PreSendFailureExecutor::default());
    let supervisor = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        executor.clone(),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "retry-pre-send".into(),
            "Retry this only after explicit owner action.".into(),
        )
        .await
        .unwrap();
    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    let failed_action = fixture
        .store
        .load(&fixture.task)
        .unwrap()
        .action
        .id
        .unwrap();
    assert!(
        supervisor
            .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
            .await
            .is_err()
    );

    let retried = supervisor
        .retry_failed_action(
            &fixture.task,
            &failed_action,
            1_000 + AUTOMATION_OFFLINE_AFTER_MS + 2,
        )
        .await
        .unwrap();
    assert_eq!(retried.status, AutomationActionStatus::Queued);
    assert_ne!(retried.id.as_deref(), Some(failed_action.as_str()));
    assert_eq!(supervisor.queued_count().await, 1);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn queued_pair_verification_can_be_cancelled_without_replaying_its_reference() {
    let fixture = fixture().await;
    let replacement = fixture
        .store
        .issue_pairing(&fixture.task, "https://chatgpt.com/c/replacement", 0, 1_000)
        .unwrap();
    fixture
        .supervisor
        .queue_pair_verification(
            &fixture.task,
            &replacement.reference,
            replacement.generation,
            1_001,
        )
        .await
        .unwrap();
    let action_id = fixture
        .store
        .load(&fixture.task)
        .unwrap()
        .action
        .id
        .unwrap();

    let cancelled = fixture
        .supervisor
        .cancel_action(&fixture.task, &action_id, 1_002)
        .await
        .unwrap();

    assert_eq!(cancelled.status, AutomationActionStatus::Unpaired);
    assert_eq!(fixture.supervisor.queued_count().await, 0);
    let state = fixture.store.load(&fixture.task).unwrap();
    assert!(state.pairing.candidate_url.is_none());
    assert!(state.pairing.pending.is_none());
    assert_eq!(fixture.executor.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn terminal_failure_after_send_acceptance_is_ambiguous_and_never_retryable() {
    let fixture = fixture().await;
    let supervisor = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        Arc::new(AcceptedTerminalFailureExecutor),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "accepted-terminal-failure".into(),
            "Do not replay this message after an accepted failure.".into(),
        )
        .await
        .unwrap();

    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap()
        .unwrap();

    let state = fixture.store.load(&fixture.task).unwrap();
    assert_eq!(state.action.status, AutomationActionStatus::Ambiguous);
    assert_eq!(
        state.chat_mirror.message_ids,
        vec!["accepted-terminal-failure"]
    );
    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 2)
        .await
        .unwrap();
    assert_eq!(supervisor.queued_count().await, 0);
    assert!(
        fixture
            .chat
            .read(false)
            .await
            .unwrap()
            .text
            .contains("Do not replay this message")
    );
}

#[tokio::test]
async fn delivered_send_acceptance_outweighs_a_stale_pre_send_error_flag() {
    let fixture = fixture().await;
    let supervisor = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        Arc::new(AcceptedEventThenTransportErrorExecutor),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "accepted-event-before-error".into(),
            "Do not replay after the event stream proves Send acceptance.".into(),
        )
        .await
        .unwrap();
    supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();

    assert!(
        supervisor
            .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
            .await
            .is_err()
    );

    let state = fixture.store.load(&fixture.task).unwrap();
    assert_eq!(state.action.status, AutomationActionStatus::Ambiguous);
    assert_eq!(
        state.chat_mirror.message_ids,
        vec!["accepted-event-before-error"]
    );
}

#[tokio::test]
async fn persisted_in_flight_action_is_not_replayed_after_supervisor_restart() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "restart-message".into(),
            "Do not replay an action whose Send state is uncertain.".into(),
        )
        .await
        .unwrap();
    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    let action_id = fixture
        .store
        .load(&fixture.task)
        .unwrap()
        .action
        .id
        .unwrap();
    fixture
        .store
        .update_action_status(
            &fixture.task,
            &action_id,
            AutomationActionStatus::Navigating,
            None,
            None,
            1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1,
        )
        .unwrap();

    let restarted_executor = Arc::new(FakeExecutor::default());
    let restarted = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        restarted_executor.clone(),
    );
    restarted
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    restarted
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 2)
        .await
        .unwrap();

    assert_eq!(restarted.queued_count().await, 0);
    assert_eq!(restarted_executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.store.load(&fixture.task).unwrap().action.status,
        AutomationActionStatus::Navigating
    );
}

#[tokio::test]
async fn persisted_queued_pairing_is_revoked_after_supervisor_restart() {
    let fixture = fixture().await;
    let pairing = fixture
        .store
        .issue_pairing(
            &fixture.task,
            "https://chatgpt.com/c/restart-pairing",
            0,
            1_000,
        )
        .unwrap();
    fixture
        .store
        .queue_action(
            &fixture.task,
            "restart-pairing-action",
            codexify::agent_automation::AutomationActionKind::PairVerify,
            pairing.generation,
            None,
            1_001,
        )
        .unwrap();

    let restarted = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        Arc::new(FakeExecutor::default()),
    );
    restarted
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();

    let state = fixture.store.load(&fixture.task).unwrap();
    assert!(state.pairing.pending.is_none());
    assert_eq!(state.action.status, AutomationActionStatus::Unverified);
    assert_eq!(restarted.queued_count().await, 0);
    assert!(
        fixture
            .store
            .verify_pairing_reference(&fixture.task, 0, &pairing.reference, 1_002)
            .is_err()
    );
}

#[tokio::test]
async fn persisted_queued_chat_without_remaining_work_is_cleared_after_restart() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "restart-consumed-message".into(),
            "Consume me before the queued action can run.".into(),
        )
        .await
        .unwrap();
    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(fixture.supervisor.queued_count().await, 1);
    fixture.chat.read(true).await.unwrap();

    let restarted = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        Arc::new(FakeExecutor::default()),
    );
    restarted
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    restarted
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap();

    let state = fixture.store.load(&fixture.task).unwrap();
    assert_eq!(state.action.status, AutomationActionStatus::Idle);
    assert!(state.action.id.is_none());
    assert_eq!(restarted.queued_count().await, 0);
}

#[tokio::test]
async fn unread_batches_are_split_without_truncating_messages() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    for index in 0..130 {
        fixture
            .chat
            .append_user(
                format!("split-{index:03}"),
                format!("Message {index:03} must remain intact."),
            )
            .await
            .unwrap();
    }
    fixture.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::GenerationComplete {
            final_markdown: None,
            canonical_url: Some("https://chatgpt.com/c/example".into()),
        },
    ]);

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    fixture
        .supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap()
        .unwrap();

    {
        let executions = fixture.executor.executions.lock().unwrap();
        let ActionPrompt::OfflineChat { messages, .. } = &executions[0].request.prompt else {
            panic!("expected an offline chat action");
        };
        assert_eq!(messages.len(), 128);
        assert_message_body(&messages[0].markdown, "Message 000 must remain intact.");
        assert_message_body(&messages[127].markdown, "Message 127 must remain intact.");
    }
    assert_eq!(
        fixture
            .store
            .load(&fixture.task)
            .unwrap()
            .chat_mirror
            .message_ids
            .len(),
        128
    );

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 2)
        .await
        .unwrap();
    assert_eq!(fixture.supervisor.queued_count().await, 1);
}

#[tokio::test]
async fn unread_batches_account_for_xml_escaping_before_queueing() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    let escaped_heavy = "<".repeat(40 * 1024);
    for id in ["escaped-first", "escaped-second"] {
        fixture
            .chat
            .append_user(id.into(), escaped_heavy.clone())
            .await
            .unwrap();
    }

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    fixture
        .supervisor
        .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
        .await
        .unwrap()
        .unwrap();

    let executions = fixture.executor.executions.lock().unwrap();
    let ActionPrompt::OfflineChat { messages, .. } = &executions[0].request.prompt else {
        panic!("expected an offline chat action");
    };
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, "escaped-first");
    assert_message_body(&messages[0].markdown, &escaped_heavy);
}

#[tokio::test]
async fn one_oversized_unread_message_fails_without_queueing_or_truncation() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    let message = "x".repeat(256 * 1024 + 1);
    fixture
        .chat
        .append_user("oversized-browser-message".into(), message.clone())
        .await
        .unwrap();

    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();

    assert_eq!(fixture.supervisor.queued_count().await, 0);
    let state = fixture.store.load(&fixture.task).unwrap();
    assert_eq!(state.action.status, AutomationActionStatus::Failed);
    assert!(
        state
            .action
            .last_error
            .as_deref()
            .unwrap()
            .contains("too large")
    );
    assert_eq!(
        fixture.chat.unread_snapshot().await.unwrap().messages[0].markdown,
        message
    );
}

#[tokio::test]
async fn stale_priority_action_is_skipped_without_blocking_the_next_ready_action() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "ready-chat".into(),
            "This ready action must still execute.".into(),
        )
        .await
        .unwrap();
    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();

    let pair_task = ConversationIdentity::from_openai_session("stale-pair-task").unwrap();
    let pair_chat = MarkdownChatStore::default()
        .chat(&fixture.config, Some(&pair_task), &SessionState::new())
        .unwrap();
    pair_chat.ensure().await.unwrap();
    fixture
        .store
        .set_selection(
            &pair_task,
            "gpt-6-sol",
            "high",
            "catalog-1",
            "account-1",
            100,
        )
        .unwrap();
    let pairing = fixture
        .store
        .issue_pairing(&pair_task, "https://chatgpt.com/c/stale-pair", 0, 100)
        .unwrap();
    fixture
        .supervisor
        .register_task(AutomationTaskHandle::new(
            pair_task.clone(),
            0,
            fixture.store.clone(),
            pair_chat,
        ))
        .await
        .unwrap();
    fixture
        .supervisor
        .queue_pair_verification(&pair_task, &pairing.reference, pairing.generation, 101)
        .await
        .unwrap();
    fixture
        .store
        .issue_pairing(&pair_task, "https://chatgpt.com/c/replacement-pair", 0, 102)
        .unwrap();
    fixture.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::GenerationComplete {
            final_markdown: None,
            canonical_url: Some("https://chatgpt.com/c/example".into()),
        },
    ]);

    assert!(
        fixture
            .supervisor
            .run_next(1_000 + AUTOMATION_OFFLINE_AFTER_MS + 1)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(fixture.executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.supervisor.queued_count().await, 0);
    let executions = fixture.executor.executions.lock().unwrap();
    assert!(matches!(
        executions[0].request.prompt,
        ActionPrompt::OfflineChat { .. }
    ));
}

#[tokio::test]
async fn real_executor_accepts_chat_id_aliases_but_rejects_different_conversations() {
    let reply = Arc::new(Mutex::new(String::new()));
    let app = Router::new()
        .route(
            "/v1/health",
            get(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "kind":"codexify-browser-companion",
                    "build":COMPANION_BUILD,
                    "status":"idle",
                    "authenticated":true,
                    "accountFingerprint":"account-1",
                    "activeActionId":null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/session/capabilities",
            post(|headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "accountFingerprint":"account-1",
                    "catalogRevision":"catalog-1",
                    "models":[{"id":"gpt-6-sol","label":"GPT-6 Sol","efforts":[{"id":"high","label":"High","available":true,"reason":null}]}]
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/actions/execute",
            post(|State(reply): State<Arc<Mutex<String>>>, headers: HeaderMap| async move {
                if !authorized(&headers) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,
                    "build":COMPANION_BUILD,
                    "actionId":"canonical-check",
                    "state":"idle",
                    "sendAccepted":true,
                    "canonicalUrl":reply.lock().unwrap().clone(),
                    "selected":{"model":"gpt-6-sol","effort":"high"},
                    "lastEventSequence":0,
                    "attention":null
                }))
                .into_response()
            }),
        ).with_state(reply.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let descriptor = write_companion_descriptor(root.path(), &endpoint);
    let config = codexify::markdown_chat::AgentAutomationConfig {
        enabled: true,
        descriptor_path: descriptor.display().to_string(),
        ..Default::default()
    };
    let executor = BrowserAutomationExecutor::new(config);
    for (prompt, unread_message_ids) in [
        (
            ActionPrompt::OfflineChat {
                batch_id: "canonical-batch".into(),
                messages: vec![codexify::agent_automation::CompanionUnreadMessage {
                    id: "canonical-message".into(),
                    markdown: "Check the canonical URL.".into(),
                }],
            },
            vec!["canonical-message".into()],
        ),
        (
            ActionPrompt::PairVerify {
                reference: "A".repeat(43),
            },
            Vec::new(),
        ),
    ] {
        let canonical = "https://chatgpt.com/g/g-p-0123456789abcdef0123456789abcdef/c/thread";
        let friendly =
            "https://chatgpt.com/g/g-p-0123456789abcdef0123456789abcdef-original-project/c/thread";
        for (expected, actual, accepted) in [
            (friendly, canonical, true),
            (friendly, friendly, true),
            (friendly, "https://chatgpt.com/c/thread", true),
            (
                friendly,
                "https://chatgpt.com/g/g-custom-gpt/c/thread",
                true,
            ),
            (
                friendly,
                "https://chatgpt.com/g/g-p-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/c/thread",
                true,
            ),
            (
                friendly,
                "https://chatgpt.com/g/g-p-0123456789abcdef0123456789abcdef/c/different",
                false,
            ),
            (
                "https://chatgpt.com/c/expected-conversation",
                "https://chatgpt.com/c/different-conversation",
                false,
            ),
        ] {
            *reply.lock().unwrap() = actual.into();
            let execution = AutomationExecution {
                task_key: "b".repeat(64),
                owner_generation: 0,
                account_fingerprint: "account-1".into(),
                catalog_revision: "catalog-1".into(),
                activity_epoch: "epoch-1".into(),
                activity_sequence: 1,
                tool_call_count_at_start: 1,
                unread_message_ids: unread_message_ids.clone(),
                transcript_revision: "revision-1".into(),
                pairing_commit: None,
                request: codexify::agent_automation::ExecuteActionRequest {
                    action_id: "canonical-check".into(),
                    idempotency_key: "canonical-idempotency".into(),
                    url: expected.into(),
                    pairing_generation: 1,
                    connector_name: "Codexify".into(),
                    expected_account_fingerprint: "account-1".into(),
                    model: (!matches!(&prompt, ActionPrompt::PairVerify { .. }))
                        .then(|| "gpt-6-sol".into()),
                    effort: (!matches!(&prompt, ActionPrompt::PairVerify { .. }))
                        .then(|| "high".into()),
                    prompt: prompt.clone(),
                    expected_url: expected.into(),
                    auto_approve_tool_calls: false,
                    bypass_conversation_cooldown: false,
                    retain_pairing_window: false,
                    pairing_continuation_of: None,
                    agent_ticket: None,
                },
            };
            let (events, _) = mpsc::unbounded_channel();

            let result = executor
                .execute(execution, events, CancellationToken::new())
                .await;
            if accepted {
                assert_eq!(result.unwrap().canonical_url.as_deref(), Some(actual));
            } else {
                let error = result.err().unwrap();
                assert!(error.send_accepted);
                assert!(error.message.contains("conversation"));
            }
        }
    }
    server.abort();
}

#[tokio::test]
async fn explicit_pair_verification_outranks_pending_offline_chat() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .chat
        .append_user(
            "lower-priority-chat".into(),
            "This can wait for pairing.".into(),
        )
        .await
        .unwrap();
    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();

    let pair_task = ConversationIdentity::from_openai_session("pair-priority-task").unwrap();
    let pair_chat = MarkdownChatStore::default()
        .chat(&fixture.config, Some(&pair_task), &SessionState::new())
        .unwrap();
    pair_chat.ensure().await.unwrap();
    fixture
        .store
        .set_selection(
            &pair_task,
            "gpt-6-sol",
            "high",
            "catalog-1",
            "account-1",
            100,
        )
        .unwrap();
    let pairing = fixture
        .store
        .issue_pairing(&pair_task, "https://chatgpt.com/c/pair-priority", 0, 100)
        .unwrap();
    fixture
        .supervisor
        .register_task(AutomationTaskHandle::new(
            pair_task.clone(),
            0,
            fixture.store.clone(),
            pair_chat,
        ))
        .await
        .unwrap();

    fixture
        .supervisor
        .queue_pair_verification(&pair_task, &pairing.reference, pairing.generation, 101)
        .await
        .unwrap();

    assert_eq!(fixture.supervisor.queued_count().await, 2);
    assert_eq!(
        fixture.supervisor.next_queued_kind().await,
        Some(codexify::agent_automation::AutomationActionKind::PairVerify)
    );
    let state = fixture.store.load(&pair_task).unwrap();
    assert_eq!(
        state.action.kind,
        Some(codexify::agent_automation::AutomationActionKind::PairVerify)
    );
    assert_eq!(state.action.status, AutomationActionStatus::Queued);
}

#[tokio::test]
async fn pair_verification_records_canonical_url_without_polluting_chat() {
    let fixture = fixture().await;
    let pair_task = ConversationIdentity::from_openai_session("pair-execution-task").unwrap();
    let pair_chat = MarkdownChatStore::default()
        .chat(&fixture.config, Some(&pair_task), &SessionState::new())
        .unwrap();
    pair_chat.ensure().await.unwrap();
    fixture
        .store
        .set_selection(
            &pair_task,
            "gpt-6-sol",
            "high",
            "catalog-1",
            "account-1",
            100,
        )
        .unwrap();
    let pairing = fixture
        .store
        .issue_pairing(&pair_task, "https://chatgpt.com/c/pair-execution", 0, 100)
        .unwrap();
    fixture
        .supervisor
        .register_task(AutomationTaskHandle::new(
            pair_task.clone(),
            0,
            fixture.store.clone(),
            pair_chat.clone(),
        ))
        .await
        .unwrap();
    fixture
        .executor
        .events
        .lock()
        .unwrap()
        .push(CompanionEventKind::GenerationComplete {
            final_markdown: Some("Pairing setup response.".into()),
            canonical_url: Some("https://chatgpt.com/c/pair-execution".into()),
        });
    fixture
        .supervisor
        .queue_pair_verification(&pair_task, &pairing.reference, pairing.generation, 101)
        .await
        .unwrap();

    fixture.supervisor.run_next(102).await.unwrap().unwrap();

    let state = fixture.store.load(&pair_task).unwrap();
    assert_eq!(
        state.pairing.canonical_url.as_deref(),
        Some("https://chatgpt.com/c/pair-execution")
    );
    assert!(state.pairing.verified_owner_generation.is_none());
    assert_eq!(state.action.status, AutomationActionStatus::Unverified);
    assert!(
        pair_chat
            .clean_markdown_transcript()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn pair_verification_may_finish_before_the_browser_response_settles() {
    let fixture = fixture().await;
    let pair_task = ConversationIdentity::from_openai_session("pair-tool-race-task").unwrap();
    let pair_chat = MarkdownChatStore::default()
        .chat(&fixture.config, Some(&pair_task), &SessionState::new())
        .unwrap();
    pair_chat.ensure().await.unwrap();
    fixture
        .store
        .set_selection(
            &pair_task,
            "gpt-6-sol",
            "high",
            "catalog-1",
            "account-1",
            100,
        )
        .unwrap();
    let pairing = fixture
        .store
        .issue_pairing(&pair_task, "https://chatgpt.com/c/pair-tool-race", 0, 100)
        .unwrap();
    let executor = Arc::new(PairVerifyingExecutor {
        store: fixture.store.clone(),
        task: pair_task.clone(),
        generation: pairing.generation,
        reference: pairing.reference.clone(),
    });
    let supervisor =
        AgentAutomationSupervisor::new(fixture.config.markdown_chat.automation.clone(), executor);
    supervisor
        .register_task(AutomationTaskHandle::new(
            pair_task.clone(),
            0,
            fixture.store.clone(),
            pair_chat,
        ))
        .await
        .unwrap();
    supervisor
        .queue_pair_verification(&pair_task, &pairing.reference, pairing.generation, 101)
        .await
        .unwrap();

    supervisor.run_next(104).await.unwrap().unwrap();

    let state = fixture.store.load(&pair_task).unwrap();
    assert_eq!(state.pairing.verified_owner_generation, Some(0));
    assert_eq!(state.action.status, AutomationActionStatus::Idle);
}

#[tokio::test]
async fn pairing_failure_before_send_preserves_the_browser_error_and_revokes_the_reference() {
    struct TerminalFailureExecutor;
    #[async_trait]
    impl AutomationExecutor for TerminalFailureExecutor {
        async fn execute(
            &self,
            execution: AutomationExecution,
            events: mpsc::UnboundedSender<CompanionEvent>,
            _cancellation: CancellationToken,
        ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
            let message = "ChatGPT did not retain the selected connector";
            events
                .send(CompanionEvent {
                    sequence: 1,
                    action_id: execution.request.action_id.clone(),
                    at_ms: 102,
                    kind: CompanionEventKind::Failed {
                        code: "connector_unavailable".into(),
                        message: message.into(),
                    },
                })
                .unwrap();
            let mut snapshot = snapshot_for(execution);
            snapshot.state = CompanionActionState::Failed;
            snapshot.send_accepted = false;
            snapshot.canonical_url = None;
            snapshot.attention = Some(codexify::agent_automation::CompanionAttention {
                code: "connector_unavailable".into(),
                message: Some(message.into()),
            });
            Ok(snapshot)
        }
    }
    let fixture = fixture().await;
    let supervisor = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        Arc::new(TerminalFailureExecutor),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    let issue = fixture
        .store
        .issue_pairing(&fixture.task, "https://chatgpt.com/c/example", 0, 100)
        .unwrap();
    supervisor
        .queue_pair_verification(&fixture.task, &issue.reference, issue.generation, 101)
        .await
        .unwrap();
    assert_eq!(
        supervisor.run_next(104).await.err().unwrap(),
        "ChatGPT did not retain the selected connector"
    );
    let state = fixture.store.load(&fixture.task).unwrap();
    assert!(state.pairing.pending.is_none());
    assert_eq!(state.action.status, AutomationActionStatus::Failed);
    assert_eq!(
        state.action.last_error.as_deref(),
        Some("ChatGPT did not retain the selected connector")
    );
}

#[tokio::test]
async fn valid_pairing_survives_a_browser_failure_before_terminal_reconciliation() {
    let fixture = fixture().await;
    let pair_task = ConversationIdentity::from_openai_session("pair-failed-after-tool").unwrap();
    let pair_chat = MarkdownChatStore::default()
        .chat(&fixture.config, Some(&pair_task), &SessionState::new())
        .unwrap();
    pair_chat.ensure().await.unwrap();
    fixture
        .store
        .set_selection(
            &pair_task,
            "gpt-6-sol",
            "high",
            "catalog-1",
            "account-1",
            100,
        )
        .unwrap();
    let pairing = fixture
        .store
        .issue_pairing(
            &pair_task,
            "https://chatgpt.com/c/pair-failed-after-tool",
            0,
            100,
        )
        .unwrap();
    let executor = Arc::new(PairVerifyingFailureExecutor {
        store: fixture.store.clone(),
        task: pair_task.clone(),
        generation: pairing.generation,
        reference: pairing.reference.clone(),
    });
    let supervisor =
        AgentAutomationSupervisor::new(fixture.config.markdown_chat.automation.clone(), executor);
    supervisor
        .register_task(AutomationTaskHandle::new(
            pair_task.clone(),
            0,
            fixture.store.clone(),
            pair_chat,
        ))
        .await
        .unwrap();
    supervisor
        .queue_pair_verification(&pair_task, &pairing.reference, pairing.generation, 101)
        .await
        .unwrap();

    assert!(supervisor.run_next(104).await.is_err());

    let state = fixture.store.load(&pair_task).unwrap();
    assert!(state.pairing.pending.is_none());
    assert_eq!(state.pairing.verified_owner_generation, Some(0));
    assert!(state.pairing.verified_at_ms.is_some());
    assert_eq!(state.action.status, AutomationActionStatus::Idle);
    assert!(state.action.last_error.is_none());
}

#[tokio::test]
async fn background_worker_scans_notified_tasks_and_runs_the_global_queue() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1).await.unwrap();
    fixture
        .chat
        .append_user(
            "worker-message".into(),
            "Wake the background worker.".into(),
        )
        .await
        .unwrap();
    fixture.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::GenerationComplete {
            final_markdown: None,
            canonical_url: Some("https://chatgpt.com/c/example".into()),
        },
    ]);
    let worker = fixture.supervisor.start();
    fixture.supervisor.notify();

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while fixture.executor.calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    assert_eq!(fixture.executor.calls.load(Ordering::SeqCst), 1);
    drop(worker);
}

#[tokio::test]
async fn offline_message_without_model_override_or_previous_agent_call_is_sent_once() {
    let fixture = fixture().await;
    fixture.store.clear_selection(&fixture.task).unwrap();
    fixture
        .chat
        .append_user("fresh-message".into(), "Continue this conversation.".into())
        .await
        .unwrap();
    fixture.supervisor.scan_once(1_000).await.unwrap();
    assert_eq!(fixture.supervisor.queued_count().await, 1);
    fixture.supervisor.run_next(1_001).await.unwrap().unwrap();
    {
        let executions = fixture.executor.executions.lock().unwrap();
        assert_eq!(executions.len(), 1);
        assert!(executions[0].request.model.is_none());
        assert!(executions[0].request.effort.is_none());
        assert_eq!(
            executions[0].request.expected_account_fingerprint,
            "account-1"
        );
    }
    fixture.supervisor.scan_once(1_002).await.unwrap();
    assert_eq!(fixture.supervisor.queued_count().await, 0);
}

#[tokio::test]
async fn setting_an_owner_goal_while_offline_starts_and_queues_it_without_a_model_override() {
    let fixture = fixture().await;
    fixture.store.clear_selection(&fixture.task).unwrap();
    let receipt = fixture
        .store
        .apply_owner_goal_command(
            &fixture.task,
            "start-fresh-goal",
            codexify::agent_automation::OwnerGoalCommand::Set {
                markdown: "Complete the requested goal.".into(),
            },
            1_000,
        )
        .unwrap();
    assert_eq!(
        receipt.goal.unwrap().status,
        codexify::agent_automation::GoalStatus::Active
    );
    fixture.supervisor.scan_once(1_001).await.unwrap();
    assert_eq!(
        fixture.supervisor.next_queued_kind().await,
        Some(codexify::agent_automation::AutomationActionKind::OfflineGoal)
    );
    fixture.supervisor.run_next(1_002).await.unwrap().unwrap();
    assert!(
        fixture.executor.executions.lock().unwrap()[0]
            .request
            .model
            .is_none()
    );
}

#[tokio::test]
async fn an_active_goal_is_queued_when_the_agent_later_goes_offline() {
    let fixture = fixture().await;
    fixture.chat.record_agent_call(1_000).await.unwrap();
    fixture
        .store
        .apply_owner_goal_command(
            &fixture.task,
            "goal-before-offline",
            codexify::agent_automation::OwnerGoalCommand::Set {
                markdown: "Resume when the agent goes offline.".into(),
            },
            1_001,
        )
        .unwrap();
    fixture.supervisor.scan_once(1_002).await.unwrap();
    assert_eq!(fixture.supervisor.queued_count().await, 0);
    fixture
        .supervisor
        .scan_once(1_000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(
        fixture.supervisor.next_queued_kind().await,
        Some(codexify::agent_automation::AutomationActionKind::OfflineGoal)
    );
}

#[tokio::test]
async fn interrupted_pair_navigation_is_revoked_and_reported_without_replaying_it() {
    let fixture = fixture().await;
    let issue = fixture
        .store
        .issue_pairing(
            &fixture.task,
            "https://chatgpt.com/c/interrupted-pair",
            0,
            1_000,
        )
        .unwrap();
    fixture
        .store
        .queue_action(
            &fixture.task,
            "interrupted-pair",
            codexify::agent_automation::AutomationActionKind::PairVerify,
            issue.generation,
            None,
            1_001,
        )
        .unwrap();
    fixture
        .store
        .update_action_status(
            &fixture.task,
            "interrupted-pair",
            AutomationActionStatus::Navigating,
            None,
            None,
            1_002,
        )
        .unwrap();
    let restarted = AgentAutomationSupervisor::new(
        fixture.config.markdown_chat.automation.clone(),
        fixture.executor.clone(),
    );
    restarted
        .register_task(AutomationTaskHandle::new(
            fixture.task.clone(),
            0,
            fixture.store.clone(),
            fixture.chat.clone(),
        ))
        .await
        .unwrap();
    let state = fixture.store.load(&fixture.task).unwrap();
    assert_eq!(state.action.status, AutomationActionStatus::Unverified);
    assert!(state.action.last_error.unwrap().contains("interrupted"));
    assert!(state.pairing.pending.is_none());
    assert_eq!(restarted.queued_count().await, 0);
    assert_eq!(fixture.executor.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stopping_suppresses_wakes_and_messages_never_toggle_the_goal() {
    use codexify::agent_automation::{AutomationActionKind, GoalStatus, OwnerGoalCommand};
    let f = fixture().await;
    assert!(
        f.store
            .pause_goal_for_agent_stop(&f.task, "stop-no-goal", 999)
            .unwrap()
            .goal
            .is_none()
    );
    f.store
        .apply_owner_goal_command(
            &f.task,
            "set-stop-goal",
            OwnerGoalCommand::Set {
                markdown: "Finish the goal".into(),
            },
            1000,
        )
        .unwrap();
    f.chat.record_agent_call(1000).await.unwrap();
    f.chat
        .append_user(
            "message-while-enabled".into(),
            "An extra instruction".into(),
        )
        .await
        .unwrap();
    assert_eq!(
        f.store.load(&f.task).unwrap().goal.unwrap().status,
        GoalStatus::Active
    );
    f.store
        .pause_goal_for_agent_stop(&f.task, "stop-agent", 1001)
        .unwrap();
    f.chat
        .request_agent_stop("stop-agent".into())
        .await
        .unwrap();
    f.chat.record_agent_call(1002).await.unwrap();
    f.supervisor
        .scan_once(1002 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    assert_eq!(
        f.store.load(&f.task).unwrap().goal.unwrap().status,
        GoalStatus::Configured
    );
    f.chat
        .append_user(
            "message-while-paused".into(),
            "Do this message alone".into(),
        )
        .await
        .unwrap();
    f.supervisor.scan_once(1003).await.unwrap();
    assert_eq!(
        f.supervisor.next_queued_kind().await,
        Some(AutomationActionKind::OfflineChat)
    );
    assert_eq!(
        f.store.load(&f.task).unwrap().goal.unwrap().status,
        GoalStatus::Configured
    );
    f.supervisor.run_next(1004).await.unwrap().unwrap();
    f.store
        .apply_owner_goal_command(&f.task, "resume-stop-goal", OwnerGoalCommand::Run, 1005)
        .unwrap();
    f.chat.resume_agent().await.unwrap();
    f.store
        .pause_goal_for_agent_stop(&f.task, "stop-agent", 1006)
        .unwrap();
    f.chat
        .request_agent_stop("stop-agent".into())
        .await
        .unwrap();
    assert_eq!(
        f.store.load(&f.task).unwrap().goal.unwrap().status,
        GoalStatus::Active,
        "replaying the original stop must not pause an explicitly resumed goal"
    );
    assert!(!f.chat.agent_stop_requested().await.unwrap());
}

#[tokio::test]
async fn playing_a_blocked_goal_releases_its_old_timeout_and_queues_the_next_turn() {
    use codexify::agent_automation::{
        AutomationActionKind, GoalFinish, GoalStatus, OwnerGoalCommand,
    };
    let f = fixture().await;
    f.store
        .apply_owner_goal_command(
            &f.task,
            "set-blocked-goal",
            OwnerGoalCommand::Set {
                markdown: "Continue making progress".into(),
            },
            1000,
        )
        .unwrap();
    f.supervisor.scan_once(1001).await.unwrap();
    f.supervisor.run_next(1002).await.unwrap().unwrap();
    let action = f.store.load(&f.task).unwrap().action.id.unwrap();
    f.store
        .update_action_status(
            &f.task,
            &action,
            AutomationActionStatus::Ambiguous,
            Some("Previous browser action timed out"),
            None,
            2001,
        )
        .unwrap();
    let blocked = f
        .store
        .finish_goal(
            &f.task,
            1,
            GoalFinish::Blocked {
                reason: "Waiting for input".into(),
            },
            2002,
        )
        .unwrap();
    f.supervisor.scan_once(9000).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    let resumed = f
        .store
        .apply_owner_goal_command(&f.task, "play-blocked-goal", OwnerGoalCommand::Run, 10000)
        .unwrap()
        .goal
        .unwrap();
    assert_eq!(resumed.status, GoalStatus::Active);
    assert_eq!(resumed.elapsed_active_ms, blocked.elapsed_active_ms);
    assert_eq!(resumed.turn_count, 2);
    f.supervisor.scan_once(10001).await.unwrap();
    assert_eq!(
        f.supervisor.next_queued_kind().await,
        Some(AutomationActionKind::OfflineGoal)
    );
    f.supervisor.run_next(10002).await.unwrap().unwrap();
    assert_eq!(f.store.load(&f.task).unwrap().goal.unwrap().turn_count, 3);
}

#[tokio::test]
async fn goal_turns_count_confirmed_browser_submissions_once_and_only_while_enabled() {
    use codexify::agent_automation::{AutomationActionKind, OwnerGoalCommand};
    let f = fixture().await;
    f.store
        .apply_owner_goal_command(
            &f.task,
            "count-goal",
            OwnerGoalCommand::Set {
                markdown: "Track turns".into(),
            },
            1000,
        )
        .unwrap();
    f.chat
        .append_user(
            "online-message".into(),
            "One opportunistic instruction".into(),
        )
        .await
        .unwrap();
    assert_eq!(f.store.load(&f.task).unwrap().goal.unwrap().turn_count, 1);
    f.supervisor.scan_once(1001).await.unwrap();
    *f.executor.events.lock().unwrap() = vec![
        CompanionEventKind::SendAccepted,
        CompanionEventKind::SendAccepted,
    ];
    f.supervisor.run_next(1002).await.unwrap().unwrap();
    assert_eq!(
        f.store.load(&f.task).unwrap().goal.unwrap().turn_count,
        2,
        "event replay plus the terminal receipt must count one submission"
    );
    let action = f.store.load(&f.task).unwrap().action.id.unwrap();
    f.store.record_goal_turn(&f.task, &action).unwrap();
    assert_eq!(f.store.load(&f.task).unwrap().goal.unwrap().turn_count, 2);
    f.store
        .pause_goal_for_agent_stop(&f.task, "pause-counting", 1003)
        .unwrap();
    f.chat.read(true).await.unwrap();
    f.chat
        .append_user(
            "paused-message".into(),
            "An offline message while paused".into(),
        )
        .await
        .unwrap();
    f.supervisor.scan_once(1004).await.unwrap();
    assert_eq!(
        f.supervisor.next_queued_kind().await,
        Some(AutomationActionKind::OfflineChat)
    );
    f.supervisor.run_next(1005).await.unwrap().unwrap();
    assert_eq!(f.store.load(&f.task).unwrap().goal.unwrap().turn_count, 2);
    f.chat.read(true).await.unwrap();
    f.store
        .apply_owner_goal_command(&f.task, "resume-counting", OwnerGoalCommand::Run, 1006)
        .unwrap();
    f.supervisor.scan_once(1007).await.unwrap();
    assert_eq!(
        f.supervisor.next_queued_kind().await,
        Some(AutomationActionKind::OfflineGoal)
    );
    f.supervisor.run_next(1008).await.unwrap().unwrap();
    assert_eq!(f.store.load(&f.task).unwrap().goal.unwrap().turn_count, 3);
}

async fn orphaned_goal_fixture(progress: bool) -> Fixture {
    use codexify::agent_automation::{AutomationActionKind, OwnerGoalCommand};
    let f = fixture().await;
    f.store
        .apply_owner_goal_command(
            &f.task,
            "orphan-goal",
            OwnerGoalCommand::Set {
                markdown: "Continue after restart".into(),
            },
            500,
        )
        .unwrap();
    f.chat.record_agent_call(1000).await.unwrap();
    f.store
        .queue_action(
            &f.task,
            "orphaned-goal-action",
            AutomationActionKind::OfflineGoal,
            1,
            Some(1),
            2000,
        )
        .unwrap();
    f.store
        .update_action_status(
            &f.task,
            "orphaned-goal-action",
            AutomationActionStatus::Navigating,
            None,
            None,
            2001,
        )
        .unwrap();
    f.store
        .record_goal_turn(&f.task, "orphaned-goal-action")
        .unwrap();
    if progress {
        f.chat.record_agent_call(3000).await.unwrap();
    }
    f
}

fn retained_goal_snapshot() -> CompanionActionSnapshot {
    CompanionActionSnapshot {
        protocol_version: codexify::agent_automation::PROTOCOL_VERSION,
        build: COMPANION_BUILD.into(),
        action_id: "orphaned-goal-action".into(),
        state: CompanionActionState::Idle,
        send_accepted: true,
        canonical_url: Some("https://chatgpt.com/c/example".into()),
        selected: None,
        last_event_sequence: 10,
        cooldown_until_ms: None,
        attention: None,
    }
}

async fn restarted_goal_supervisor(
    f: &Fixture,
    retained: Option<CompanionActionSnapshot>,
) -> (Arc<AgentAutomationSupervisor>, Arc<FakeExecutor>) {
    let executor = Arc::new(FakeExecutor::default());
    *executor.retained.lock().unwrap() = retained;
    let restarted =
        AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor.clone());
    restarted
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    (restarted, executor)
}

#[tokio::test]
async fn completed_goal_receipt_after_restart_unblocks_the_next_turn_without_replaying_or_double_counting()
 {
    use codexify::agent_automation::AutomationActionKind;
    let f = orphaned_goal_fixture(true).await;
    let before = f.store.load(&f.task).unwrap().goal.unwrap();
    let transcript = f.chat.clean_markdown_transcript().await.unwrap();
    let (restarted, executor) = restarted_goal_supervisor(&f, Some(retained_goal_snapshot())).await;
    restarted
        .scan_once(3000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(
        restarted.next_queued_kind().await,
        Some(AutomationActionKind::OfflineGoal)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    let queued = f.store.load(&f.task).unwrap();
    assert_ne!(queued.action.id.as_deref(), Some("orphaned-goal-action"));
    assert_eq!(queued.goal.as_ref().unwrap().turn_count, 2);
    assert_eq!(
        queued.goal.as_ref().unwrap().updated_at_ms,
        before.updated_at_ms
    );
    // Presence and wake-up now share the same boundary: the old active
    // interval is paused before the queued replacement has made a tool call.
    assert_eq!(queued.goal.as_ref().unwrap().active_since_ms, None);
    assert_eq!(
        f.chat.clean_markdown_transcript().await.unwrap(),
        transcript
    );
    restarted
        .run_next(3001 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.store.load(&f.task).unwrap().goal.unwrap().turn_count, 3);
    assert_ne!(
        executor.executions.lock().unwrap()[0].request.action_id,
        "orphaned-goal-action"
    );
}

#[tokio::test]
async fn recovery_requires_an_exact_accepted_terminal_goal_receipt() {
    for case in [
        "missing",
        "not-accepted",
        "still-generating",
        "wrong-id",
        "wrong-conversation",
        "attention",
    ] {
        let f = orphaned_goal_fixture(false).await;
        let mut snapshot = retained_goal_snapshot();
        match case {
            "not-accepted" => snapshot.send_accepted = false,
            "still-generating" => snapshot.state = CompanionActionState::Generating,
            "wrong-id" => snapshot.action_id = "different-action".into(),
            "attention" => {
                snapshot.attention = Some(codexify::agent_automation::CompanionAttention {
                    code: "cancelled".into(),
                    message: None,
                })
            }
            "wrong-conversation" => {
                snapshot.canonical_url = Some("https://chatgpt.com/c/different".into())
            }
            _ => {}
        }
        let (restarted, executor) =
            restarted_goal_supervisor(&f, (case != "missing").then_some(snapshot)).await;
        restarted
            .scan_once(3000 + AUTOMATION_OFFLINE_AFTER_MS)
            .await
            .unwrap();
        assert_eq!(restarted.queued_count().await, 0, "{case}");
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0, "{case}");
        assert_eq!(
            f.store.load(&f.task).unwrap().action.status,
            AutomationActionStatus::Navigating,
            "{case}"
        );
    }
}

#[tokio::test]
async fn a_recovered_goal_without_agent_progress_waits_for_the_retry_interval() {
    let f = orphaned_goal_fixture(false).await;
    let (restarted, _) = restarted_goal_supervisor(&f, Some(retained_goal_snapshot())).await;
    restarted
        .scan_once(3000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(restarted.queued_count().await, 0);
    assert_eq!(
        f.store.load(&f.task).unwrap().action.status,
        AutomationActionStatus::Stalled
    );
    restarted
        .scan_once(4000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(restarted.queued_count().await, 0);
}

struct PausingRecoveryExecutor {
    store: Arc<AgentAutomationStore>,
    task: ConversationIdentity,
}
#[async_trait]
impl AutomationExecutor for PausingRecoveryExecutor {
    async fn retained_action(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<CompanionActionSnapshot>, String> {
        self.store.stop_goal(&self.task, 1, 3001)?;
        Ok(Some(retained_goal_snapshot()))
    }
    async fn execute(
        &self,
        _: AutomationExecution,
        _: mpsc::UnboundedSender<CompanionEvent>,
        _: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        panic!("a goal paused during recovery must not execute")
    }
}

#[tokio::test]
async fn pausing_during_retained_receipt_lookup_does_not_resume_the_goal() {
    let f = orphaned_goal_fixture(false).await;
    let executor = Arc::new(PausingRecoveryExecutor {
        store: f.store.clone(),
        task: f.task.clone(),
    });
    let restarted =
        AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor);
    restarted
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    restarted
        .scan_once(3000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(restarted.queued_count().await, 0);
    assert_eq!(
        f.store.load(&f.task).unwrap().goal.unwrap().status,
        codexify::agent_automation::GoalStatus::Configured
    );
}

#[tokio::test]
async fn real_retained_action_lookup_is_read_only_and_requires_the_same_idle_account() {
    let action_reads = Arc::new(AtomicUsize::new(0));
    let reads = action_reads.clone();
    let app = Router::new().route("/v1/health", get(|| async {
        axum::Json(serde_json::json!({"protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,"kind":"codexify-browser-companion","build":COMPANION_BUILD,"status":"idle","authenticated":true,"accountFingerprint":"account-1","activeActionId":null}))
    })).route("/v1/actions/{action_id}", get(move |headers:HeaderMap| { let reads=reads.clone(); async move {
        assert!(authorized(&headers)); reads.fetch_add(1, Ordering::SeqCst);
        axum::Json(serde_json::json!({"protocolVersion":codexify::agent_automation::PROTOCOL_VERSION,"build":COMPANION_BUILD,"actionId":"orphaned-goal-action","state":"idle","sendAccepted":true,"canonicalUrl":"https://chatgpt.com/c/example","selected":null,"lastEventSequence":10,"attention":null}))
    }}));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let descriptor = write_companion_descriptor(root.path(), &endpoint);
    let executor = BrowserAutomationExecutor::new(codexify::markdown_chat::AgentAutomationConfig {
        enabled: true,
        descriptor_path: descriptor.display().to_string(),
        ..Default::default()
    });
    assert!(
        executor
            .retained_action("orphaned-goal-action", "different-account")
            .await
            .is_err()
    );
    assert_eq!(action_reads.load(Ordering::SeqCst), 0);
    assert!(
        executor
            .retained_action("orphaned-goal-action", "account-1")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(action_reads.load(Ordering::SeqCst), 1);
    server.abort();
}

struct WatchdogExecutor {
    accepted: bool,
    pulses: Option<usize>,
    calls: AtomicUsize,
    cancelled: Mutex<Option<CancellationToken>>,
}
#[async_trait]
impl AutomationExecutor for WatchdogExecutor {
    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.cancelled.lock().unwrap() = Some(cancellation.clone());
        if self.accepted {
            events
                .send(CompanionEvent {
                    sequence: 1,
                    action_id: execution.request.action_id.clone(),
                    at_ms: 1,
                    kind: CompanionEventKind::SendAccepted,
                })
                .unwrap();
        }
        if let Some(pulses) = self.pulses {
            for i in 0..pulses {
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                if events
                    .send(CompanionEvent {
                        sequence: i as u64 + 2,
                        action_id: execution.request.action_id.clone(),
                        at_ms: i as u64 + 2,
                        kind: CompanionEventKind::StatusDelta {
                            text: "Checked generation progress".into(),
                            continuation: false,
                        },
                    })
                    .is_err()
                {
                    break;
                }
            }
            if pulses < 20 {
                return Ok(snapshot_for(execution));
            }
        }
        cancellation.cancelled().await;
        std::future::pending().await
    }
    async fn timed_out_action(
        &self,
        _: &AutomationExecution,
        message: &str,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        Err(AutomationExecutionError {
            message: message.into(),
            send_accepted: self.accepted,
            failure_status: None,
        })
    }
}

async fn watchdog_fixture(
    accepted: bool,
    pulses: Option<usize>,
    total_ms: u64,
) -> (
    Fixture,
    Arc<AgentAutomationSupervisor>,
    Arc<WatchdogExecutor>,
) {
    let f = fixture().await;
    f.chat
        .append_user(
            "watchdog-message".into(),
            "Do not lose or duplicate this instruction".into(),
        )
        .await
        .unwrap();
    let executor = Arc::new(WatchdogExecutor {
        accepted,
        pulses,
        calls: AtomicUsize::new(0),
        cancelled: Mutex::new(None),
    });
    let supervisor = AgentAutomationSupervisor::new_with_action_timeouts(
        f.config.markdown_chat.automation.clone(),
        executor.clone(),
        std::time::Duration::from_millis(300),
        std::time::Duration::from_millis(300),
        std::time::Duration::from_millis(total_ms),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    supervisor.scan_once(1000).await.unwrap();
    (f, supervisor, executor)
}

#[tokio::test]
async fn a_stalled_start_is_bounded_and_releases_the_global_queue_without_consuming_the_message() {
    let (f, s, executor) = watchdog_fixture(false, None, 2000).await;
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), s.run_next(1001))
        .await
        .unwrap();
    assert!(result.err().unwrap().contains("before Send"));
    assert_eq!(
        f.store.load(&f.task).unwrap().action.status,
        AutomationActionStatus::Failed
    );
    assert!(
        f.chat
            .read(false)
            .await
            .unwrap()
            .text
            .contains("Do not lose")
    );
    assert!(
        executor
            .cancelled
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(500), s.run_next(1002))
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn accepted_action_silence_times_out_without_replaying_its_send() {
    let (f, s, executor) = watchdog_fixture(true, None, 2000).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), s.run_next(1001))
            .await
            .unwrap()
            .err()
            .unwrap()
            .contains("without checked progress")
    );
    assert_eq!(
        f.store.load(&f.task).unwrap().action.status,
        AutomationActionStatus::Ambiguous
    );
    s.scan_once(1002).await.unwrap();
    assert_eq!(s.queued_count().await, 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn checked_progress_keeps_a_healthy_action_alive_beyond_the_idle_deadline() {
    let (_fixture, s, _executor) = watchdog_fixture(true, Some(6), 2000).await;
    assert!(s.run_next(1001).await.unwrap().is_some());
}

#[tokio::test]
async fn an_overall_deadline_bounds_even_an_action_that_keeps_emitting_progress() {
    let (f, s, _) = watchdog_fixture(true, Some(100), 650).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), s.run_next(1001))
            .await
            .unwrap()
            .err()
            .unwrap()
            .contains("time limit")
    );
    assert_eq!(
        f.store.load(&f.task).unwrap().action.status,
        AutomationActionStatus::Ambiguous
    );
}

#[tokio::test]
async fn abandoned_goal_actions_retry_and_skip_the_wake_if_agent_activity_returns() {
    let f = orphaned_goal_fixture(false).await;
    let (restarted, executor) = restarted_goal_supervisor(&f, None).await;
    restarted.scan_once(2001 + 32 * 60 * 1000).await.unwrap();
    let retried = f.store.load(&f.task).unwrap();
    assert_eq!(retried.action.status, AutomationActionStatus::Queued);
    assert!(retried.is_automatically_retrying());
    assert_ne!(retried.action.id.as_deref(), Some("orphaned-goal-action"));
    assert_eq!(
        restarted.next_queued_kind().await,
        Some(codexify::agent_automation::AutomationActionKind::OfflineGoal)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    f.chat
        .record_agent_call(2002 + 32 * 60 * 1000)
        .await
        .unwrap();
    *executor.retained.lock().unwrap() = Some(retained_goal_snapshot());
    restarted.scan_once(2002 + 32 * 60 * 1000).await.unwrap();
    restarted.run_next(2003 + 32 * 60 * 1000).await.unwrap();
    assert_eq!(
        f.store.load(&f.task).unwrap().action.status,
        AutomationActionStatus::Idle
    );
    assert_eq!(restarted.queued_count().await, 0);
    assert_eq!(
        executor.calls.load(Ordering::SeqCst),
        0,
        "returning native activity makes the queued wake stale"
    );
    assert_eq!(f.store.load(&f.task).unwrap().goal.unwrap().turn_count, 2);
}

#[tokio::test]
async fn online_agent_activity_releases_legacy_browser_ownership_without_browser_inspection() {
    let f = orphaned_goal_fixture(true).await;
    let (restarted, executor) = restarted_goal_supervisor(&f, None).await;
    restarted.scan_once(3001).await.unwrap();
    assert_eq!(executor.inspections.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.store.load(&f.task).unwrap().action.status,
        AutomationActionStatus::Idle
    );
    assert_eq!(f.store.load(&f.task).unwrap().goal.unwrap().turn_count, 2);
    assert_eq!(restarted.queued_count().await, 0);
}

#[tokio::test]
async fn submission_completion_releases_the_queue_without_waiting_for_agent_tool_activity() {
    let f = fixture().await;
    f.store
        .apply_owner_goal_command(
            &f.task,
            "submission-goal",
            codexify::agent_automation::OwnerGoalCommand::Set {
                markdown: "Continue passively".into(),
            },
            1000,
        )
        .unwrap();
    *f.executor.events.lock().unwrap() = vec![
        CompanionEventKind::SendAccepted,
        CompanionEventKind::SubmissionComplete {
            canonical_url: "https://chatgpt.com/c/example".into(),
        },
    ];
    f.supervisor.scan_once(1001).await.unwrap();
    f.supervisor.run_next(1002).await.unwrap().unwrap();
    let state = f.store.load(&f.task).unwrap();
    assert_eq!(state.action.status, AutomationActionStatus::Idle);
    assert_eq!(state.goal.unwrap().turn_count, 2);
    f.supervisor.scan_once(1003).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    f.chat.record_agent_call(1004).await.unwrap();
    f.supervisor
        .scan_once(1004 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(f.supervisor.queued_count().await, 1);
}

#[tokio::test]
async fn a_pairing_reference_gets_its_verification_window_when_the_queued_submission_starts() {
    let f = fixture().await;
    let task = ConversationIdentity::from_openai_session("delayed-pair").unwrap();
    let chat = MarkdownChatStore::default()
        .chat(&f.config, Some(&task), &SessionState::new())
        .unwrap();
    chat.ensure().await.unwrap();
    f.store
        .set_selection(&task, "gpt-6-sol", "high", "catalog-1", "account-1", 100)
        .unwrap();
    let issue = f
        .store
        .issue_pairing(&task, "https://chatgpt.com/c/delayed-pair", 0, 100)
        .unwrap();
    f.supervisor
        .register_task(AutomationTaskHandle::new(
            task.clone(),
            0,
            f.store.clone(),
            chat,
        ))
        .await
        .unwrap();
    f.supervisor
        .queue_pair_verification(&task, &issue.reference, issue.generation, 101)
        .await
        .unwrap();
    let started = 100 + 2 * codexify::agent_automation::PAIRING_REFERENCE_TTL_MS;
    f.supervisor.run_next(started).await.unwrap().unwrap();
    let state = f.store.load(&task).unwrap();
    assert_eq!(
        state.pairing.pending.unwrap().expires_at_ms,
        started + codexify::agent_automation::PAIRING_REFERENCE_TTL_MS
    );
    assert_eq!(state.action.status, AutomationActionStatus::Unverified);
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_active_goal_edit_waits_for_offline_and_uses_the_new_revision() {
    use codexify::agent_automation::OwnerGoalCommand;
    let f = fixture().await;
    f.store
        .apply_owner_goal_command(
            &f.task,
            "initial-edit-goal",
            OwnerGoalCommand::Set {
                markdown: "Original goal".into(),
            },
            1000,
        )
        .unwrap();
    f.chat.record_agent_call(1001).await.unwrap();
    f.store
        .apply_owner_goal_command(
            &f.task,
            "edited-goal",
            OwnerGoalCommand::Set {
                markdown: "Updated goal".into(),
            },
            1002,
        )
        .unwrap();
    f.chat
        .append_user(
            "goal-update-notice".into(),
            "The goal has changed. The new goal is: Updated goal".into(),
        )
        .await
        .unwrap();
    f.supervisor.scan_once(1003).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 0);
    let notice = f.chat.read(true).await.unwrap();
    assert!(notice.text.contains("Updated goal"));
    f.chat.record_agent_call(1004).await.unwrap();
    let offline = 1004 + AUTOMATION_OFFLINE_AFTER_MS;
    f.supervisor.scan_once(offline).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 1);
    f.supervisor.run_next(offline + 1).await.unwrap().unwrap();
    let executions = f.executor.executions.lock().unwrap();
    assert!(matches!(
        executions[0].request.prompt,
        ActionPrompt::OfflineGoal { goal_revision: 2 }
    ));
    let goal = f.store.load(&f.task).unwrap().goal.unwrap();
    assert_eq!(goal.revision, 2);
    assert_eq!(goal.markdown, "Updated goal");
    assert_eq!(goal.turn_count, 2);
}

#[tokio::test]
async fn editing_a_queued_goal_never_submits_the_old_revision() {
    use codexify::agent_automation::OwnerGoalCommand;
    let f = fixture().await;
    f.store
        .apply_owner_goal_command(
            &f.task,
            "queued-original",
            OwnerGoalCommand::Set {
                markdown: "Original queued goal".into(),
            },
            1000,
        )
        .unwrap();
    f.supervisor.scan_once(1001).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 1);
    f.store
        .apply_owner_goal_command(
            &f.task,
            "queued-edit",
            OwnerGoalCommand::Set {
                markdown: "Updated queued goal".into(),
            },
            1002,
        )
        .unwrap();
    f.supervisor.scan_once(1003).await.unwrap();
    f.supervisor.run_next(1004).await.unwrap().unwrap();
    let executions = f.executor.executions.lock().unwrap();
    assert_eq!(executions.len(), 1);
    assert!(matches!(
        executions[0].request.prompt,
        ActionPrompt::OfflineGoal { goal_revision: 2 }
    ));
    assert_eq!(
        f.store.load(&f.task).unwrap().goal.unwrap().markdown,
        "Updated queued goal"
    );
}

#[tokio::test]
async fn verified_pairing_routes_a_message_posted_during_grace_once_after_five_seconds() {
    let f = fixture().await;
    f.chat.record_agent_call(1000).await.unwrap();
    assert!(
        f.chat
            .finish_pairing_verification("direct", 1, 1000)
            .await
            .unwrap()
    );
    f.chat
        .append_user(
            "pairing-grace-message".into(),
            "Do the actual task now".into(),
        )
        .await
        .unwrap();
    f.supervisor.scan_once(5999).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    f.supervisor.scan_once(6000).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 1);
    f.supervisor.run_next(6000).await.unwrap();
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 1);
    {
        let executions = f.executor.executions.lock().unwrap();
        assert_eq!(executions[0].request.model.as_deref(), Some("gpt-6-sol"));
        assert_eq!(executions[0].request.effort.as_deref(), Some("high"));
        match &executions[0].request.prompt {
            ActionPrompt::OfflineChat { messages, .. } => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].id, "pairing-grace-message");
            }
            _ => panic!("expected exactly one offline chat delivery"),
        }
    }
    f.supervisor.scan_once(6001).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn agent_reading_a_grace_period_message_prevents_its_browser_fallback() {
    let f = fixture().await;
    f.chat.record_agent_call(1000).await.unwrap();
    assert!(
        f.chat
            .finish_pairing_verification("direct", 1, 1000)
            .await
            .unwrap()
    );
    f.chat
        .append_user("pairing-read-message".into(), "Do the task".into())
        .await
        .unwrap();
    assert!(!f.chat.read(true).await.unwrap().text.is_empty());
    f.chat.record_agent_call(2000).await.unwrap();
    assert!(
        f.chat
            .owner_summary()
            .await
            .unwrap()
            .agent_presence_until_ms
            .is_none()
    );
    f.supervisor.scan_once(6000).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    f.supervisor
        .scan_once(2000 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn a_message_posted_after_pairing_grace_is_immediately_eligible_for_browser_delivery() {
    let f = fixture().await;
    f.chat.record_agent_call(1000).await.unwrap();
    assert!(
        f.chat
            .finish_pairing_verification("direct", 1, 1000)
            .await
            .unwrap()
    );
    f.chat
        .append_user("after-pairing-grace".into(), "Run the task".into())
        .await
        .unwrap();
    f.supervisor.scan_once(7000).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 1);
    f.supervisor.run_next(7000).await.unwrap();
    f.supervisor.scan_once(7001).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_authorized_goal_can_start_after_pairing_grace_without_waiting_four_minutes() {
    let f = fixture().await;
    f.chat.record_agent_call(1000).await.unwrap();
    assert!(
        f.chat
            .finish_pairing_verification("direct", 1, 1000)
            .await
            .unwrap()
    );
    let goal = f
        .store
        .set_goal(&f.task, "Complete the requested task", 1000)
        .unwrap();
    f.store.run_goal(&f.task, goal.revision, 1000).unwrap();
    f.supervisor.scan_once(5999).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    f.supervisor.scan_once(6000).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 1);
    f.supervisor.run_next(6000).await.unwrap();
    let executions = f.executor.executions.lock().unwrap();
    assert!(matches!(
        executions[0].request.prompt,
        ActionPrompt::OfflineGoal { .. }
    ));
    assert_eq!(executions[0].request.model.as_deref(), Some("gpt-6-sol"));
}

#[tokio::test]
async fn cancelling_a_failed_action_stops_retries_across_restart_without_consuming_messages() {
    use codexify::agent_automation::{AutomationActionKind, GoalStatus};
    let f = fixture().await;
    let goal = f
        .store
        .set_goal(&f.task, "Keep the requested goal", 1000)
        .unwrap();
    f.store.run_goal(&f.task, goal.revision, 1000).unwrap();
    f.chat
        .append_user("cancel-pending".into(), "Keep this message unread".into())
        .await
        .unwrap();
    f.supervisor.scan_once(1001).await.unwrap();
    let action_id = f.store.load(&f.task).unwrap().action.id.unwrap();
    f.supervisor.discard_queued_actions(&f.task).await;
    f.store
        .update_action_status(
            &f.task,
            &action_id,
            AutomationActionStatus::Failed,
            Some("Browser unavailable before Send"),
            None,
            1002,
        )
        .unwrap();
    let before = f.store.load(&f.task).unwrap();
    assert!(before.automatic_retry_at_ms().is_some());
    let cancelled = f
        .supervisor
        .cancel_action(&f.task, &action_id, 1003)
        .await
        .unwrap();
    assert_eq!(cancelled.status, AutomationActionStatus::Cancelled);
    assert!(cancelled.last_error.is_none());
    let after = f.store.load(&f.task).unwrap();
    assert_eq!(after.goal, before.goal);
    assert_eq!(after.chat_mirror, before.chat_mirror);
    assert_eq!(after.goal.as_ref().unwrap().status, GoalStatus::Active);
    assert!(after.automatic_retry_at_ms().is_none());
    assert!(!after.action_needs_attention());
    assert_eq!(after.retry_mode(), Some("retry"));
    assert_eq!(f.chat.unread_snapshot().await.unwrap().messages.len(), 1);
    assert_eq!(
        f.supervisor
            .cancel_action(&f.task, &action_id, 1004)
            .await
            .unwrap(),
        cancelled
    );
    let restarted = AgentAutomationSupervisor::new(
        f.config.markdown_chat.automation.clone(),
        f.executor.clone(),
    );
    restarted
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    restarted.scan_once(1_000_000).await.unwrap();
    assert_eq!(restarted.queued_count().await, 0);
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 0);
    // An explicit retry remains possible; cancelling did not discard the user's intent.
    restarted
        .retry_failed_action(&f.task, &action_id, 1_000_001)
        .await
        .unwrap();
    assert_eq!(
        restarted.next_queued_kind().await,
        Some(AutomationActionKind::OfflineCombined)
    );
}

#[tokio::test]
async fn new_messages_after_cancelling_a_fenced_send_do_not_replay_the_accepted_batch() {
    let f = fixture().await;
    let failed = AgentAutomationSupervisor::new(
        f.config.markdown_chat.automation.clone(),
        Arc::new(AmbiguousExecutor),
    );
    failed
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    f.chat
        .append_user("already-submitted".into(), "Original message".into())
        .await
        .unwrap();
    failed.scan_once(1001).await.unwrap();
    assert!(failed.run_next(1002).await.is_err());
    let submitted = f.store.load(&f.task).unwrap();
    assert_eq!(submitted.action.status, AutomationActionStatus::Ambiguous);
    assert!(submitted.action.messages_fenced);
    let action_id = submitted.action.id.unwrap();
    failed
        .cancel_action(&f.task, &action_id, 1004)
        .await
        .unwrap();
    f.supervisor.scan_once(1005).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    f.chat
        .append_user("new-after-cancellation".into(), "Continue".into())
        .await
        .unwrap();
    f.store
        .resume_cancelled_action_for_new_message(&f.task, 1006)
        .unwrap();
    assert_eq!(
        f.store.load(&f.task).unwrap().chat_mirror,
        submitted.chat_mirror
    );
    f.supervisor.scan_once(1007).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 1);
    f.supervisor.run_next(1008).await.unwrap();
    let executions = f.executor.executions.lock().unwrap();
    assert_eq!(executions.len(), 1);
    let ActionPrompt::OfflineChat { messages, .. } = &executions[0].request.prompt else {
        panic!("expected a new user-message batch");
    };
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, "new-after-cancellation");
}

#[tokio::test]
async fn resuming_a_goal_after_cancelling_a_fenced_send_preserves_accepted_messages() {
    for active in [false, true] {
        let f = fixture().await;
        let failed = AgentAutomationSupervisor::new(
            f.config.markdown_chat.automation.clone(),
            Arc::new(AmbiguousExecutor),
        );
        failed
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                f.store.clone(),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        let goal = f
            .store
            .set_goal(&f.task, "Keep the existing goal", 900)
            .unwrap();
        if active {
            f.store.run_goal(&f.task, goal.revision, 901).unwrap();
        }
        f.chat
            .append_user("already-submitted".into(), "Original message".into())
            .await
            .unwrap();
        failed.scan_once(1001).await.unwrap();
        assert!(failed.run_next(1002).await.is_err());
        let submitted = f.store.load(&f.task).unwrap();
        assert_eq!(submitted.action.status, AutomationActionStatus::Ambiguous);
        assert!(submitted.action.messages_fenced);
        let action_id = submitted.action.id.unwrap();
        failed
            .cancel_action(&f.task, &action_id, 1004)
            .await
            .unwrap();
        let before = f.store.load(&f.task).unwrap();
        let receipt = f
            .store
            .apply_owner_goal_command(
                &f.task,
                "resume-fenced-goal",
                codexify::agent_automation::OwnerGoalCommand::Run,
                1005,
            )
            .unwrap();
        let resumed = receipt.goal.unwrap();
        let previous = before.goal.unwrap();
        assert_eq!(resumed.revision, previous.revision);
        assert_eq!(resumed.elapsed_active_ms, previous.elapsed_active_ms);
        assert_eq!(resumed.turn_count, previous.turn_count);
        assert_eq!(
            f.store.load(&f.task).unwrap().chat_mirror,
            before.chat_mirror
        );
        f.supervisor.scan_once(1006).await.unwrap();
        assert_eq!(f.supervisor.queued_count().await, 1);
        assert_eq!(
            f.supervisor.next_queued_kind().await,
            Some(codexify::agent_automation::AutomationActionKind::OfflineGoal)
        );
    }
}

#[tokio::test]
async fn cancelling_an_uncertain_send_retains_its_acceptance_evidence_and_cannot_replay_it() {
    let f = fixture().await;
    f.chat
        .append_user(
            "uncertain-cancel".into(),
            "Do not duplicate this message".into(),
        )
        .await
        .unwrap();
    f.supervisor.scan_once(1001).await.unwrap();
    let action_id = f.store.load(&f.task).unwrap().action.id.unwrap();
    f.supervisor.discard_queued_actions(&f.task).await;
    f.store
        .update_action_status(
            &f.task,
            &action_id,
            AutomationActionStatus::Ambiguous,
            Some("Send could not be confirmed"),
            None,
            1002,
        )
        .unwrap();
    f.supervisor
        .cancel_action(&f.task, &action_id, 1003)
        .await
        .unwrap();
    let after = f.store.load(&f.task).unwrap();
    assert_eq!(after.action.send_accepted, Some(true));
    assert_eq!(after.action.status, AutomationActionStatus::Cancelled);
    assert!(after.retry_mode().is_none());
    assert!(
        f.supervisor
            .retry_failed_action(&f.task, &action_id, 1004)
            .await
            .is_err()
    );
    f.store
        .resume_cancelled_action_for_new_message(&f.task, 1005)
        .unwrap();
    f.supervisor.scan_once(1_000_000).await.unwrap();
    assert_eq!(f.store.load(&f.task).unwrap().action, after.action);
    assert_eq!(f.supervisor.queued_count().await, 0);
    assert_eq!(f.chat.unread_snapshot().await.unwrap().messages.len(), 1);
}

#[tokio::test]
async fn cancellation_rejects_stale_or_running_actions_and_new_intent_can_resume_unsent_work() {
    let f = fixture().await;
    f.chat
        .append_user("cancel-queued".into(), "Send this later".into())
        .await
        .unwrap();
    f.supervisor.scan_once(1001).await.unwrap();
    let action_id = f.store.load(&f.task).unwrap().action.id.unwrap();
    assert!(
        f.supervisor
            .cancel_action(&f.task, "old-action", 1002)
            .await
            .is_err()
    );
    assert_eq!(f.supervisor.queued_count().await, 1);
    f.supervisor
        .cancel_action(&f.task, &action_id, 1003)
        .await
        .unwrap();
    assert!(f.supervisor.run_next(1004).await.unwrap().is_none());
    f.store
        .resume_cancelled_action_for_new_message(&f.task, 1002)
        .unwrap();
    assert_eq!(
        f.store.load(&f.task).unwrap().action.status,
        AutomationActionStatus::Cancelled
    );
    f.store
        .resume_cancelled_action_for_new_message(&f.task, 1005)
        .unwrap();
    f.supervisor.scan_once(1006).await.unwrap();
    let queued = f.store.load(&f.task).unwrap().action;
    assert_ne!(queued.id.as_deref(), Some(action_id.as_str()));
    assert_eq!(f.supervisor.queued_count().await, 1);
    f.store
        .update_action_status(
            &f.task,
            queued.id.as_ref().unwrap(),
            AutomationActionStatus::Navigating,
            None,
            None,
            1007,
        )
        .unwrap();
    assert!(
        f.supervisor
            .cancel_action(&f.task, queued.id.as_ref().unwrap(), 1008)
            .await
            .is_err()
    );
    assert_eq!(
        f.store.load(&f.task).unwrap().action.status,
        AutomationActionStatus::Navigating
    );
}

struct CreatedPairingFailureExecutor;
#[async_trait]
impl AutomationExecutor for CreatedPairingFailureExecutor {
    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        _: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        for (sequence, kind) in [
            CompanionEventKind::SendAccepted,
            CompanionEventKind::ConversationCreated {
                canonical_url: "https://chatgpt.com/c/created-despite-failure".into(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            events
                .send(CompanionEvent {
                    sequence: sequence as u64 + 1,
                    action_id: execution.request.action_id.clone(),
                    at_ms: 1002,
                    kind,
                })
                .unwrap();
        }
        Err(AutomationExecutionError {
            message: "Model restoration failed after creation".into(),
            send_accepted: true,
            failure_status: None,
        })
    }
}

#[tokio::test]
async fn creation_records_its_url_before_failure_and_accepts_verification_after_the_grace_period() {
    let f = fixture().await;
    let issue = f
        .store
        .issue_pairing(
            &f.task,
            "https://chatgpt.com/g/g-p-project/project",
            0,
            1000,
        )
        .unwrap();
    f.store.mark_creating_chat(&f.task).unwrap();
    let supervisor = AgentAutomationSupervisor::new(
        f.config.markdown_chat.automation.clone(),
        Arc::new(CreatedPairingFailureExecutor),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    supervisor
        .queue_new_chat(
            &f.task,
            &issue.reference,
            issue.generation,
            1001,
            "B".repeat(43),
            None,
        )
        .await
        .unwrap();
    assert!(supervisor.run_next(1002).await.is_err());
    let state = f.store.load(&f.task).unwrap();
    assert_eq!(
        state.pairing.canonical_url.as_deref(),
        Some("https://chatgpt.com/c/created-despite-failure")
    );
    assert_eq!(
        state.pairing.last_attempted_url,
        state.pairing.canonical_url
    );
    assert!(state.pairing.pending.is_some());
    assert!(state.action_needs_attention_at(70_000));
    assert_eq!(supervisor.queued_count().await, 0);
    f.store
        .verify_pairing_reference(&f.task, 0, &issue.reference, 70_000)
        .unwrap();
    let state = f.store.load(&f.task).unwrap();
    assert_eq!(state.pairing.verified_owner_generation, Some(0));
    assert!(state.action.last_error.is_none());
    assert!(!state.action_needs_attention_at(70_001));
}

#[tokio::test]
async fn agent_delivery_acknowledgment_clears_ambiguous_chat_and_combined_sends_without_replay() {
    for combined in [false, true] {
        let f = fixture().await;
        if combined {
            let goal = f.store.set_goal(&f.task, "Continue the goal", 100).unwrap();
            f.store.run_goal(&f.task, goal.revision, 101).unwrap();
        }
        f.chat.record_agent_call(1_000).await.unwrap();
        let sent = f
            .chat
            .append_user("delivery-proof".into(), "Use the new target".into())
            .await
            .unwrap();
        let s = AgentAutomationSupervisor::new(
            f.config.markdown_chat.automation.clone(),
            Arc::new(AmbiguousExecutor),
        );
        s.register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
        let now = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
        s.scan_once(now).await.unwrap();
        assert!(s.run_next(now + 1).await.is_err());
        let failed = f.store.load(&f.task).unwrap();
        assert_eq!(failed.action.status, AutomationActionStatus::Ambiguous);
        assert!(failed.action.messages_fenced);
        assert!(!failed.action.goal_turn_counted);

        // Activity or an outbound delivery receipt alone does not prove that
        // the agent read this exact batch; leave uncertainty fenced.
        f.chat.mark_delivered(sent.end).await.unwrap();
        f.chat.record_agent_call(now + 2).await.unwrap();
        s.scan_once(now + 2).await.unwrap();
        assert_eq!(
            f.store.load(&f.task).unwrap().action.status,
            AutomationActionStatus::Ambiguous
        );

        f.chat.read(true).await.unwrap();
        f.chat.record_agent_call(now + 3).await.unwrap();
        s.scan_once(now + 3).await.unwrap();
        let confirmed = f.store.load(&f.task).unwrap();
        assert_eq!(confirmed.action.status, AutomationActionStatus::Idle);
        assert!(confirmed.action.last_error.is_none());
        assert_eq!(confirmed.action.send_accepted, Some(true));
        assert!(s.live_state(&f.task).await.is_none());
        if combined {
            assert_eq!(confirmed.goal.unwrap().turn_count, 2);
        }

        // A late observer result must not put the confirmed action back into
        // an error state after the stronger agent acknowledgment won.
        f.store
            .update_action_status(
                &f.task,
                failed.action.id.as_deref().unwrap(),
                AutomationActionStatus::Ambiguous,
                Some("Late browser cleanup error"),
                None,
                now + 4,
            )
            .unwrap();
        assert_eq!(
            f.store.load(&f.task).unwrap().action.status,
            AutomationActionStatus::Idle
        );
        assert!(f.store.load(&f.task).unwrap().action.last_error.is_none());
        s.scan_once(now + 4).await.unwrap();
        assert_eq!(s.queued_count().await, 0);
        if combined {
            assert_eq!(f.store.load(&f.task).unwrap().goal.unwrap().turn_count, 2);
        }

        f.chat
            .append_user(
                "next-instruction".into(),
                "Only send this new instruction".into(),
            )
            .await
            .unwrap();
        let executor = Arc::new(FakeExecutor::default());
        let restarted = AgentAutomationSupervisor::new(
            f.config.markdown_chat.automation.clone(),
            executor.clone(),
        );
        restarted
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                f.store.clone(),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        restarted
            .scan_once(now + 3 + AUTOMATION_OFFLINE_AFTER_MS)
            .await
            .unwrap();
        restarted
            .run_next(now + 4 + AUTOMATION_OFFLINE_AFTER_MS)
            .await
            .unwrap();
        let requests = executor.executions.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].unread_message_ids, vec!["next-instruction"]);
    }
}

#[tokio::test]
async fn agent_delivery_acknowledgment_recovers_an_ambiguous_chat_after_restart() {
    let f = fixture().await;
    f.chat.record_agent_call(1_000).await.unwrap();
    f.chat
        .append_user(
            "read-before-restart".into(),
            "Already received by the agent".into(),
        )
        .await
        .unwrap();
    let s = AgentAutomationSupervisor::new(
        f.config.markdown_chat.automation.clone(),
        Arc::new(AmbiguousExecutor),
    );
    s.register_task(AutomationTaskHandle::new(
        f.task.clone(),
        0,
        f.store.clone(),
        f.chat.clone(),
    ))
    .await
    .unwrap();
    let now = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
    s.scan_once(now).await.unwrap();
    assert!(s.run_next(now + 1).await.is_err());
    f.chat.read(true).await.unwrap();
    f.chat.record_agent_call(now + 2).await.unwrap();
    let executor = Arc::new(FakeExecutor::default());
    let restarted =
        AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor.clone());
    restarted
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    restarted.scan_once(now + 2).await.unwrap();
    assert_eq!(
        f.store.load(&f.task).unwrap().action.status,
        AutomationActionStatus::Idle
    );
    assert!(f.store.load(&f.task).unwrap().action.last_error.is_none());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(executor.inspections.load(Ordering::SeqCst), 0);
    assert_eq!(restarted.queued_count().await, 0);
}

#[tokio::test]
async fn agent_delivery_goal_progress_does_not_require_a_browser_acceptance_flag() {
    let f = fixture().await;
    let goal = f.store.set_goal(&f.task, "Keep working", 100).unwrap();
    f.store.run_goal(&f.task, goal.revision, 101).unwrap();
    f.chat.record_agent_call(1_000).await.unwrap();
    let s = AgentAutomationSupervisor::new(
        f.config.markdown_chat.automation.clone(),
        Arc::new(AmbiguousExecutor),
    );
    s.register_task(AutomationTaskHandle::new(
        f.task.clone(),
        0,
        f.store.clone(),
        f.chat.clone(),
    ))
    .await
    .unwrap();
    let now = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
    s.scan_once(now).await.unwrap();
    assert!(s.run_next(now + 1).await.is_err());
    let failed = f.store.load(&f.task).unwrap();
    assert!(!failed.action.goal_turn_counted);
    assert!(failed.goal_wake.nudged_epoch.is_some());
    s.scan_once(now + 2).await.unwrap();
    assert_eq!(
        f.store.load(&f.task).unwrap().action.status,
        AutomationActionStatus::Ambiguous
    );
    f.chat.record_agent_call(now + 3).await.unwrap();
    s.scan_once(now + 3).await.unwrap();
    let recovered = f.store.load(&f.task).unwrap();
    assert_eq!(recovered.action.status, AutomationActionStatus::Idle);
    assert!(recovered.action.last_error.is_none());
    assert_eq!(recovered.goal.unwrap().turn_count, 2);
    assert_eq!(s.queued_count().await, 0);
    assert!(s.live_state(&f.task).await.is_none());
}

#[tokio::test]
async fn agent_delivery_acknowledgment_does_not_override_explicit_cancellation() {
    let f = fixture().await;
    f.chat.record_agent_call(1_000).await.unwrap();
    f.chat
        .append_user(
            "cancelled-delivery".into(),
            "Instruction before cancellation".into(),
        )
        .await
        .unwrap();
    let s = AgentAutomationSupervisor::new(
        f.config.markdown_chat.automation.clone(),
        Arc::new(AmbiguousExecutor),
    );
    s.register_task(AutomationTaskHandle::new(
        f.task.clone(),
        0,
        f.store.clone(),
        f.chat.clone(),
    ))
    .await
    .unwrap();
    let now = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
    s.scan_once(now).await.unwrap();
    assert!(s.run_next(now + 1).await.is_err());
    let action_id = f.store.load(&f.task).unwrap().action.id.unwrap();
    f.store.cancel_action(&f.task, &action_id, now + 2).unwrap();
    f.chat.read(true).await.unwrap();
    f.chat.record_agent_call(now + 3).await.unwrap();
    s.scan_once(now + 3).await.unwrap();
    assert_eq!(
        f.store.load(&f.task).unwrap().action.status,
        AutomationActionStatus::Cancelled
    );
    assert_eq!(s.queued_count().await, 0);
}

#[tokio::test]
async fn agent_delivery_acknowledgment_wins_over_a_late_browser_observer_failure() {
    struct LateFailure {
        started: Notify,
        release: Notify,
        execution: Mutex<Option<AutomationExecution>>,
    }
    #[async_trait]
    impl AutomationExecutor for LateFailure {
        async fn execute(
            &self,
            execution: AutomationExecution,
            _: mpsc::UnboundedSender<CompanionEvent>,
            _: CancellationToken,
        ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
            *self.execution.lock().unwrap() = Some(execution);
            self.started.notify_one();
            self.release.notified().await;
            Err(AutomationExecutionError {
                message: "Late browser observer failure".into(),
                send_accepted: true,
                failure_status: None,
            })
        }
    }
    let f = fixture().await;
    f.chat.record_agent_call(1_000).await.unwrap();
    f.chat
        .append_user(
            "late-observer-message".into(),
            "Already read by the agent".into(),
        )
        .await
        .unwrap();
    let executor = Arc::new(LateFailure {
        started: Notify::new(),
        release: Notify::new(),
        execution: Mutex::new(None),
    });
    let s =
        AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor.clone());
    s.register_task(AutomationTaskHandle::new(
        f.task.clone(),
        0,
        f.store.clone(),
        f.chat.clone(),
    ))
    .await
    .unwrap();
    let now = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
    s.scan_once(now).await.unwrap();
    let runner = tokio::spawn({
        let s = s.clone();
        async move { s.run_next(now + 1).await }
    });
    executor.started.notified().await;
    let execution = executor.execution.lock().unwrap().clone().unwrap();
    let ActionPrompt::OfflineChat { ref batch_id, .. } = execution.request.prompt else {
        panic!("expected chat batch")
    };
    f.store
        .record_accepted_batch(
            &f.task,
            &execution.request.action_id,
            codexify::agent_automation::AcceptedChatBatch {
                owner_generation: 0,
                batch_id,
                message_ids: &execution.unread_message_ids,
                transcript_revision: &execution.transcript_revision,
            },
            now + 2,
        )
        .unwrap();
    f.store
        .update_action_status(
            &f.task,
            &execution.request.action_id,
            AutomationActionStatus::Ambiguous,
            Some("Initial browser uncertainty"),
            None,
            now + 2,
        )
        .unwrap();
    f.chat.read(true).await.unwrap();
    f.chat.record_agent_call(now + 3).await.unwrap();
    s.scan_once(now + 3).await.unwrap();
    assert_eq!(
        f.store.load(&f.task).unwrap().action.status,
        AutomationActionStatus::Idle
    );
    executor.release.notify_one();
    assert!(runner.await.unwrap().is_err());
    let final_state = f.store.load(&f.task).unwrap();
    assert_eq!(final_state.action.status, AutomationActionStatus::Idle);
    assert!(final_state.action.last_error.is_none());
    assert!(s.live_state(&f.task).await.is_none());
}

#[tokio::test]
async fn native_delivery_clears_pre_send_failure_and_retry_after_restart() {
    for combined in [false, true] {
        for restart in [false, true] {
            let f = fixture().await;
            if combined {
                let goal = f.store.set_goal(&f.task, "Keep working", 100).unwrap();
                f.store.run_goal(&f.task, goal.revision, 101).unwrap();
            }
            f.chat.record_agent_call(1_000).await.unwrap();
            let sent = f
                .chat
                .append_user(
                    "native-read".into(),
                    "Already delivered through a tool response".into(),
                )
                .await
                .unwrap();
            let executor = Arc::new(PreSendFailureExecutor::default());
            let mut s = AgentAutomationSupervisor::new(
                f.config.markdown_chat.automation.clone(),
                executor.clone(),
            );
            s.register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                f.store.clone(),
                f.chat.clone(),
            ))
            .await
            .unwrap();
            let now = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
            s.scan_once(now).await.unwrap();
            assert!(s.run_next(now + 1).await.is_err());
            let failed = f.store.load(&f.task).unwrap();
            assert_eq!(failed.action.send_accepted, Some(false));
            assert!(!failed.action.messages_fenced);
            assert!(failed.automatic_retry_at_ms().is_some());
            if restart {
                s = AgentAutomationSupervisor::new(
                    f.config.markdown_chat.automation.clone(),
                    executor.clone(),
                );
                s.register_task(AutomationTaskHandle::new(
                    f.task.clone(),
                    0,
                    f.store.clone(),
                    f.chat.clone(),
                ))
                .await
                .unwrap();
            }
            f.chat.mark_delivered(sent.end).await.unwrap();
            f.chat.record_agent_call(now + 2).await.unwrap();
            s.scan_once(now + 2).await.unwrap();
            assert_eq!(
                f.store.load(&f.task).unwrap().action.status,
                AutomationActionStatus::Failed
            );
            f.chat.read(true).await.unwrap();
            f.chat.record_agent_call(now + 3).await.unwrap();
            // Native tool completion and UI state reads register the task.
            // Recovery must not wait for the global browser queue to drain.
            s.register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                f.store.clone(),
                f.chat.clone(),
            ))
            .await
            .unwrap();
            let confirmed = f.store.load(&f.task).unwrap();
            assert_eq!(confirmed.action.status, AutomationActionStatus::Idle);
            assert!(confirmed.action.last_error.is_none());
            assert!(confirmed.automatic_retry_at_ms().is_none());
            assert!(confirmed.action.messages_fenced);
            assert_eq!(confirmed.chat_mirror.message_ids, vec!["native-read"]);
            assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
            assert_eq!(s.queued_count().await, 0);
            if combined {
                assert_eq!(
                    confirmed.goal.unwrap().turn_count,
                    1,
                    "Native delivery did not start a new browser turn"
                );
            }
            let directory = f.store.task_directory(&f.task).unwrap();
            let intent: serde_json::Value = serde_json::from_slice(
                &std::fs::read(directory.join("chat-submission.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(intent["messageIds"], serde_json::json!(["native-read"]));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(directory.join("chat-submission.json"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
        }
    }
}

#[tokio::test]
async fn native_delivery_during_browser_preparation_is_successful_without_an_error_banner() {
    struct ReadsThenFails {
        chat: Arc<codexify::markdown_chat::ChatFile>,
    }
    #[async_trait]
    impl AutomationExecutor for ReadsThenFails {
        async fn execute(
            &self,
            _: AutomationExecution,
            _: mpsc::UnboundedSender<CompanionEvent>,
            _: CancellationToken,
        ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
            self.chat.read(true).await.unwrap();
            Err(AutomationExecutionError {
                message: "ChatGPT did not stop the previous response; the new message was not sent"
                    .into(),
                send_accepted: false,
                failure_status: None,
            })
        }
    }
    let f = fixture().await;
    f.chat.record_agent_call(1_000).await.unwrap();
    f.chat
        .append_user(
            "read-during-stop".into(),
            "The working agent receives this directly".into(),
        )
        .await
        .unwrap();
    let s = AgentAutomationSupervisor::new(
        f.config.markdown_chat.automation.clone(),
        Arc::new(ReadsThenFails {
            chat: f.chat.clone(),
        }),
    );
    s.register_task(AutomationTaskHandle::new(
        f.task.clone(),
        0,
        f.store.clone(),
        f.chat.clone(),
    ))
    .await
    .unwrap();
    let now = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
    s.scan_once(now).await.unwrap();
    assert!(s.run_next(now + 1).await.unwrap().is_none());
    let state = f.store.load(&f.task).unwrap();
    assert_eq!(state.action.status, AutomationActionStatus::Idle);
    assert!(state.action.last_error.is_none());
    assert!(state.action.messages_fenced);
    assert!(state.automatic_retry_at_ms().is_none());
    assert!(s.live_state(&f.task).await.is_none());
}

#[tokio::test]
async fn native_delivery_while_queued_skips_browser_work_and_stale_action_errors() {
    let f = fixture().await;
    f.chat.record_agent_call(1_000).await.unwrap();
    f.chat
        .append_user(
            "read-while-queued".into(),
            "No browser submission is necessary".into(),
        )
        .await
        .unwrap();
    let now = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
    f.supervisor.scan_once(now).await.unwrap();
    f.chat.read(true).await.unwrap();
    f.chat.record_agent_call(now + 1).await.unwrap();
    assert!(f.supervisor.run_next(now + 2).await.unwrap().is_none());
    let state = f.store.load(&f.task).unwrap();
    assert_eq!(state.action.status, AutomationActionStatus::Idle);
    assert!(state.action.last_error.is_none());
    assert!(state.action.messages_fenced);
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn native_delivery_does_not_reuse_an_older_submission_intent_for_a_new_action() {
    let f = fixture().await;
    f.chat.record_agent_call(1_000).await.unwrap();
    f.chat
        .append_user("old-intent".into(), "The old batch".into())
        .await
        .unwrap();
    let s = AgentAutomationSupervisor::new(
        f.config.markdown_chat.automation.clone(),
        Arc::new(PreSendFailureExecutor::default()),
    );
    s.register_task(AutomationTaskHandle::new(
        f.task.clone(),
        0,
        f.store.clone(),
        f.chat.clone(),
    ))
    .await
    .unwrap();
    let now = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
    s.scan_once(now).await.unwrap();
    assert!(s.run_next(now + 1).await.is_err());
    f.chat.read(true).await.unwrap();
    f.chat
        .append_user(
            "new-unread-intent".into(),
            "This batch has not been read".into(),
        )
        .await
        .unwrap();
    let generation = f.store.load(&f.task).unwrap().pairing.generation;
    f.store
        .queue_action(
            &f.task,
            "new-action-without-submission",
            codexify::agent_automation::AutomationActionKind::OfflineChat,
            generation,
            None,
            now + 2,
        )
        .unwrap();
    f.store
        .update_action_status(
            &f.task,
            "new-action-without-submission",
            AutomationActionStatus::Failed,
            Some("New unread action"),
            None,
            now + 3,
        )
        .unwrap();
    f.chat.record_agent_call(now + 4).await.unwrap();
    s.scan_once(now + 4).await.unwrap();
    let state = f.store.load(&f.task).unwrap();
    assert_eq!(state.action.status, AutomationActionStatus::Failed);
    assert_eq!(
        state.action.last_error.as_deref(),
        Some("New unread action")
    );
    assert_eq!(
        f.chat.unread_snapshot().await.unwrap().messages[0].id,
        "new-unread-intent"
    );
}

#[tokio::test]
async fn offline_goal_retries_three_times_with_backoff_without_tool_activity_across_restart() {
    for submitted_only in [false, true] {
        let f = fixture().await;
        let goal = f
            .store
            .set_goal(&f.task, "Finish the objective", 100)
            .unwrap();
        f.store.run_goal(&f.task, goal.revision, 101).unwrap();
        f.chat.record_agent_call(1_000).await.unwrap();
        f.executor.events.lock().unwrap().push(if submitted_only {
            CompanionEventKind::SubmissionComplete {
                canonical_url: "https://chatgpt.com/c/example".into(),
            }
        } else {
            CompanionEventKind::GenerationComplete {
                final_markdown: None,
                canonical_url: Some("https://chatgpt.com/c/example".into()),
            }
        });
        let first = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
        f.supervisor.scan_once(first).await.unwrap();
        f.supervisor.run_next(first + 1).await.unwrap();
        let mut previous_action = f.store.load(&f.task).unwrap().action.id;
        for (retry, delay) in [60_000, 120_000, 240_000].into_iter().enumerate() {
            let state = f.store.load(&f.task).unwrap();
            let earliest = state
                .goal_wake
                .last_nudged_at_ms
                .unwrap()
                .max(state.action.updated_at_ms.unwrap())
                + delay;
            let deadline = state
                .automatic_retry_at_ms()
                .unwrap_or(earliest + delay / 10);
            let restarted = AgentAutomationSupervisor::new(
                f.config.markdown_chat.automation.clone(),
                f.executor.clone(),
            );
            restarted
                .register_task(AutomationTaskHandle::new(
                    f.task.clone(),
                    0,
                    f.store.clone(),
                    f.chat.clone(),
                ))
                .await
                .unwrap();
            restarted.scan_once(earliest - 1).await.unwrap();
            assert_eq!(
                restarted.queued_count().await,
                0,
                "do not retry before the base delay"
            );
            restarted.scan_once(deadline).await.unwrap();
            assert_eq!(
                restarted.queued_count().await,
                1,
                "an unchanged activity epoch must not suppress retry {retry}"
            );
            let queued = f.store.load(&f.task).unwrap();
            assert_ne!(queued.action.id, previous_action);
            restarted.scan_once(deadline).await.unwrap();
            assert_eq!(
                restarted.queued_count().await,
                1,
                "only one pending wake per goal"
            );
            restarted.run_next(deadline + 1).await.unwrap();
            previous_action = queued.action.id;
        }
        {
            let executions = f.executor.executions.lock().unwrap();
            assert_eq!(executions.len(), 4);
            assert!(executions.iter().all(|e| matches!(
                e.request.prompt,
                ActionPrompt::OfflineGoal { .. }
            ) && e.unread_message_ids.is_empty()));
        }
        assert_eq!(f.store.load(&f.task).unwrap().goal.unwrap().turn_count, 5);
        let exhausted_at =
            f.store.load(&f.task).unwrap().action.updated_at_ms.unwrap() + 86_400_000;
        let restarted = AgentAutomationSupervisor::new(
            f.config.markdown_chat.automation.clone(),
            f.executor.clone(),
        );
        restarted
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                Arc::new(AgentAutomationStore::for_config(&f.config).unwrap()),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        restarted.scan_once(exhausted_at).await.unwrap();
        assert_eq!(restarted.queued_count().await, 0);
        let exhausted = f.store.load(&f.task).unwrap();
        assert!(exhausted.automatic_retry_at_ms().is_none());
        assert!(exhausted.action_needs_attention());
        assert_eq!(exhausted.retry_mode(), Some("continue_goal"));
        assert_eq!(f.executor.calls.load(Ordering::SeqCst), 4);
        restarted
            .retry_failed_action(
                &f.task,
                exhausted.action.id.as_deref().unwrap(),
                exhausted_at + 1,
            )
            .await
            .unwrap();
        restarted.run_next(exhausted_at + 2).await.unwrap();
        let resumed = f.store.load(&f.task).unwrap();
        assert_eq!(resumed.goal_wake.recovery_attempts, 0);
        assert_ne!(resumed.action.id, exhausted.action.id);
        let retry_at = resumed
            .goal_wake
            .last_nudged_at_ms
            .unwrap()
            .max(resumed.action.updated_at_ms.unwrap())
            + 66_000;
        restarted.scan_once(retry_at).await.unwrap();
        assert_eq!(restarted.queued_count().await, 1);
        restarted.run_next(retry_at + 1).await.unwrap();
        assert_eq!(
            f.store.load(&f.task).unwrap().goal_wake.recovery_attempts,
            1
        );
        // New agent activity, rather than another accepted Send, resets recovery.
        f.chat.record_agent_call(retry_at + 10).await.unwrap();
        restarted.scan_once(retry_at + 10).await.unwrap();
        assert_eq!(
            f.store.load(&f.task).unwrap().goal_wake.recovery_attempts,
            0
        );
        restarted
            .scan_once(retry_at + 10 + AUTOMATION_OFFLINE_AFTER_MS)
            .await
            .unwrap();
        assert_eq!(restarted.queued_count().await, 1);
    }
}

#[tokio::test]
async fn cancelled_goal_only_action_does_not_strand_an_active_objective() {
    use codexify::agent_automation::AutomationActionKind;
    for later_activity in [false, true] {
        let f = fixture().await;
        let goal = f
            .store
            .set_goal(&f.task, "Keep pursuing the goal", 100)
            .unwrap();
        f.store.run_goal(&f.task, goal.revision, 101).unwrap();
        f.store
            .queue_action(
                &f.task,
                "cancelled-goal",
                AutomationActionKind::OfflineGoal,
                1,
                Some(goal.revision),
                1000,
            )
            .unwrap();
        f.store
            .cancel_action(&f.task, "cancelled-goal", 1001)
            .unwrap();
        if later_activity {
            f.chat.record_agent_call(1010).await.unwrap();
        }
        let deadline = if later_activity {
            1010 + AUTOMATION_OFFLINE_AFTER_MS
        } else {
            1001 + 66_000
        };
        let restarted = AgentAutomationSupervisor::new(
            f.config.markdown_chat.automation.clone(),
            f.executor.clone(),
        );
        restarted
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                f.store.clone(),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        restarted
            .scan_once(if later_activity {
                deadline - 1
            } else {
                1001 + 60_000 - 1
            })
            .await
            .unwrap();
        assert_eq!(restarted.queued_count().await, 0);
        restarted.scan_once(deadline).await.unwrap();
        assert_eq!(restarted.queued_count().await, 1);
        assert_eq!(
            restarted.next_queued_kind().await,
            Some(AutomationActionKind::OfflineGoal)
        );
        restarted.run_next(deadline + 1).await.unwrap();
        assert_eq!(f.executor.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn offline_goal_retry_respects_activity_pause_stop_and_completion() {
    use codexify::agent_automation::GoalFinish;
    for change in ["online", "paused", "stopped", "blocked", "completed"] {
        let f = fixture().await;
        let goal = f
            .store
            .set_goal(&f.task, "Keep pursuing the goal", 100)
            .unwrap();
        f.store.run_goal(&f.task, goal.revision, 101).unwrap();
        f.supervisor.scan_once(1000).await.unwrap();
        f.supervisor.run_next(1001).await.unwrap();
        let deadline = f
            .store
            .load(&f.task)
            .unwrap()
            .goal_wake
            .last_nudged_at_ms
            .unwrap()
            + 66_000;
        match change {
            "online" => {
                f.chat.record_agent_call(deadline - 1).await.unwrap();
            }
            "paused" => {
                f.store.stop_goal(&f.task, goal.revision, 1002).unwrap();
            }
            "stopped" => {
                f.chat.request_agent_stop("stop-goal".into()).await.unwrap();
            }
            "blocked" => {
                f.store
                    .finish_goal(
                        &f.task,
                        goal.revision,
                        GoalFinish::Blocked {
                            reason: "Need input".into(),
                        },
                        1002,
                    )
                    .unwrap();
            }
            "completed" => {
                f.store
                    .finish_goal(
                        &f.task,
                        goal.revision,
                        GoalFinish::Completed { summary: None },
                        1002,
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        f.supervisor.scan_once(deadline).await.unwrap();
        assert_eq!(f.supervisor.queued_count().await, 0, "{change}");
        assert_eq!(f.executor.calls.load(Ordering::SeqCst), 1, "{change}");
    }
}

#[tokio::test]
async fn offline_goal_retry_never_replays_a_previously_accepted_user_batch() {
    let f = fixture().await;
    let goal = f.store.set_goal(&f.task, "Complete the goal", 100).unwrap();
    f.store.run_goal(&f.task, goal.revision, 101).unwrap();
    f.chat
        .append_user("accepted-once".into(), "Keep this instruction".into())
        .await
        .unwrap();
    f.supervisor.scan_once(1000).await.unwrap();
    f.supervisor.run_next(1001).await.unwrap();
    let accepted = f.store.load(&f.task).unwrap();
    assert!(accepted.action.messages_fenced);
    for delay in [60_000, 120_000, 240_000] {
        let state = f.store.load(&f.task).unwrap();
        let earliest = state
            .goal_wake
            .last_nudged_at_ms
            .unwrap()
            .max(state.action.updated_at_ms.unwrap())
            + delay;
        let deadline = state
            .automatic_retry_at_ms()
            .unwrap_or(earliest + delay / 10);
        f.supervisor.scan_once(earliest - 1).await.unwrap();
        assert_eq!(f.supervisor.queued_count().await, 0);
        f.supervisor.scan_once(deadline).await.unwrap();
        f.supervisor.run_next(deadline + 1).await.unwrap();
    }
    f.supervisor.scan_once(86_400_000).await.unwrap();
    assert_eq!(f.supervisor.queued_count().await, 0);
    let executions = f.executor.executions.lock().unwrap();
    assert_eq!(executions.len(), 4);
    assert_eq!(executions[0].unread_message_ids, vec!["accepted-once"]);
    assert!(
        executions[1..]
            .iter()
            .all(|execution| execution.unread_message_ids.is_empty()
                && matches!(execution.request.prompt, ActionPrompt::OfflineGoal { .. }))
    );
    assert_eq!(
        f.store.load(&f.task).unwrap().chat_mirror,
        accepted.chat_mirror
    );
}

#[tokio::test]
async fn resumed_goal_activity_clears_retry_cooldowns_across_restart() {
    for failure in [
        "failed",
        "authentication_preflight",
        "conversation_snapshot",
        "ambiguous",
    ] {
        let f = fixture().await;
        let goal = f.store.set_goal(&f.task, "Resume this goal", 100).unwrap();
        f.store.run_goal(&f.task, goal.revision, 101).unwrap();
        f.chat.record_agent_call(1_000).await.unwrap();
        let executor: Arc<dyn AutomationExecutor> = match failure {
            "failed" => Arc::new(PreSendFailureExecutor::default()),
            "ambiguous" => Arc::new(AmbiguousExecutor),
            _ => Arc::new(AuthenticationRetryExecutor {
                calls: AtomicUsize::new(0),
                recovered: AtomicBool::new(false),
                failure,
            }),
        };
        let supervisor = AgentAutomationSupervisor::new(
            f.config.markdown_chat.automation.clone(),
            executor.clone(),
        );
        supervisor
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                f.store.clone(),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        let first = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
        supervisor.scan_once(first).await.unwrap();
        let _ = supervisor.run_next(first + 1).await;
        let failed = f.store.load(&f.task).unwrap();
        assert!(failed.action.last_error.is_some(), "{failure}");
        let retry_at = failed.automatic_retry_at_ms().unwrap();
        let resumed_at = failed.action.updated_at_ms.unwrap() + 10;
        assert!(resumed_at < retry_at);
        f.chat.record_agent_call(resumed_at).await.unwrap();

        let restarted =
            AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor);
        restarted
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                Arc::new(AgentAutomationStore::for_config(&f.config).unwrap()),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        restarted.scan_once(resumed_at + 1).await.unwrap();
        let recovered = f.store.load(&f.task).unwrap();
        assert_eq!(
            recovered.action.status,
            AutomationActionStatus::Idle,
            "{failure}"
        );
        assert!(recovered.action.last_error.is_none());
        assert!(recovered.automatic_retry_at_ms().is_none());
        assert!(!recovered.is_automatically_retrying());
        assert_eq!(recovered.action.automatic_retry_attempts, 0);
        assert_eq!(recovered.action.chatgpt_access_retry_attempts, 0);
        assert_eq!(
            recovered.action.send_accepted,
            if failure == "ambiguous" {
                Some(true)
            } else {
                failed.action.send_accepted
            }
        );
        assert_eq!(
            recovered.action.messages_fenced,
            failed.action.messages_fenced
        );
        assert_eq!(recovered.chat_mirror, failed.chat_mirror);
        assert_eq!(recovered.goal.as_ref().unwrap().revision, goal.revision);
        assert_eq!(recovered.goal.as_ref().unwrap().markdown, goal.markdown);
        assert_eq!(
            recovered.goal.as_ref().unwrap().turn_count,
            failed.goal.as_ref().unwrap().turn_count
                + u64::from(failure == "ambiguous" && !failed.action.goal_turn_counted)
        );
        assert!(restarted.live_state(&f.task).await.is_none());
        restarted.scan_once(retry_at).await.unwrap();
        assert_eq!(restarted.queued_count().await, 0);
        assert!(restarted.run_next(retry_at + 1).await.unwrap().is_none());
        let reopened = AgentAutomationStore::for_config(&f.config)
            .unwrap()
            .load(&f.task)
            .unwrap();
        assert!(reopened.action.last_error.is_none());
        assert!(reopened.automatic_retry_at_ms().is_none());

        // The active goal can still receive a normal wake after later inactivity.
        restarted
            .scan_once(resumed_at + AUTOMATION_OFFLINE_AFTER_MS)
            .await
            .unwrap();
        assert_eq!(
            restarted.next_queued_kind().await,
            Some(codexify::agent_automation::AutomationActionKind::OfflineGoal)
        );
        assert!(!f.store.load(&f.task).unwrap().is_automatically_retrying());
    }
}

#[tokio::test]
async fn resumed_goal_activity_retires_an_already_queued_retry_before_scan_or_dispatch() {
    for reconcile_by_scan in [true, false] {
        let f = fixture().await;
        let goal = f.store.set_goal(&f.task, "Resume this goal", 100).unwrap();
        f.store.run_goal(&f.task, goal.revision, 101).unwrap();
        let executor = Arc::new(PreSendFailureExecutor::default());
        let supervisor = AgentAutomationSupervisor::new(
            f.config.markdown_chat.automation.clone(),
            executor.clone(),
        );
        supervisor
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                f.store.clone(),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        supervisor.scan_once(1_000).await.unwrap();
        assert!(supervisor.run_next(1_001).await.is_err());
        let retry_at = f
            .store
            .load(&f.task)
            .unwrap()
            .automatic_retry_at_ms()
            .unwrap();
        supervisor.scan_once(retry_at).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 1);
        assert!(f.store.load(&f.task).unwrap().is_automatically_retrying());
        f.chat.record_agent_call(retry_at + 1).await.unwrap();
        if reconcile_by_scan {
            supervisor.scan_once(retry_at + 2).await.unwrap();
        }
        assert!(supervisor.run_next(retry_at + 3).await.unwrap().is_none());
        let recovered = f.store.load(&f.task).unwrap();
        assert_eq!(supervisor.queued_count().await, 0);
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            1,
            "the queued continuation must never execute"
        );
        assert_eq!(recovered.action.status, AutomationActionStatus::Idle);
        assert!(recovered.action.last_error.is_none());
        assert!(!recovered.is_automatically_retrying());
        assert!(supervisor.live_state(&f.task).await.is_none());
    }
}

#[tokio::test]
async fn resumed_goal_activity_requires_fresh_agent_activity_and_preserves_new_user_messages() {
    let f = fixture().await;
    let goal = f.store.set_goal(&f.task, "Resume this goal", 100).unwrap();
    f.store.run_goal(&f.task, goal.revision, 101).unwrap();
    f.chat.record_agent_call(1_000).await.unwrap();
    let supervisor = AgentAutomationSupervisor::new(
        f.config.markdown_chat.automation.clone(),
        Arc::new(PreSendFailureExecutor::default()),
    );
    supervisor
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    let first = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
    supervisor.scan_once(first).await.unwrap();
    assert!(supervisor.run_next(first + 1).await.is_err());
    let failed = f.store.load(&f.task).unwrap();
    let failed_at = failed.action.updated_at_ms.unwrap();
    f.chat
        .append_user(
            "new-pending-message".into(),
            "Keep this message for delivery".into(),
        )
        .await
        .unwrap();
    supervisor.scan_once(failed_at + 2).await.unwrap();
    assert_eq!(
        f.store.load(&f.task).unwrap().action,
        failed.action,
        "outgoing messages are not recovery proof"
    );
    f.chat.record_agent_call(failed_at - 1).await.unwrap();
    supervisor.scan_once(failed_at + 3).await.unwrap();
    assert_eq!(
        f.store.load(&f.task).unwrap().action,
        failed.action,
        "activity older than the failure is not recovery proof"
    );
    f.chat.record_agent_call(failed_at + 4).await.unwrap();
    supervisor.scan_once(failed_at + 5).await.unwrap();
    let recovered = f.store.load(&f.task).unwrap();
    assert!(recovered.action.last_error.is_none());
    assert!(!recovered.is_automatically_retrying());
    assert_eq!(recovered.chat_mirror, failed.chat_mirror);
    assert!(
        f.chat
            .unread_snapshot()
            .await
            .unwrap()
            .messages
            .iter()
            .any(|m| m.id == "new-pending-message")
    );
}

#[tokio::test]
async fn resumed_goal_activity_never_treats_an_unsent_user_batch_as_delivered() {
    for with_goal in [false, true] {
        let f = fixture().await;
        if with_goal {
            let goal = f
                .store
                .set_goal(&f.task, "Keep the pending instruction", 100)
                .unwrap();
            f.store.run_goal(&f.task, goal.revision, 101).unwrap();
        }
        f.chat
            .append_user(
                "pending-instruction".into(),
                "Keep this unsent message".into(),
            )
            .await
            .unwrap();
        let executor = Arc::new(PreSendFailureExecutor::default());
        let supervisor =
            AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor);
        supervisor
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                f.store.clone(),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        supervisor.scan_once(1_000).await.unwrap();
        assert!(supervisor.run_next(1_001).await.is_err());
        let failed = f.store.load(&f.task).unwrap();
        let resumed_at = failed.action.updated_at_ms.unwrap() + 10;
        f.chat.record_agent_call(resumed_at).await.unwrap();
        supervisor.scan_once(resumed_at + 1).await.unwrap();
        let after = f.store.load(&f.task).unwrap();
        assert_eq!(after.action, failed.action);
        assert_eq!(after.chat_mirror, failed.chat_mirror);
        assert!(after.automatic_retry_at_ms().is_some());
        assert!(
            f.chat
                .unread_snapshot()
                .await
                .unwrap()
                .messages
                .iter()
                .any(|m| m.id == "pending-instruction")
        );
    }
}

#[tokio::test]
async fn offline_goal_pre_send_failures_stop_after_five_retries_and_allow_manual_retry() {
    let f = fixture().await;
    let goal = f
        .store
        .set_goal(&f.task, "Keep pursuing the goal", 100)
        .unwrap();
    f.store.run_goal(&f.task, goal.revision, 101).unwrap();
    let executor = Arc::new(PreSendFailureExecutor::default());
    let mut supervisor =
        AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor.clone());
    supervisor
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    let mut now = 1000;
    for (attempt, delay) in [
        (0, 30_000),
        (1, 60_000),
        (2, 120_000),
        (3, 240_000),
        (4, 480_000),
    ] {
        supervisor.scan_once(now).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 1);
        assert!(supervisor.run_next(now + 1).await.is_err());
        let failed = f.store.load(&f.task).unwrap();
        let retry_at = failed.automatic_retry_at_ms().unwrap();
        assert_eq!(failed.action.automatic_retry_attempts, attempt);
        assert!(
            (failed.action.updated_at_ms.unwrap() + delay
                ..=failed.action.updated_at_ms.unwrap() + delay + delay / 10)
                .contains(&retry_at)
        );
        // Reopening the store and supervisor must retain the retry stage.
        supervisor = AgentAutomationSupervisor::new(
            f.config.markdown_chat.automation.clone(),
            executor.clone(),
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
        supervisor.scan_once(retry_at - 1).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 0);
        now = retry_at;
    }
    supervisor.scan_once(now).await.unwrap();
    assert!(supervisor.run_next(now + 1).await.is_err());
    let exhausted = f.store.load(&f.task).unwrap();
    assert!(exhausted.automatic_retry_at_ms().is_none());
    supervisor.scan_once(now + 86_400_000).await.unwrap();
    assert_eq!(supervisor.queued_count().await, 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 6);
    assert_eq!(
        f.store.load(&f.task).unwrap().goal.unwrap().turn_count,
        1,
        "a failed Send does not count as another goal turn"
    );
    f.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::SubmissionComplete {
            canonical_url: "https://chatgpt.com/c/example".into(),
        },
    ]);
    f.supervisor
        .retry_failed_action(
            &f.task,
            exhausted.action.id.as_deref().unwrap(),
            now + 86_400_001,
        )
        .await
        .unwrap();
    f.supervisor.run_next(now + 86_400_002).await.unwrap();
    let resumed = f.store.load(&f.task).unwrap();
    assert_eq!(resumed.action.send_accepted, Some(true));
    assert_eq!(resumed.action.automatic_retry_attempts, 0);
}

#[tokio::test]
async fn missing_connector_keeps_goal_work_pending_until_an_exact_name_is_saved() {
    let f = fixture().await;
    let path = f._root.path().join("connector-settings.json");
    std::fs::write(&path, "{}").unwrap();
    let mut config = f.config.clone();
    config.loaded_config_path = Some(path.clone());
    config.markdown_chat.automation.connector_name.clear();
    let store = Arc::new(AgentAutomationStore::for_config(&config).unwrap());
    let goal = store
        .set_goal(&f.task, "Keep this goal pending", 100)
        .unwrap();
    store.run_goal(&f.task, goal.revision, 101).unwrap();
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
    supervisor.scan_once(1000).await.unwrap();
    assert_eq!(supervisor.queued_count().await, 0);
    assert_eq!(
        store.load(&f.task).unwrap().goal.unwrap().status,
        codexify::agent_automation::GoalStatus::Active
    );
    std::fs::write(
        &path,
        r#"{"agentChat":{"automation":{"connectorName":"First exact name"}}}"#,
    )
    .unwrap();
    supervisor.scan_once(1001).await.unwrap();
    assert_eq!(supervisor.queued_count().await, 1);
    std::fs::write(&path, "{}").unwrap();
    assert!(
        supervisor
            .run_next(1002)
            .await
            .err()
            .unwrap()
            .contains("connector name")
    );
    assert_eq!(f.executor.calls.load(Ordering::SeqCst), 0);
    let failed = store.load(&f.task).unwrap();
    assert_eq!(failed.action.send_accepted, Some(false));
    std::fs::write(
        &path,
        r#"{"agentChat":{"automation":{"connectorName":"Saved exact name"}}}"#,
    )
    .unwrap();
    f.executor.events.lock().unwrap().extend([
        CompanionEventKind::SendAccepted,
        CompanionEventKind::SubmissionComplete {
            canonical_url: "https://chatgpt.com/c/example".into(),
        },
    ]);
    let retry_at = failed.automatic_retry_at_ms().unwrap();
    supervisor.scan_once(retry_at).await.unwrap();
    supervisor.run_next(retry_at + 1).await.unwrap();
    let executions = f.executor.executions.lock().unwrap();
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0].request.connector_name, "Saved exact name");
}

#[tokio::test]
async fn completing_a_goal_during_automatic_recovery_clears_its_retry_indicator() {
    let f = orphaned_goal_fixture(false).await;
    let (restarted, _) = restarted_goal_supervisor(&f, None).await;
    let now = 2001 + 32 * 60 * 1000;
    restarted.scan_once(now).await.unwrap();
    let queued = f.store.load(&f.task).unwrap();
    assert!(queued.is_automatically_retrying());
    f.store
        .finish_goal(
            &f.task,
            queued.goal.unwrap().revision,
            codexify::agent_automation::GoalFinish::Completed { summary: None },
            now + 1,
        )
        .unwrap();
    let completed = f.store.load(&f.task).unwrap();
    assert_eq!(completed.action.status, AutomationActionStatus::Idle);
    assert!(!completed.is_automatically_retrying());
}

struct AuthenticationRetryExecutor {
    calls: AtomicUsize,
    recovered: AtomicBool,
    failure: &'static str,
}
#[async_trait]
impl AutomationExecutor for AuthenticationRetryExecutor {
    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        _cancel: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.recovered.load(Ordering::SeqCst) {
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
                        at_ms: index as u64 + 1,
                        kind,
                    })
                    .unwrap();
            }
            return Ok(snapshot_for(execution));
        }
        let rate_limited = self.failure.starts_with("rate_limit");
        if matches!(
            self.failure,
            "authentication_preflight" | "rate_limit_preflight"
        ) {
            return Err(AutomationExecutionError {
                message: "Authentication unavailable before browser execution".into(),
                send_accepted: false,
                failure_status: Some(if rate_limited {
                    AutomationActionStatus::RateLimited
                } else {
                    AutomationActionStatus::AuthenticationRequired
                }),
            });
        }
        let mut snapshot = snapshot_for(execution);
        let editor_failure = self.failure.starts_with("editor");
        let access_failure = self.failure.starts_with("conversation") || editor_failure;
        snapshot.state = if rate_limited {
            CompanionActionState::RateLimited
        } else if access_failure {
            CompanionActionState::Failed
        } else {
            CompanionActionState::AuthenticationRequired
        };
        snapshot.send_accepted = self.failure == "editor_accepted";
        snapshot.last_event_sequence = 0;
        snapshot.attention = Some(codexify::agent_automation::CompanionAttention {
            code: if rate_limited {
                "rate_limited"
            } else if editor_failure {
                "editor_unavailable"
            } else if access_failure {
                "conversation_unavailable"
            } else {
                "authentication_required"
            }
            .into(),
            message: Some(
                if rate_limited {
                    "ChatGPT rejected the request with HTTP 429"
                } else if editor_failure {
                    "ChatGPT did not accept the message in its editor; nothing was sent"
                } else if access_failure {
                    "ChatGPT conversation temporarily not accessible"
                } else {
                    "Sign in to ChatGPT"
                }
                .into(),
            ),
        });
        if matches!(
            self.failure,
            "conversation_stream" | "editor_stream" | "rate_limit_stream"
        ) {
            events
                .send(CompanionEvent {
                    sequence: 1,
                    action_id: snapshot.action_id.clone(),
                    at_ms: 1,
                    kind: if rate_limited {
                        CompanionEventKind::RateLimited {
                            message: snapshot.attention.as_ref().unwrap().message.clone(),
                        }
                    } else {
                        CompanionEventKind::Failed {
                            code: snapshot.attention.as_ref().unwrap().code.clone(),
                            message: snapshot
                                .attention
                                .as_ref()
                                .unwrap()
                                .message
                                .clone()
                                .unwrap(),
                        }
                    },
                })
                .unwrap();
            snapshot.last_event_sequence = 1;
        }
        Ok(snapshot)
    }
}

#[tokio::test]
async fn chatgpt_access_retries_persist_across_restart_stop_after_six_and_reset_after_recovery() {
    for mode in ["chat", "goal", "combined"] {
        for failure in [
            "authentication_preflight",
            "authentication_snapshot",
            "conversation_snapshot",
            "conversation_stream",
            "editor_snapshot",
            "editor_stream",
            "rate_limit_preflight",
            "rate_limit_snapshot",
            "rate_limit_stream",
        ] {
            let f = fixture().await;
            if mode != "chat" {
                let goal = f
                    .store
                    .set_goal(&f.task, "Continue after authentication recovers", 100)
                    .unwrap();
                f.store.run_goal(&f.task, goal.revision, 101).unwrap();
            }
            f.chat.record_agent_call(1_000).await.unwrap();
            if mode != "goal" {
                f.chat
                    .append_user(
                        "authentication-message".into(),
                        "Keep this message until it can be sent".into(),
                    )
                    .await
                    .unwrap();
            }
            let executor = Arc::new(AuthenticationRetryExecutor {
                calls: AtomicUsize::new(0),
                recovered: AtomicBool::new(false),
                failure,
            });
            let mut supervisor = AgentAutomationSupervisor::new(
                f.config.markdown_chat.automation.clone(),
                executor.clone(),
            );
            supervisor
                .register_task(AutomationTaskHandle::new(
                    f.task.clone(),
                    0,
                    f.store.clone(),
                    f.chat.clone(),
                ))
                .await
                .unwrap();
            let mut now = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
            supervisor.scan_once(now).await.unwrap();
            let _ = supervisor.run_next(now + 1).await;
            for (attempt, minutes) in [3, 5, 10, 20, 30, 60].into_iter().enumerate() {
                let failed = f.store.load(&f.task).unwrap();
                assert_eq!(
                    failed.action.status,
                    if failure.starts_with("rate_limit") {
                        AutomationActionStatus::RateLimited
                    } else if failure.starts_with("conversation") || failure.starts_with("editor") {
                        AutomationActionStatus::ConversationUnavailable
                    } else {
                        AutomationActionStatus::AuthenticationRequired
                    }
                );
                if failure.starts_with("editor") {
                    assert_eq!(
                        failed.action.last_error.as_deref(),
                        Some("ChatGPT did not accept the message in its editor; nothing was sent")
                    );
                }
                assert_eq!(failed.action.chatgpt_access_retry_attempts, attempt as u32);
                assert_eq!(failed.automatic_retry_attempts(), attempt as u32);
                assert_eq!(failed.automatic_retry_limit(), 6);
                let deadline = failed.automatic_retry_at_ms().unwrap();
                let delay = minutes * 60_000;
                assert!(
                    (failed.action.updated_at_ms.unwrap() + delay
                        ..=failed.action.updated_at_ms.unwrap() + delay + delay / 10)
                        .contains(&deadline)
                );
                assert_eq!(
                    failed.action.automatic_retry_attempts, 0,
                    "access failures must not consume the short retry budget"
                );
                let reopened = Arc::new(AgentAutomationStore::for_config(&f.config).unwrap());
                assert_eq!(
                    reopened.load(&f.task).unwrap().automatic_retry_at_ms(),
                    Some(deadline)
                );
                supervisor = AgentAutomationSupervisor::new(
                    f.config.markdown_chat.automation.clone(),
                    executor.clone(),
                );
                supervisor
                    .register_task(AutomationTaskHandle::new(
                        f.task.clone(),
                        0,
                        reopened,
                        f.chat.clone(),
                    ))
                    .await
                    .unwrap();
                supervisor.scan_once(deadline - 1).await.unwrap();
                assert_eq!(supervisor.queued_count().await, 0);
                supervisor.scan_once(deadline).await.unwrap();
                assert_eq!(supervisor.queued_count().await, 1);
                let queued = f.store.load(&f.task).unwrap();
                assert_eq!(queued.automatic_retry_limit(), 6);
                assert_eq!(queued.automatic_retry_attempts(), attempt as u32 + 1);
                assert!(queued.is_automatically_retrying());
                let _ = supervisor.run_next(deadline + 1).await;
                now = deadline + 1;
            }
            let exhausted = f.store.load(&f.task).unwrap();
            assert_eq!(exhausted.action.chatgpt_access_retry_attempts, 6);
            assert!(exhausted.automatic_retry_at_ms().is_none());
            assert!(!exhausted.is_automatically_retrying());
            supervisor.scan_once(now + 86_400_000).await.unwrap();
            assert_eq!(supervisor.queued_count().await, 0);
            assert_eq!(executor.calls.load(Ordering::SeqCst), 7);
            assert_eq!(
                f.chat.unread_snapshot().await.unwrap().messages.len(),
                usize::from(mode != "goal")
            );
            executor.recovered.store(true, Ordering::SeqCst);
            supervisor
                .retry_failed_action(
                    &f.task,
                    exhausted.action.id.as_deref().unwrap(),
                    now + 86_400_001,
                )
                .await
                .unwrap();
            supervisor.run_next(now + 86_400_002).await.unwrap();
            let recovered = f.store.load(&f.task).unwrap();
            assert_eq!(recovered.action.status, AutomationActionStatus::Idle);
            assert_eq!(recovered.action.chatgpt_access_retry_attempts, 0);
            assert!(!recovered.action.chatgpt_access_retry_in_progress);
        }
    }
}

#[tokio::test]
async fn editor_failure_with_send_acceptance_is_ambiguous_and_never_replays_user_messages() {
    let f = fixture().await;
    f.chat
        .append_user("editor-message".into(), "Keep this message".into())
        .await
        .unwrap();
    let executor = Arc::new(AuthenticationRetryExecutor {
        calls: AtomicUsize::new(0),
        recovered: AtomicBool::new(false),
        failure: "editor_accepted",
    });
    let supervisor =
        AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor.clone());
    supervisor
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    supervisor.scan_once(1_000).await.unwrap();
    supervisor.run_next(1_001).await.unwrap();
    let failed = f.store.load(&f.task).unwrap();
    assert_eq!(failed.action.status, AutomationActionStatus::Ambiguous);
    assert_eq!(failed.action.send_accepted, Some(true));
    assert!(failed.automatic_retry_at_ms().is_none());
    supervisor.scan_once(86_400_000).await.unwrap();
    assert_eq!(supervisor.queued_count().await, 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn exhausted_unsent_goal_failure_clears_only_after_fresh_agent_activity() {
    for failure in [
        "authentication_preflight",
        "conversation_snapshot",
        "editor_snapshot",
        "failed",
    ] {
        let f = fixture().await;
        let goal = f
            .store
            .set_goal(&f.task, "Recover after exhausted retries", 100)
            .unwrap();
        f.store.run_goal(&f.task, goal.revision, 101).unwrap();
        let executor: Arc<dyn AutomationExecutor> = if failure == "failed" {
            Arc::new(PreSendFailureExecutor::default())
        } else {
            Arc::new(AuthenticationRetryExecutor {
                calls: AtomicUsize::new(0),
                recovered: AtomicBool::new(false),
                failure,
            })
        };
        let supervisor = AgentAutomationSupervisor::new(
            f.config.markdown_chat.automation.clone(),
            executor.clone(),
        );
        supervisor
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                f.store.clone(),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        let mut at = 1_000;
        supervisor.scan_once(at).await.unwrap();
        let _ = supervisor.run_next(at + 1).await;
        while let Some(retry_at) = f.store.load(&f.task).unwrap().automatic_retry_at_ms() {
            at = retry_at;
            supervisor.scan_once(at).await.unwrap();
            let _ = supervisor.run_next(at + 1).await;
        }
        let exhausted = f.store.load(&f.task).unwrap();
        assert!(exhausted.action.last_error.is_some());
        assert_eq!(
            exhausted.automatic_retry_attempts() as usize,
            exhausted.automatic_retry_limit()
        );
        let failed_at = exhausted.action.updated_at_ms.unwrap();
        f.chat.record_agent_call(failed_at - 1).await.unwrap();
        supervisor.scan_once(failed_at + 2).await.unwrap();
        assert_eq!(
            f.store.load(&f.task).unwrap().action,
            exhausted.action,
            "an old online timestamp must not clear exhaustion"
        );
        f.chat.record_agent_call(failed_at + 3).await.unwrap();
        let restarted =
            AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor);
        restarted
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                Arc::new(AgentAutomationStore::for_config(&f.config).unwrap()),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        restarted.scan_once(failed_at + 4).await.unwrap();
        let recovered = f.store.load(&f.task).unwrap();
        assert_eq!(recovered.action.status, AutomationActionStatus::Idle);
        assert!(recovered.action.last_error.is_none());
        assert!(!recovered.action_needs_attention());
        assert!(!recovered.is_automatically_retrying());
        assert_eq!(recovered.action.send_accepted, Some(false));
        assert_eq!(
            recovered.action.messages_fenced,
            exhausted.action.messages_fenced
        );
        assert_eq!(recovered.chat_mirror, exhausted.chat_mirror);
        assert_eq!(
            recovered.goal.unwrap().turn_count,
            exhausted.goal.unwrap().turn_count
        );
        assert_eq!(restarted.queued_count().await, 0);
    }
}

#[tokio::test]
async fn pausing_or_dismissing_a_failure_stops_retries_persistently_and_preserves_user_messages() {
    for queued in [false, true] {
        for dismiss in [false, true] {
            for with_goal in [false, true] {
                let f = fixture().await;
                if with_goal {
                    let goal = f.store.set_goal(&f.task, "Keep this goal", 100).unwrap();
                    f.store.run_goal(&f.task, goal.revision, 101).unwrap();
                } else {
                    f.chat
                        .append_user("pending".into(), "Preserve this message".into())
                        .await
                        .unwrap();
                }
                let executor = Arc::new(PreSendFailureExecutor::default());
                let supervisor = AgentAutomationSupervisor::new(
                    f.config.markdown_chat.automation.clone(),
                    executor.clone(),
                );
                supervisor
                    .register_task(AutomationTaskHandle::new(
                        f.task.clone(),
                        0,
                        f.store.clone(),
                        f.chat.clone(),
                    ))
                    .await
                    .unwrap();
                supervisor.scan_once(1_000).await.unwrap();
                assert!(supervisor.run_next(1_001).await.is_err());
                let failed = f.store.load(&f.task).unwrap();
                let deadline = failed.automatic_retry_at_ms().unwrap();
                if queued {
                    supervisor.scan_once(deadline).await.unwrap();
                }
                let action_id = f.store.load(&f.task).unwrap().action.id.unwrap();
                if dismiss {
                    supervisor
                        .cancel_action(&f.task, &action_id, deadline + 1)
                        .await
                        .unwrap();
                } else {
                    supervisor
                        .pause_action_retries(&f.task, &action_id)
                        .await
                        .unwrap();
                }
                let saved = f.store.load(&f.task).unwrap();
                assert!(saved.action.automatic_retry_paused);
                assert_eq!(saved.action.last_error.is_none(), dismiss);
                assert_eq!(saved.action_needs_attention(), !dismiss);
                assert!(saved.automatic_retry_at_ms().is_none());
                assert_eq!(saved.chat_mirror, failed.chat_mirror);
                assert_eq!(saved.action.send_accepted, Some(false));
                assert_eq!(saved.goal, failed.goal);
                assert!(supervisor.run_next(deadline + 2).await.unwrap().is_none());
                let restarted = AgentAutomationSupervisor::new(
                    f.config.markdown_chat.automation.clone(),
                    executor.clone(),
                );
                restarted
                    .register_task(AutomationTaskHandle::new(
                        f.task.clone(),
                        0,
                        Arc::new(AgentAutomationStore::for_config(&f.config).unwrap()),
                        f.chat.clone(),
                    ))
                    .await
                    .unwrap();
                restarted.scan_once(deadline + 86_400_000).await.unwrap();
                assert_eq!(restarted.queued_count().await, 0);
                assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
                if !with_goal {
                    assert_eq!(f.chat.unread_snapshot().await.unwrap().messages.len(), 1);
                }
                // Retry is explicit new intent even when automatic retries were paused.
                restarted
                    .retry_failed_action(
                        &f.task,
                        saved.action.id.as_deref().unwrap(),
                        deadline + 86_400_001,
                    )
                    .await
                    .unwrap();
                assert_eq!(restarted.queued_count().await, 1);
                assert!(!f.store.load(&f.task).unwrap().action.automatic_retry_paused);
            }
        }
    }
}

#[tokio::test]
async fn global_auto_retry_policy_is_hot_and_stops_queued_work_but_allows_manual_retry() {
    for checkpoint in ["scan", "dispatch", "restart"] {
        let f = fixture().await;
        let path = f._root.path().join("retry-settings.json");
        let write_policy = |enabled| {
            std::fs::write(&path, serde_json::json!({"agentChat":{"automation":{"connectorName":"Fixture connector","automaticRetries":enabled}}}).to_string()).unwrap()
        };
        write_policy(true);
        let mut config = f.config.clone();
        config.loaded_config_path = Some(path.clone());
        let store = Arc::new(AgentAutomationStore::for_config(&config).unwrap());
        let goal = store.set_goal(&f.task, "Keep this goal", 100).unwrap();
        store.run_goal(&f.task, goal.revision, 101).unwrap();
        let executor = Arc::new(AuthenticationRetryExecutor {
            calls: AtomicUsize::new(0),
            recovered: AtomicBool::new(false),
            failure: "authentication_preflight",
        });
        let mut supervisor = AgentAutomationSupervisor::new(
            config.markdown_chat.automation.clone(),
            executor.clone(),
        );
        supervisor
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                store.clone(),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        supervisor.scan_once(1_000).await.unwrap();
        let _ = supervisor.run_next(1_001).await;
        let failed = store.load(&f.task).unwrap();
        let deadline = failed.automatic_retry_at_ms().unwrap();
        supervisor.scan_once(deadline).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 1);
        write_policy(false);
        if checkpoint == "scan" {
            supervisor.scan_once(deadline + 1).await.unwrap();
        } else if checkpoint == "restart" {
            supervisor = AgentAutomationSupervisor::new(
                config.markdown_chat.automation.clone(),
                executor.clone(),
            );
            supervisor
                .register_task(AutomationTaskHandle::new(
                    f.task.clone(),
                    0,
                    Arc::new(AgentAutomationStore::for_config(&config).unwrap()),
                    f.chat.clone(),
                ))
                .await
                .unwrap();
        }
        assert!(supervisor.run_next(deadline + 2).await.unwrap().is_none());
        let stopped = store.load(&f.task).unwrap();
        assert_eq!(
            stopped.action, failed.action,
            "restore the original failure without consuming a retry or losing its error type"
        );
        assert_eq!(stopped.goal_wake, failed.goal_wake);
        assert!(!stopped.automatic_retries_enabled);
        assert!(stopped.automatic_retry_at_ms().is_none());
        supervisor.scan_once(deadline + 86_400_000).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 0);
        // Explicit retry is still authorized with the global policy off.
        supervisor
            .retry_failed_action(
                &f.task,
                stopped.action.id.as_deref().unwrap(),
                deadline + 86_400_001,
            )
            .await
            .unwrap();
        assert_eq!(supervisor.queued_count().await, 1);
        let _ = supervisor.run_next(deadline + 86_400_002).await;
        let failed_again = store.load(&f.task).unwrap();
        assert!(failed_again.automatic_retry_at_ms().is_none());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
        write_policy(true);
        assert!(
            store
                .load(&f.task)
                .unwrap()
                .automatic_retry_at_ms()
                .is_some()
        );
    }
}

#[tokio::test]
async fn disabling_retries_keeps_normal_goal_wakes_working() {
    let f = fixture().await;
    let mut config = f.config.clone();
    config.markdown_chat.automation.automatic_retries = false;
    let store = Arc::new(AgentAutomationStore::for_config(&config).unwrap());
    let goal = store.set_goal(&f.task, "Normal goal wakes", 100).unwrap();
    store.run_goal(&f.task, goal.revision, 101).unwrap();
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
    supervisor.scan_once(1_000).await.unwrap();
    assert_eq!(supervisor.queued_count().await, 1);
    supervisor.run_next(1_001).await.unwrap();
    let sent = store.load(&f.task).unwrap();
    assert!(sent.goal_wake.last_nudged_at_ms.is_some());
    assert!(sent.automatic_retry_at_ms().is_none());
    supervisor.scan_once(86_400_000).await.unwrap();
    assert_eq!(
        supervisor.queued_count().await,
        0,
        "do not retry an accepted goal wake that has no agent response"
    );
    f.chat.record_agent_call(86_400_001).await.unwrap();
    supervisor.scan_once(86_400_002).await.unwrap();
    supervisor
        .scan_once(86_400_001 + AUTOMATION_OFFLINE_AFTER_MS)
        .await
        .unwrap();
    assert_eq!(
        supervisor.queued_count().await,
        1,
        "fresh activity permits the next normal goal wake"
    );
}

struct ApprovalOnOpenExecutor {
    opened: AtomicBool,
    approvals: AtomicUsize,
    visits: AtomicUsize,
}
#[async_trait]
impl AutomationExecutor for ApprovalOnOpenExecutor {
    async fn approve_pending_conversation(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<bool, String> {
        if self.opened.swap(false, Ordering::SeqCst) {
            self.approvals.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        } else {
            Ok(false)
        }
    }
    async fn execute(
        &self,
        execution: AutomationExecution,
        _: mpsc::UnboundedSender<CompanionEvent>,
        _: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        self.visits.fetch_add(1, Ordering::SeqCst);
        self.opened.store(true, Ordering::SeqCst);
        let mut snapshot = snapshot_for(execution);
        snapshot.state = CompanionActionState::ApprovalRequired;
        snapshot.send_accepted = false;
        snapshot.selected = None;
        snapshot.attention = Some(codexify::agent_automation::CompanionAttention {
            code: "approval_required".into(),
            message: Some("Materialization needs approval".into()),
        });
        Ok(snapshot)
    }
}

#[tokio::test]
async fn approval_found_on_open_resumes_existing_turn_and_waits_across_restart_for_first_tool_call()
{
    for with_message in [false, true] {
        let f = fixture().await;
        let goal = f
            .store
            .set_goal(&f.task, "Continue after materialization", 100)
            .unwrap();
        f.store.run_goal(&f.task, goal.revision, 101).unwrap();
        if with_message {
            f.chat
                .append_user(
                    "unread-during-approval".into(),
                    "Keep this unread until the agent reads it".into(),
                )
                .await
                .unwrap();
        }
        let executor = Arc::new(ApprovalOnOpenExecutor {
            opened: AtomicBool::new(false),
            approvals: AtomicUsize::new(0),
            visits: AtomicUsize::new(0),
        });
        let supervisor = AgentAutomationSupervisor::new(
            f.config.markdown_chat.automation.clone(),
            executor.clone(),
        );
        supervisor
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                f.store.clone(),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        let initial_turns = f.store.load(&f.task).unwrap().goal.unwrap().turn_count;
        supervisor.scan_once(1_000).await.unwrap();
        assert_eq!(supervisor.queued_count().await, 1);
        supervisor.run_next(1_001).await.unwrap();
        let resumed = f.store.load(&f.task).unwrap();
        assert_eq!(executor.approvals.load(Ordering::SeqCst), 1);
        assert_eq!(executor.visits.load(Ordering::SeqCst), 1);
        assert_eq!(resumed.action.status, AutomationActionStatus::Idle);
        assert_eq!(resumed.action.send_accepted, Some(false));
        assert!(!resumed.action.messages_fenced);
        assert!(!resumed.action.goal_turn_counted);
        assert_eq!(resumed.goal.as_ref().unwrap().turn_count, initial_turns);
        assert!(resumed.chat_mirror.message_ids.is_empty());
        assert!(resumed.action.last_error.is_none());
        let until = resumed.approval_resume_until_ms().unwrap();
        assert!(until >= 181_001);
        let restarted = AgentAutomationSupervisor::new(
            f.config.markdown_chat.automation.clone(),
            executor.clone(),
        );
        restarted
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                Arc::new(AgentAutomationStore::for_config(&f.config).unwrap()),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        for now in [6_001, 61_001, 121_001] {
            restarted.scan_once(now).await.unwrap();
            assert_eq!(
                restarted.queued_count().await,
                0,
                "approval must get the full first-tool-call grace period"
            );
            assert!(restarted.run_next(now + 1).await.unwrap().is_none());
        }
        if with_message {
            assert_eq!(f.chat.unread_snapshot().await.unwrap().messages.len(), 1);
        }
        // Fresh agent tool activity ends the wait. It is not evidence that the
        // unsubmitted pending user batch was accepted by a browser Send.
        f.chat.record_agent_call(121_002).await.unwrap();
        restarted.scan_once(121_003).await.unwrap();
        let after = f.store.load(&f.task).unwrap();
        assert!(after.approval_resume.is_none());
        assert_eq!(after.action.send_accepted, Some(false));
        assert!(!after.action.messages_fenced);
        assert_eq!(after.goal.unwrap().turn_count, initial_turns);
        assert_eq!(restarted.queued_count().await, 0);
        assert_eq!(executor.visits.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn configured_retry_timings_drive_dispatch_and_exhaustion_across_restart() {
    for mode in ["send", "access", "goal"] {
        let mut f = fixture().await;
        let timings = &mut f.config.markdown_chat.automation.retry_timings;
        timings.action_delays_seconds = vec![7, 13];
        timings.chatgpt_access_delays_seconds = vec![7, 13];
        timings.goal_recovery_delays_seconds = vec![7, 13];
        timings.jitter_percent = 0;
        let executor: Arc<dyn AutomationExecutor> = match mode {
            "send" => Arc::new(PreSendFailureExecutor::default()),
            "access" => Arc::new(AuthenticationRetryExecutor {
                calls: AtomicUsize::new(0),
                recovered: AtomicBool::new(false),
                failure: "rate_limit_preflight",
            }),
            _ => {
                let goal = f
                    .store
                    .set_goal(&f.task, "Continue after custom recovery delay", 100)
                    .unwrap();
                f.store.run_goal(&f.task, goal.revision, 101).unwrap();
                f.executor.events.lock().unwrap().extend([
                    CompanionEventKind::SendAccepted,
                    CompanionEventKind::GenerationComplete {
                        final_markdown: None,
                        canonical_url: Some("https://chatgpt.com/c/example".into()),
                    },
                ]);
                f.executor.clone()
            }
        };
        f.chat.record_agent_call(1_000).await.unwrap();
        if mode != "goal" {
            f.chat
                .append_user(
                    "custom-policy-message".into(),
                    "Retry on the configured schedule".into(),
                )
                .await
                .unwrap();
        }
        let mut now = 1_000 + AUTOMATION_OFFLINE_AFTER_MS;
        for attempt in 0..=2 {
            let store = Arc::new(AgentAutomationStore::for_config(&f.config).unwrap());
            let supervisor = AgentAutomationSupervisor::new(
                f.config.markdown_chat.automation.clone(),
                executor.clone(),
            );
            supervisor
                .register_task(AutomationTaskHandle::new(
                    f.task.clone(),
                    0,
                    store.clone(),
                    f.chat.clone(),
                ))
                .await
                .unwrap();
            if attempt > 0 {
                supervisor.scan_once(now - 1).await.unwrap();
                assert_eq!(supervisor.queued_count().await, 0, "{mode}");
            }
            supervisor.scan_once(now).await.unwrap();
            assert_eq!(
                supervisor.queued_count().await,
                1,
                "{mode}, attempt {attempt}"
            );
            let _ = supervisor.run_next(now + 1).await;
            let failed = store.load(&f.task).unwrap();
            assert_eq!(failed.automatic_retry_limit(), 2, "{mode}");
            assert_eq!(failed.automatic_retry_attempts(), attempt, "{mode}");
            if let Some(seconds) = [7, 13].get(attempt as usize) {
                let deadline = failed.automatic_retry_at_ms().expect(mode);
                assert_eq!(
                    deadline,
                    failed
                        .action
                        .updated_at_ms
                        .unwrap()
                        .max(failed.goal_wake.last_nudged_at_ms.unwrap_or(0))
                        + seconds * 1000,
                    "{mode}"
                );
                now = deadline;
            } else {
                assert_eq!(failed.automatic_retry_at_ms(), None, "{mode}");
                supervisor.scan_once(now + 100_000).await.unwrap();
                assert_eq!(supervisor.queued_count().await, 0, "{mode}");
            }
        }
    }
}

struct CooldownRetryExecutor {
    requests: Mutex<Vec<codexify::agent_automation::ExecuteActionRequest>>,
}

#[async_trait]
impl AutomationExecutor for CooldownRetryExecutor {
    async fn execute(
        &self,
        execution: AutomationExecution,
        _events: mpsc::UnboundedSender<CompanionEvent>,
        _cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        self.requests.lock().unwrap().push(execution.request);
        Err(AutomationExecutionError {
            message: "ChatGPT rejected the request with HTTP 429".into(),
            send_accepted: false,
            failure_status: Some(AutomationActionStatus::RateLimited),
        })
    }
}

#[tokio::test]
async fn manual_retry_bypasses_only_its_attempt_and_preserves_automatic_cooldowns() {
    let f = fixture().await;
    let executor = Arc::new(CooldownRetryExecutor {
        requests: Mutex::new(Vec::new()),
    });
    let supervisor =
        AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor.clone());
    supervisor
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    f.chat.record_agent_call(1000).await.unwrap();
    f.chat
        .append_user(
            "cooldown-retry".into(),
            "Keep this message until Send is accepted".into(),
        )
        .await
        .unwrap();
    let now = 1000 + AUTOMATION_OFFLINE_AFTER_MS;
    supervisor.scan_once(now).await.unwrap();
    let _ = supervisor.run_next(now + 1).await;
    assert!(!executor.requests.lock().unwrap()[0].bypass_conversation_cooldown);
    let failed = f.store.load(&f.task).unwrap();
    assert!(failed.automatic_retry_at_ms().unwrap() > now + 2);
    let retried = supervisor
        .retry_failed_action(&f.task, failed.action.id.as_deref().unwrap(), now + 2)
        .await
        .unwrap();
    assert!(retried.bypass_conversation_cooldown);
    let _ = supervisor.run_next(now + 3).await;
    assert!(executor.requests.lock().unwrap()[1].bypass_conversation_cooldown);
    let failed = f.store.load(&f.task).unwrap();
    assert!(!failed.action.bypass_conversation_cooldown);
    let deadline = failed.automatic_retry_at_ms().unwrap();
    supervisor.scan_once(deadline).await.unwrap();
    let _ = supervisor.run_next(deadline + 1).await;
    assert!(!executor.requests.lock().unwrap()[2].bypass_conversation_cooldown);
    let failed = f.store.load(&f.task).unwrap();
    f.chat.record_agent_call(deadline + 2).await.unwrap();
    let unqueued = supervisor
        .retry_failed_action(&f.task, failed.action.id.as_deref().unwrap(), deadline + 3)
        .await
        .unwrap();
    assert!(
        !unqueued.bypass_conversation_cooldown,
        "an online chat must not keep a bypass for a later automatic action"
    );
    assert_eq!(supervisor.queued_count().await, 0);
}

#[derive(Clone, Copy)]
enum CooldownOutcome {
    Wait(u64),
    RateLimit,
    Accepted,
}
struct WaitingExecutor {
    outcomes: Mutex<std::collections::VecDeque<CooldownOutcome>>,
    requests: Mutex<Vec<codexify::agent_automation::ExecuteActionRequest>>,
}
#[async_trait]
impl AutomationExecutor for WaitingExecutor {
    async fn execute(
        &self,
        execution: AutomationExecution,
        _events: mpsc::UnboundedSender<CompanionEvent>,
        _cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        let manual = execution.request.bypass_conversation_cooldown;
        self.requests
            .lock()
            .unwrap()
            .push(execution.request.clone());
        let result = if manual {
            CooldownOutcome::Accepted
        } else {
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(CooldownOutcome::Accepted)
        };
        match result {
            CooldownOutcome::Wait(until) => {
                let mut snapshot = snapshot_for(execution);
                snapshot.state = CompanionActionState::Waiting;
                snapshot.send_accepted = false;
                snapshot.canonical_url = None;
                snapshot.selected = None;
                snapshot.cooldown_until_ms = Some(until);
                Ok(snapshot)
            }
            CooldownOutcome::RateLimit => Err(AutomationExecutionError {
                message: "ChatGPT rejected the request with HTTP 429".into(),
                send_accepted: false,
                failure_status: Some(AutomationActionStatus::RateLimited),
            }),
            CooldownOutcome::Accepted => Ok(snapshot_for(execution)),
        }
    }
}
async fn waiting_fixture(
    outcomes: Vec<CooldownOutcome>,
    automatic_retries: bool,
) -> (
    Fixture,
    Arc<AgentAutomationSupervisor>,
    Arc<WaitingExecutor>,
) {
    let mut f = fixture().await;
    f.config.markdown_chat.automation.automatic_retries = automatic_retries;
    let settings = f._root.path().join("retry-settings.json");
    std::fs::write(&settings, serde_json::json!({"agentChat":{"automation":{"connectorName":"Fixture connector","automaticRetries":automatic_retries}}}).to_string()).unwrap();
    f.config.loaded_config_path = Some(settings);
    f.config
        .markdown_chat
        .automation
        .retry_timings
        .jitter_percent = 0;
    f.store = Arc::new(AgentAutomationStore::for_config(&f.config).unwrap());
    let executor = Arc::new(WaitingExecutor {
        outcomes: Mutex::new(outcomes.into()),
        requests: Mutex::new(Vec::new()),
    });
    let supervisor =
        AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor.clone());
    supervisor
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    f.chat.record_agent_call(1000).await.unwrap();
    (f, supervisor, executor)
}
#[tokio::test]
async fn initial_cooldown_waits_without_failure_or_retry_even_when_automatic_retries_are_disabled()
{
    let now = 1000 + AUTOMATION_OFFLINE_AFTER_MS;
    let until = now + 164_000;
    let (f, supervisor, executor) = waiting_fixture(
        vec![CooldownOutcome::Wait(until), CooldownOutcome::Accepted],
        false,
    )
    .await;
    let goal = f.store.set_goal(&f.task, "Already done", 100).unwrap();
    f.store.run_goal(&f.task, goal.revision, 101).unwrap();
    f.store
        .finish_goal(
            &f.task,
            goal.revision,
            GoalFinish::Completed { summary: None },
            102,
        )
        .unwrap();
    f.chat
        .append_user("initial-wait".into(), "Do this next".into())
        .await
        .unwrap();
    supervisor.scan_once(now).await.unwrap();
    supervisor.run_next(now + 1).await.unwrap();
    let waiting = f.store.load(&f.task).unwrap();
    assert_eq!(waiting.action.status, AutomationActionStatus::Queued);
    assert_eq!(waiting.browser_cooldown_until_ms(), Some(until));
    assert!(waiting.action.last_error.is_none());
    assert!(waiting.automatic_retry_at_ms().is_none());
    assert!(!waiting.action_needs_attention());
    assert!(!waiting.is_automatically_retrying());
    assert_eq!(waiting.automatic_retry_attempts(), 0);
    assert_eq!(waiting.retry_mode(), Some("retry"));
    assert_eq!(waiting.goal.as_ref().unwrap().status, GoalStatus::Completed);
    for time in [now + 2, until - 1] {
        supervisor.scan_once(time).await.unwrap();
        assert!(supervisor.run_next(time).await.unwrap().is_none());
    }
    assert_eq!(executor.requests.lock().unwrap().len(), 1);
    supervisor.scan_once(until).await.unwrap();
    supervisor.run_next(until).await.unwrap();
    let requests = executor.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_ne!(requests[0].action_id, requests[1].action_id);
    assert_ne!(requests[0].idempotency_key, requests[1].idempotency_key);
    let done = f.store.load(&f.task).unwrap();
    assert_eq!(done.action.status, AutomationActionStatus::Idle);
    assert!(done.action.last_error.is_none());
    assert_eq!(done.automatic_retry_attempts(), 0);
    assert_eq!(done.goal.unwrap().status, GoalStatus::Completed);
}
#[tokio::test]
async fn repeated_cooldowns_preserve_real_failure_budget_and_resume_at_availability_without_extra_backoff()
 {
    let now = 1000 + AUTOMATION_OFFLINE_AFTER_MS;
    let first = now + 900_000;
    let second = first + 300_000;
    let (f, supervisor, executor) = waiting_fixture(
        vec![
            CooldownOutcome::RateLimit,
            CooldownOutcome::Wait(first),
            CooldownOutcome::Wait(second),
            CooldownOutcome::RateLimit,
        ],
        true,
    )
    .await;
    f.chat
        .append_user("retry-wait".into(), "Preserve this message".into())
        .await
        .unwrap();
    supervisor.scan_once(now).await.unwrap();
    let _ = supervisor.run_next(now).await;
    let failed = f.store.load(&f.task).unwrap();
    let ready = failed.automatic_retry_at_ms().unwrap();
    assert_eq!(ready, failed.action.updated_at_ms.unwrap() + 180_000);
    supervisor.scan_once(ready).await.unwrap();
    assert_eq!(
        f.store
            .load(&f.task)
            .unwrap()
            .action
            .chatgpt_access_retry_attempts,
        1
    );
    supervisor.run_next(ready).await.unwrap();
    for (until, next) in [(first, second), (second, 0)] {
        let waiting = f.store.load(&f.task).unwrap();
        assert_eq!(waiting.action.chatgpt_access_retry_attempts, 0);
        assert_eq!(waiting.browser_cooldown_until_ms(), Some(until));
        assert!(waiting.automatic_retry_at_ms().is_none());
        assert!(waiting.action.last_error.is_none());
        assert_eq!(
            waiting.suspended_retry.as_ref().unwrap().action,
            failed.action
        );
        assert!(supervisor.run_next(until - 1).await.unwrap().is_none());
        supervisor.scan_once(until).await.unwrap();
        let result = supervisor.run_next(until).await;
        if next != 0 {
            result.unwrap();
        } else {
            assert!(result.is_err());
        }
    }
    assert_eq!(executor.requests.lock().unwrap().len(), 4);
    let failed_again = f.store.load(&f.task).unwrap();
    assert_eq!(failed_again.action.chatgpt_access_retry_attempts, 1);
    assert_eq!(
        failed_again.automatic_retry_at_ms(),
        Some(failed_again.action.updated_at_ms.unwrap() + 300_000)
    );
}
#[tokio::test]
async fn goal_recovery_cooldown_does_not_consume_a_goal_retry_or_count_an_extra_turn() {
    let now = 1000 + AUTOMATION_OFFLINE_AFTER_MS;
    let until = now + 300_000;
    let (f, supervisor, _) = waiting_fixture(
        vec![
            CooldownOutcome::Accepted,
            CooldownOutcome::Wait(until),
            CooldownOutcome::Accepted,
        ],
        true,
    )
    .await;
    let goal = f.store.set_goal(&f.task, "Keep working", 100).unwrap();
    f.store.run_goal(&f.task, goal.revision, 101).unwrap();
    supervisor.scan_once(now).await.unwrap();
    supervisor.run_next(now).await.unwrap();
    let failed = f.store.load(&f.task).unwrap();
    let turns = failed.goal.as_ref().unwrap().turn_count;
    let retry = failed.automatic_retry_at_ms().unwrap();
    supervisor.scan_once(retry).await.unwrap();
    supervisor.run_next(retry).await.unwrap();
    let waiting = f.store.load(&f.task).unwrap();
    assert_eq!(waiting.goal_wake.recovery_attempts, 0);
    assert_eq!(waiting.goal.as_ref().unwrap().turn_count, turns);
    assert!(waiting.automatic_retry_at_ms().is_none());
    supervisor.scan_once(until).await.unwrap();
    supervisor.run_next(until).await.unwrap();
    let after = f.store.load(&f.task).unwrap();
    assert_eq!(after.goal_wake.recovery_attempts, 1);
    assert_eq!(after.goal.as_ref().unwrap().turn_count, turns + 1);
    assert_eq!(
        after.automatic_retry_at_ms(),
        Some(
            after
                .action
                .updated_at_ms
                .unwrap()
                .max(after.goal_wake.last_nudged_at_ms.unwrap_or(0))
                + 120_000
        )
    );
}
#[tokio::test]
async fn queued_cooldown_can_be_overridden_manually_without_waiting() {
    let now = 1000 + AUTOMATION_OFFLINE_AFTER_MS;
    let until = now + 180_000;
    let (f, supervisor, executor) = waiting_fixture(vec![CooldownOutcome::Wait(until)], true).await;
    f.chat
        .append_user("manual-wait".into(), "Try on my request".into())
        .await
        .unwrap();
    supervisor.scan_once(now).await.unwrap();
    supervisor.run_next(now).await.unwrap();
    let waiting = f.store.load(&f.task).unwrap();
    let retried = supervisor
        .retry_failed_action(&f.task, waiting.action.id.as_deref().unwrap(), now + 1)
        .await
        .unwrap();
    assert!(retried.bypass_conversation_cooldown);
    assert!(retried.cooldown_until_ms.is_none());
    supervisor.run_next(now + 2).await.unwrap();
    let requests = executor.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].bypass_conversation_cooldown);
}
#[tokio::test]
async fn cooldown_survives_restart_and_does_not_block_other_ready_chats() {
    let now = 1000 + AUTOMATION_OFFLINE_AFTER_MS;
    let until = now + 180_000;
    let (f, supervisor, executor) = waiting_fixture(
        vec![CooldownOutcome::Wait(until), CooldownOutcome::Accepted],
        true,
    )
    .await;
    f.chat
        .append_user("restart-wait".into(), "Wait for availability".into())
        .await
        .unwrap();
    supervisor.scan_once(now).await.unwrap();
    supervisor.run_next(now).await.unwrap();
    let (other, chat) = add_task(&f, "cooldown-other", "https://chatgpt.com/c/other").await;
    chat.record_agent_call(1000).await.unwrap();
    chat.append_user("other-wait".into(), "Other ready work".into())
        .await
        .unwrap();
    supervisor
        .register_task(AutomationTaskHandle::new(other, 0, f.store.clone(), chat))
        .await
        .unwrap();
    supervisor.scan_once(now + 1).await.unwrap();
    supervisor.run_next(now + 1).await.unwrap();
    assert_eq!(executor.requests.lock().unwrap().len(), 2);
    assert_eq!(supervisor.queued_count().await, 1);
    let restarted =
        AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor.clone());
    restarted
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    restarted.scan_once(until - 1).await.unwrap();
    assert_eq!(restarted.queued_count().await, 0);
    assert!(f.store.load(&f.task).unwrap().action.last_error.is_none());
    restarted.scan_once(until).await.unwrap();
    restarted.run_next(until).await.unwrap();
    assert_eq!(executor.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn persisted_queued_chat_can_run_now_before_its_cooldown_without_a_goal() {
    let now = 1000 + AUTOMATION_OFFLINE_AFTER_MS;
    let (f, supervisor, executor) =
        waiting_fixture(vec![CooldownOutcome::Wait(now + 180_000)], true).await;
    f.chat
        .append_user(
            "manual-after-restart".into(),
            "Keep the original request".into(),
        )
        .await
        .unwrap();
    supervisor.scan_once(now).await.unwrap();
    supervisor.run_next(now).await.unwrap();
    let before = f.store.load(&f.task).unwrap();
    assert!(before.goal.is_none());
    let restarted =
        AgentAutomationSupervisor::new(f.config.markdown_chat.automation.clone(), executor.clone());
    restarted
        .register_task(AutomationTaskHandle::new(
            f.task.clone(),
            0,
            f.store.clone(),
            f.chat.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(restarted.queued_count().await, 0);
    let queued = restarted
        .run_queued_action_now(&f.task, before.action.id.as_deref().unwrap(), now + 1)
        .await
        .unwrap();
    assert_eq!(queued.status, AutomationActionStatus::Queued);
    assert!(queued.bypass_conversation_cooldown);
    restarted.run_next(now + 1).await.unwrap().unwrap();
    let requests = executor.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].bypass_conversation_cooldown);
    assert!(f.store.load(&f.task).unwrap().goal.is_none());
}

#[tokio::test]
async fn pending_cooldown_honors_cancel_pause_and_global_retry_controls() {
    for control in ["cancel", "pause", "disable"] {
        let now = 1000 + AUTOMATION_OFFLINE_AFTER_MS;
        let until = now + 900_000;
        let (f, supervisor, executor) = waiting_fixture(
            vec![CooldownOutcome::RateLimit, CooldownOutcome::Wait(until)],
            true,
        )
        .await;
        f.chat
            .append_user(
                "controlled-wait".into(),
                "Preserve this unread message".into(),
            )
            .await
            .unwrap();
        supervisor.scan_once(now).await.unwrap();
        assert!(supervisor.run_next(now).await.is_err());
        let failed = f.store.load(&f.task).unwrap();
        let ready = failed.automatic_retry_at_ms().unwrap();
        supervisor.scan_once(ready).await.unwrap();
        supervisor.run_next(ready).await.unwrap();
        let waiting = f.store.load(&f.task).unwrap();
        let id = waiting.action.id.as_deref().unwrap();
        let supervisor = AgentAutomationSupervisor::new(
            f.config.markdown_chat.automation.clone(),
            executor.clone(),
        );
        supervisor
            .register_task(AutomationTaskHandle::new(
                f.task.clone(),
                0,
                f.store.clone(),
                f.chat.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(
            supervisor.queued_count().await,
            0,
            "controls also work while only the persisted wait exists after restart"
        );
        match control {
            "cancel" => {
                supervisor
                    .cancel_action(&f.task, id, ready + 1)
                    .await
                    .unwrap();
            }
            "pause" => {
                supervisor.pause_action_retries(&f.task, id).await.unwrap();
            }
            _ => {
                std::fs::write(f.config.loaded_config_path.as_ref().unwrap(), serde_json::json!({"agentChat":{"automation":{"connectorName":"Fixture connector","automaticRetries":false}}}).to_string()).unwrap();
            }
        }
        supervisor.scan_once(until).await.unwrap();
        assert!(supervisor.run_next(until).await.unwrap().is_none());
        let after = f.store.load(&f.task).unwrap();
        assert!(after.browser_cooldown_until_ms().is_none());
        assert!(after.automatic_retry_at_ms().is_none());
        assert_eq!(after.action.chatgpt_access_retry_attempts, 0);
        assert_eq!(after.action.last_error.is_some(), control != "cancel");
        assert_eq!(executor.requests.lock().unwrap().len(), 2);
        assert_eq!(f.chat.unread_snapshot().await.unwrap().messages.len(), 1);
    }
}

#[tokio::test]
async fn native_delivery_during_cooldown_clears_pending_work_without_another_browser_request() {
    let now = 1000 + AUTOMATION_OFFLINE_AFTER_MS;
    let until = now + 180_000;
    let (f, supervisor, executor) = waiting_fixture(vec![CooldownOutcome::Wait(until)], true).await;
    let goal = f
        .store
        .set_goal(&f.task, "Keep the current goal", 100)
        .unwrap();
    f.store.run_goal(&f.task, goal.revision, 101).unwrap();
    f.chat
        .append_user(
            "delivered-wait".into(),
            "Delivered through the connector".into(),
        )
        .await
        .unwrap();
    supervisor.scan_once(now).await.unwrap();
    supervisor.run_next(now).await.unwrap();
    assert!(
        f.store
            .load(&f.task)
            .unwrap()
            .browser_cooldown_until_ms()
            .is_some()
    );
    f.chat.read(true).await.unwrap();
    f.chat.record_agent_call(now + 1).await.unwrap();
    supervisor.scan_once(now + 1).await.unwrap();
    let delivered = f.store.load(&f.task).unwrap();
    assert!(delivered.browser_cooldown_until_ms().is_none());
    assert_eq!(delivered.action.status, AutomationActionStatus::Idle);
    assert_eq!(delivered.goal.unwrap().turn_count, 1);
    assert_eq!(supervisor.queued_count().await, 0);
    assert!(supervisor.run_next(until).await.unwrap().is_none());
    assert_eq!(executor.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn first_real_failure_after_initial_cooldown_starts_at_the_first_retry_delay() {
    let now = 1000 + AUTOMATION_OFFLINE_AFTER_MS;
    let until = now + 70_000;
    let (f, supervisor, _) = waiting_fixture(
        vec![CooldownOutcome::Wait(until), CooldownOutcome::RateLimit],
        true,
    )
    .await;
    f.chat
        .append_user("initial-budget".into(), "First actual attempt".into())
        .await
        .unwrap();
    supervisor.scan_once(now).await.unwrap();
    supervisor.run_next(now).await.unwrap();
    assert!(
        f.store
            .load(&f.task)
            .unwrap()
            .automatic_retry_at_ms()
            .is_none()
    );
    supervisor.scan_once(until).await.unwrap();
    assert!(supervisor.run_next(until).await.is_err());
    let failed = f.store.load(&f.task).unwrap();
    assert_eq!(failed.action.chatgpt_access_retry_attempts, 0);
    assert_eq!(
        failed.automatic_retry_at_ms(),
        Some(failed.action.updated_at_ms.unwrap() + 180_000)
    );
}

#[tokio::test]
async fn configured_offline_timeout_prevents_premature_chat_and_goal_followups() {
    for goal in [false, true] {
        let f = fixture_with_offline_timeout(64, 10).await;
        f.chat.record_agent_call(1_000).await.unwrap();
        if goal {
            let configured = f
                .store
                .set_goal(
                    &f.task,
                    "Continue only after the configured silence interval",
                    100,
                )
                .unwrap();
            f.store.run_goal(&f.task, configured.revision, 101).unwrap();
        } else {
            f.chat
                .append_user(
                    "slow-agent-message".into(),
                    "Continue when the agent is offline".into(),
                )
                .await
                .unwrap();
        }
        for elapsed in [240_000, 300_000, 599_999] {
            f.supervisor.scan_once(1_000 + elapsed).await.unwrap();
            assert_eq!(
                f.supervisor.queued_count().await,
                0,
                "goal={goal}, elapsed={elapsed}"
            );
            assert_eq!(f.executor.calls.load(Ordering::SeqCst), 0);
        }
        // New activity resets the full configured interval, rather than merely
        // extending the old fixed five-minute deadline.
        f.chat.record_agent_call(590_000).await.unwrap();
        f.supervisor.scan_once(1_000 + 600_000).await.unwrap();
        assert_eq!(f.supervisor.queued_count().await, 0);
        f.supervisor.scan_once(590_000 + 600_000 - 1).await.unwrap();
        assert_eq!(f.supervisor.queued_count().await, 0);
        f.supervisor.scan_once(590_000 + 600_000).await.unwrap();
        assert_eq!(f.supervisor.queued_count().await, 1, "goal={goal}");
        assert_eq!(f.executor.calls.load(Ordering::SeqCst), 0);
    }
}
