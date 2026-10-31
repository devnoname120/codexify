use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Notify, RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use super::companion::{
    MAX_PROMPT_BYTES, MAX_UNREAD_MESSAGES, rendered_action_prompt_bytes,
    same_chatgpt_conversation_url,
};
use super::store::{MAX_CHAT_MIRROR_IDS, MAX_PRIVATE_TEXT_BYTES, QueueFailure};
use super::{
    AcceptedChatBatch, ActionPrompt, AgentAutomationStore, AutomationActionKind,
    AutomationActionStatus, BrowserCompanionClient, CompanionActionSnapshot, CompanionActionState,
    CompanionEvent, CompanionEventKind, CompanionSessionStatus, CompanionUnreadMessage,
    ExecuteActionRequest, GoalStatus,
};
use crate::markdown_chat::{AgentAutomationConfig, ChatFile};
use crate::project_bindings::ConversationIdentity;

#[derive(Clone)]
pub struct AutomationExecution {
    pub task_key: String,
    pub owner_generation: u64,
    pub account_fingerprint: String,
    pub catalog_revision: String,
    pub activity_epoch: String,
    pub activity_sequence: u64,
    pub tool_call_count_at_start: u64,
    pub unread_message_ids: Vec<String>,
    pub transcript_revision: String,
    pub pairing_commit: Option<PairingCommit>,
    pub request: ExecuteActionRequest,
}

#[derive(Clone)]
pub struct PairingCommit {
    task: ConversationIdentity,
    store: Arc<AgentAutomationStore>,
    generation: u64,
}

impl PairingCommit {
    #[doc(hidden)]
    pub fn new(
        task: ConversationIdentity,
        store: Arc<AgentAutomationStore>,
        generation: u64,
    ) -> Self {
        Self {
            task,
            store,
            generation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationExecutionError {
    pub message: String,
    pub send_accepted: bool,
    pub failure_status: Option<AutomationActionStatus>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationLiveState {
    pub action_id: Option<String>,
    pub phase: AutomationActionStatus,
    pub status: Option<String>,
    pub commentary: Option<String>,
    pub answer: Option<String>,
    pub updated_at_ms: u64,
}

#[async_trait]
pub trait AutomationExecutor: Send + Sync {
    fn supports_pairing_reservation(&self) -> bool {
        false
    }

    async fn release_pairing_window(&self, _action_id: &str) -> Result<(), String> {
        Ok(())
    }
    async fn approve_pending_conversation(
        &self,
        _url: &str,
        _account: &str,
        _connector_name: &str,
    ) -> Result<bool, String> {
        Ok(false)
    }
    async fn turn_handoff_complete(
        &self,
        _url: &str,
        _account: &str,
        _handoff_id: &str,
    ) -> Result<bool, String> {
        Ok(false)
    }
    async fn prepare_new_chat(
        &self,
        _project_url: Option<&str>,
        _cancellation: CancellationToken,
    ) -> Result<super::CapabilityCatalog, String> {
        Err("New chat preparation is unavailable".into())
    }
    async fn timed_out_action(
        &self,
        _execution: &AutomationExecution,
        message: &str,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        Err(execution_error(message.into(), true))
    }
    async fn retained_action(
        &self,
        _action_id: &str,
        _account_fingerprint: &str,
    ) -> Result<Option<CompanionActionSnapshot>, String> {
        Ok(None)
    }

    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError>;
}

#[derive(Clone)]
pub struct BrowserAutomationExecutor {
    config: AgentAutomationConfig,
}

impl BrowserAutomationExecutor {
    pub fn new(config: AgentAutomationConfig) -> Self {
        Self { config }
    }

    async fn ready_client(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<BrowserCompanionClient, String> {
        super::BrowserCompanionControl::new(&self.config)?
            .ensure_running(cancellation)
            .await
    }
}

#[async_trait]
impl AutomationExecutor for BrowserAutomationExecutor {
    fn supports_pairing_reservation(&self) -> bool {
        true
    }

    async fn release_pairing_window(&self, action_id: &str) -> Result<(), String> {
        let client = BrowserCompanionClient::from_config(&self.config)?;
        tokio::time::timeout(
            Duration::from_secs(5),
            client.release_pairing_window(action_id),
        )
        .await
        .map_err(|_| "Releasing the browser window timed out".to_string())?
    }
    async fn approve_pending_conversation(
        &self,
        url: &str,
        account: &str,
        connector_name: &str,
    ) -> Result<bool, String> {
        if !self.config.auto_approve_tool_calls {
            return Ok(false);
        }
        let lifecycle = super::BrowserCompanionControl::new(&self.config)?.public_status();
        if lifecycle["manuallyStopped"] == true || lifecycle["waitingForServerRestart"] == true {
            return Ok(false);
        }
        // Passive checks reuse an existing companion; they never start or reveal it.
        let client = BrowserCompanionClient::from_config(&self.config)?;
        let cancellation = CancellationToken::new();
        let result = tokio::time::timeout(
            Duration::from_secs(6),
            client.approve_pending_conversation(url, account, connector_name, &cancellation),
        )
        .await;
        cancellation.cancel();
        result.map_err(|_| "ChatGPT approval check timed out".to_string())?
    }
    async fn turn_handoff_complete(
        &self,
        url: &str,
        account: &str,
        handoff_id: &str,
    ) -> Result<bool, String> {
        let cancellation = CancellationToken::new();
        let client = self.ready_client(&cancellation).await?;
        client
            .turn_handoff_complete(url, account, handoff_id, &cancellation)
            .await
    }
    async fn prepare_new_chat(
        &self,
        project_url: Option<&str>,
        cancellation: CancellationToken,
    ) -> Result<super::CapabilityCatalog, String> {
        let client = self.ready_client(&cancellation).await?;
        client
            .new_chat_capabilities(project_url, &cancellation)
            .await
    }
    async fn timed_out_action(
        &self,
        execution: &AutomationExecution,
        message: &str,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        let uncertain = |error: String| execution_error(format!("{message}; {error}"), true);
        let client = BrowserCompanionClient::from_config(&self.config).map_err(uncertain)?;
        let cancellation = CancellationToken::new();
        let health = client.health(&cancellation).await.map_err(uncertain)?;
        if !health.authenticated
            || health.account_fingerprint.as_deref() != Some(execution.account_fingerprint.as_str())
        {
            return Err(uncertain(
                "Browser action status could not be verified for the same account".into(),
            ));
        }
        let snapshot = client
            .action(&execution.request.action_id, &cancellation)
            .await
            .map_err(uncertain)?;
        if action_is_terminal(snapshot.state) {
            validate_terminal_snapshot(execution, &snapshot)?;
            return Ok(snapshot);
        }
        if !snapshot.send_accepted {
            // Cancel an unaccepted action so it cannot submit later after the queue moves on.
            client
                .cancel(
                    &execution.request.action_id,
                    &format!("timeout-{}", execution.request.action_id),
                    &cancellation,
                )
                .await
                .map_err(uncertain)?;
            let cancelled = client
                .action(&execution.request.action_id, &cancellation)
                .await
                .map_err(uncertain)?;
            if !cancelled.send_accepted && cancelled.state == CompanionActionState::Failed {
                return Err(execution_error(message.into(), false));
            }
        }
        Err(execution_error(message.into(), true))
    }
    async fn retained_action(
        &self,
        action_id: &str,
        account_fingerprint: &str,
    ) -> Result<Option<CompanionActionSnapshot>, String> {
        let client = BrowserCompanionClient::from_config(&self.config)?;
        let cancellation = CancellationToken::new();
        let health = client.health(&cancellation).await?;
        if !health.authenticated
            || health.account_fingerprint.as_deref() != Some(account_fingerprint)
        {
            return Err("Browser action recovery could not verify the signed-in account".into());
        }
        if health.status != CompanionSessionStatus::Idle || health.active_action_id.is_some() {
            return Ok(None);
        }
        client.action(action_id, &cancellation).await.map(Some)
    }

    async fn execute(
        &self,
        execution: AutomationExecution,
        events: mpsc::UnboundedSender<CompanionEvent>,
        cancellation: CancellationToken,
    ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
        let client = self
            .ready_client(&cancellation)
            .await
            .map_err(|message| execution_error(message, false))?;
        let health = client
            .health(&cancellation)
            .await
            .map_err(|message| execution_error(message, false))?;
        if health.status == CompanionSessionStatus::Unavailable {
            return Err(execution_error(
                "Browser companion is unavailable; its Chrome window is not ready".into(),
                false,
            ));
        }
        // An owned companion action may reload a stale sign-in page, then must
        // verify authentication and the paired account again before any Send.
        let recover_authentication = !health.authenticated
            && health.status == CompanionSessionStatus::AuthenticationRequired
            && !matches!(execution.request.prompt, ActionPrompt::CreateChat { .. });
        if !health.authenticated && !recover_authentication {
            return Err(AutomationExecutionError {
                message: "Browser companion authentication is required".into(),
                send_accepted: false,
                failure_status: Some(AutomationActionStatus::AuthenticationRequired),
            });
        }
        if !recover_authentication
            && health.account_fingerprint.as_deref() != Some(execution.account_fingerprint.as_str())
        {
            return Err(AutomationExecutionError {
                message: "Browser companion account changed; refresh capabilities and verify the pairing again".into(),
                send_accepted: false,
                failure_status: Some(AutomationActionStatus::AuthenticationRequired),
            });
        }
        let pairing_continuation = execution
            .request
            .pairing_continuation_of
            .as_deref()
            .is_some_and(|parent| health.active_action_id.as_deref() == Some(parent));
        if health.active_action_id.is_some() && !pairing_continuation {
            return Err(execution_error(
                "Browser companion is busy with another action".into(),
                false,
            ));
        }
        // Only the companion can prove and replace an unchanged verification draft.
        // Let it perform that check for pairing; ordinary work still requires an idle window.
        let replacing_pairing = matches!(
            execution.request.prompt,
            ActionPrompt::PairVerify { .. } | ActionPrompt::CreateChat { .. }
        );
        if health.status != CompanionSessionStatus::Idle
            && !pairing_continuation
            && !recover_authentication
            && !matches!(execution.request.prompt, ActionPrompt::CreateChat { .. })
            && !(replacing_pairing && health.status == CompanionSessionStatus::Busy)
        {
            return Err(execution_error(
                "The shared ChatGPT window is not ready; check for an unsent draft or an ongoing response".into(),
                false,
            ));
        }
        // Selection is verified by the companion after navigating to the target.
        // A full catalog refresh here would operate on the previous conversation.

        if let Some(commit) = &execution.pairing_commit
            && !matches!(execution.request.prompt, ActionPrompt::CreateChat { .. })
        {
            commit
                .store
                .record_pairing_canonical_url(
                    &commit.task,
                    commit.generation,
                    &execution.request.expected_url,
                    crate::markdown_chat::now_ms(),
                )
                .map_err(|message| execution_error(message, false))?;
        }

        let initial = client
            .execute_with_acceptance(&execution.request, &cancellation)
            .await
            .map_err(|error| {
                if recover_authentication && !error.send_accepted {
                    // Older compatible companions can reject recovery before
                    // recording an action. Retain the known sign-in failure.
                    AutomationExecutionError {
                        message: "Browser companion authentication is required".into(),
                        send_accepted: false,
                        failure_status: Some(AutomationActionStatus::AuthenticationRequired),
                    }
                } else {
                    execution_error(error.message, error.send_accepted)
                }
            })?;
        if action_is_terminal(initial.state) {
            if initial.last_event_sequence > 0 {
                let mut last_event_sequence = 0;
                let first = client
                    .stream_action_events(&execution.request.action_id, 0, &cancellation, |event| {
                        let sequence = event.sequence;
                        events.send(event).map_err(|_| {
                            "Browser automation event receiver is unavailable".to_string()
                        })?;
                        last_event_sequence = sequence;
                        Ok(())
                    })
                    .await;
                if let Err(message) = first {
                    client
                        .stream_action_events(
                            &execution.request.action_id,
                            last_event_sequence,
                            &cancellation,
                            |event| {
                                let sequence = event.sequence;
                                events.send(event).map_err(|_| {
                                    "Browser automation event receiver is unavailable".to_string()
                                })?;
                                last_event_sequence = sequence;
                                Ok(())
                            },
                        )
                        .await
                        .map_err(|retry| {
                            execution_error(
                                format!(
                                    "{message}; retained browser event reconciliation failed: {retry}"
                                ),
                                initial.send_accepted,
                            )
                        })?;
                }
            }
            validate_terminal_snapshot(&execution, &initial)?;
            return Ok(initial);
        }

        let mut last_event_sequence = 0;
        let mut previous_stream_error: Option<String> = None;
        loop {
            let stream = client
                .stream_action_events(
                    &execution.request.action_id,
                    last_event_sequence,
                    &cancellation,
                    |event| {
                        last_event_sequence = event.sequence;
                        events.send(event).map_err(|_| {
                            "Browser automation event receiver is unavailable".to_string()
                        })
                    },
                )
                .await;
            let Err(stream_error) = stream else {
                break;
            };
            let message = previous_stream_error.as_ref().map_or_else(
                || stream_error.clone(),
                |previous| {
                    format!("{previous}; retained browser event reconnect failed: {stream_error}")
                },
            );
            let reconciled = client
                .action(&execution.request.action_id, &cancellation)
                .await;
            if let Ok(ref snapshot) = reconciled
                && action_is_terminal(snapshot.state)
            {
                if last_event_sequence < snapshot.last_event_sequence {
                    client
                        .stream_action_events(
                            &execution.request.action_id,
                            last_event_sequence,
                            &cancellation,
                            |event| {
                                last_event_sequence = event.sequence;
                                events.send(event).map_err(|_| {
                                    "Browser automation event receiver is unavailable".to_string()
                                })
                            },
                        )
                        .await
                        .map_err(|retry| {
                            execution_error(
                                format!(
                                    "{message}; retained browser event reconciliation failed: {retry}"
                                ),
                                snapshot.send_accepted,
                            )
                        })?;
                }
                validate_terminal_snapshot(&execution, snapshot)?;
                return Ok(snapshot.clone());
            }
            if previous_stream_error.is_none() {
                previous_stream_error = Some(stream_error);
                continue;
            }
            return Err(execution_error(message, true));
        }
        // The companion accepted this action. A restart before the final receipt
        // cannot prove that Send was never attempted.
        let terminal = client
            .action(&execution.request.action_id, &cancellation)
            .await
            .map_err(|message| execution_error(message, true))?;
        if !action_is_terminal(terminal.state) {
            return Err(execution_error(
                "Browser companion action did not reach a terminal state".into(),
                terminal.send_accepted || initial.send_accepted,
            ));
        }
        validate_terminal_snapshot(&execution, &terminal)?;
        Ok(terminal)
    }
}

#[derive(Clone)]
pub struct AutomationTaskHandle {
    task: ConversationIdentity,
    owner_generation: u64,
    store: Arc<AgentAutomationStore>,
    chat: Arc<ChatFile>,
}

impl AutomationTaskHandle {
    pub fn new(
        task: ConversationIdentity,
        owner_generation: u64,
        store: Arc<AgentAutomationStore>,
        chat: Arc<ChatFile>,
    ) -> Self {
        Self {
            task,
            owner_generation,
            store,
            chat,
        }
    }
}

pub struct AgentAutomationSupervisor {
    config: AgentAutomationConfig,
    executor: Arc<dyn AutomationExecutor>,
    tasks: RwLock<HashMap<String, AutomationTaskHandle>>,
    queue: Mutex<VecDeque<AutomationExecution>>,
    scan_lock: Mutex<()>,
    run_lock: Mutex<()>,
    live: RwLock<HashMap<String, AutomationLiveState>>,
    wake: Arc<Notify>,
    action_timeouts: ActionTimeouts,
    approval_next_scan_ms: AtomicU64,
    next_retry_scan_ms: AtomicU64,
    rollover: OnceLock<ConversationRolloverContext>,
    pairing_wait_timeout: Duration,
}

struct PairingWindow {
    task: AutomationTaskHandle,
    action_id: String,
    generation: u64,
    owner_generation: u64,
}

struct ConversationRolloverContext {
    config: Arc<crate::types::AppConfig>,
    bindings: Arc<crate::project_bindings::ProjectBindingStore>,
    continuations: Arc<crate::conversation_continuations::ConversationContinuationStore>,
    tools: Arc<Vec<Box<dyn crate::tool::Tool>>>,
    tickets: Arc<crate::agent_tickets::AgentTicketStore>,
}

struct ActionTimeouts {
    start: Duration,
    progress: Duration,
    total: Duration,
}

pub struct AutomationSupervisorWorker {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for AutomationSupervisorWorker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl AgentAutomationSupervisor {
    #[cfg(test)]
    pub(crate) fn new_with_pairing_wait_timeout(
        config: AgentAutomationConfig,
        executor: Arc<dyn AutomationExecutor>,
        timeout: Duration,
    ) -> Arc<Self> {
        let mut supervisor = Self::new(config, executor);
        Arc::get_mut(&mut supervisor)
            .expect("new supervisor is unshared")
            .pairing_wait_timeout = timeout;
        supervisor
    }

    pub fn new(config: AgentAutomationConfig, executor: Arc<dyn AutomationExecutor>) -> Arc<Self> {
        Self::new_with_action_timeouts(
            config,
            executor,
            Duration::from_secs(120),
            Duration::from_secs(180),
            Duration::from_secs(32 * 60),
        )
    }

    #[doc(hidden)]
    pub fn new_with_action_timeouts(
        config: AgentAutomationConfig,
        executor: Arc<dyn AutomationExecutor>,
        start: Duration,
        progress: Duration,
        total: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            executor,
            tasks: RwLock::new(HashMap::new()),
            queue: Mutex::new(VecDeque::new()),
            scan_lock: Mutex::new(()),
            run_lock: Mutex::new(()),
            live: RwLock::new(HashMap::new()),
            wake: Arc::new(Notify::new()),
            approval_next_scan_ms: AtomicU64::new(0),
            next_retry_scan_ms: AtomicU64::new(0),
            rollover: OnceLock::new(),
            pairing_wait_timeout: Duration::from_secs(90),
            action_timeouts: ActionTimeouts {
                start,
                progress,
                total,
            },
        })
    }

    pub(crate) fn enable_conversation_rollover(
        &self,
        config: Arc<crate::types::AppConfig>,
        bindings: Arc<crate::project_bindings::ProjectBindingStore>,
        continuations: Arc<crate::conversation_continuations::ConversationContinuationStore>,
        tools: Arc<Vec<Box<dyn crate::tool::Tool>>>,
        tickets: Arc<crate::agent_tickets::AgentTicketStore>,
    ) -> Result<(), String> {
        self.rollover
            .set(ConversationRolloverContext {
                config,
                bindings,
                continuations,
                tools,
                tickets,
            })
            .map_err(|_| "Conversation rollover was already configured".to_owned())
    }

    async fn queue_conversation_rollover(
        &self,
        task: &AutomationTaskHandle,
        expected_action_id: &str,
        now_ms: u64,
    ) -> Result<bool, String> {
        let state = task.store.load(&task.task)?;
        if state.action.id.as_deref() != Some(expected_action_id)
            || state.action.status != AutomationActionStatus::ConversationLimit
            || state.action.kind == Some(AutomationActionKind::PairVerify)
        {
            return Ok(false);
        }
        self.queue_replacement_conversation(task, &state, false, now_ms)
            .await
    }

    pub(crate) fn current_schema_version(&self) -> Option<String> {
        self.rollover.get().map(|context| {
            crate::server::automation_schema_version(&context.tools, &context.config)
        })
    }

    fn schema_refresh_ready(&self) -> bool {
        self.rollover.get().is_none_or(|context| {
            crate::global_settings::automation_schema_refresh_ready(&context.config)
                && (context.config.loaded_config_path.is_none()
                    || crate::global_settings::read_schema_intent(&context.config).is_some_and(
                        |intent| {
                            intent["observedConnectorSchemaVersion"].as_str()
                                == self.current_schema_version().as_deref()
                        },
                    ))
        })
    }

    /// Called before publishing new owner input, while it can still be hidden
    /// from the old owner's in-flight chat_await/chat_write calls.
    pub(crate) async fn prepare_schema_rollover(
        &self,
        task: &ConversationIdentity,
    ) -> Result<bool, String> {
        let Some(version) = self.current_schema_version() else {
            return Ok(false);
        };
        let Some(handle) = self.tasks.read().await.get(task.stable_key()).cloned() else {
            return Ok(false);
        };
        let state = handle.store.load(&handle.task)?;
        if state.pairing.verified_owner_generation != Some(handle.owner_generation) {
            return Ok(false);
        }
        if state.pairing.connector_schema_version.as_deref() == Some(&version) {
            if state.pairing.schema_rollover_pending {
                handle
                    .store
                    .clear_schema_rollover_hold(&handle.task, &state.pairing)?;
            }
            return Ok(false);
        }
        let held = handle
            .store
            .hold_messages_for_schema_rollover(&handle.task, &state.pairing)?;
        self.notify();
        Ok(held)
    }

    async fn queue_replacement_conversation(
        &self,
        task: &AutomationTaskHandle,
        state: &super::AutomationTaskState,
        schema_changed: bool,
        now_ms: u64,
    ) -> Result<bool, String> {
        let Some(context) = self.rollover.get() else {
            return Ok(false);
        };
        if state.pairing.verified_owner_generation != Some(task.owner_generation)
            || task.chat.offline_activity().await?.agent_stop_requested
            || self.queue.lock().await.len() >= self.config.max_queued_actions
            || !self.schema_refresh_ready()
        {
            return Ok(false);
        }
        let root = context
            .bindings
            .selected_project_root(&context.config, &task.task)?
            .ok_or("The conversation continuation workspace is unavailable")?
            .canonicalize()
            .map_err(|_| "The conversation continuation workspace is unavailable")?;
        let token = context.continuations.issue_token_for_owner(
            &task.task,
            task.owner_generation,
            &root,
        )?;
        let owner = task
            .owner_generation
            .checked_add(1)
            .ok_or("Conversation owner generation overflow")?;
        let destination = self
            .config
            .chatgpt_project_url
            .as_deref()
            .unwrap_or("https://chatgpt.com/new");
        let issue = if schema_changed {
            task.store.issue_schema_rollover_pairing(
                &task.task,
                destination,
                owner,
                now_ms,
                state,
            )?
        } else {
            task.store.issue_rollover_pairing(
                &task.task,
                destination,
                owner,
                now_ms,
                &state.action,
            )?
        };
        let Some(issue) = issue else {
            return Ok(false);
        };
        if let Some(action_id) = state.action.id.as_deref() {
            self.clear_live_action(task.task.stable_key(), action_id)
                .await;
        }
        if let Err(error) = self
            .queue_pair_request(
                &task.task,
                &issue.reference,
                issue.generation,
                now_ms,
                Some((
                    token,
                    context
                        .config
                        .conversation_auth_token
                        .as_deref()
                        .map(str::to_owned),
                )),
                true,
            )
            .await
        {
            task.store
                .fail_creation_before_send(&task.task, issue.generation, &error, now_ms)?;
            return Err(error);
        }
        if task.store.load(&task.task)?.pairing.generation == issue.generation {
            self.tasks.write().await.insert(
                task.task.stable_key().to_owned(),
                AutomationTaskHandle::new(
                    task.task.clone(),
                    owner,
                    task.store.clone(),
                    task.chat.clone(),
                ),
            );
        }
        task.store
            .notify_state(&task.chat, &task.store.load(&task.task)?)
            .await;
        Ok(true)
    }

    pub fn start(self: &Arc<Self>) -> AutomationSupervisorWorker {
        super::turn_analytics::initialize();
        crate::usage_analytics::initialize();
        let supervisor = self.clone();
        let task = tokio::spawn(async move {
            loop {
                if let Err(error) = supervisor.scan_once(crate::markdown_chat::now_ms()).await {
                    tracing::warn!(%error, "agent automation scan failed");
                }
                let browser_paused = super::BrowserCompanionControl::new(&supervisor.config)
                    .is_ok_and(|control| {
                        let state = control.public_status();
                        state["manuallyStopped"] == true || state["waitingForServerRestart"] == true
                    });
                if !browser_paused {
                    loop {
                        match supervisor.run_next(crate::markdown_chat::now_ms()).await {
                            Ok(Some(_)) => {}
                            Ok(None)
                                if supervisor
                                    .has_ready_action(crate::markdown_chat::now_ms())
                                    .await =>
                            {
                                continue;
                            }
                            Ok(None) => break,
                            Err(error) => {
                                tracing::warn!(%error, "agent automation action failed");
                            }
                        }
                    }
                }
                tokio::select! {
                    _ = tokio::time::sleep(supervisor.retry_scan_delay(crate::markdown_chat::now_ms())) => {},
                    _ = supervisor.wake.notified() => {},
                }
            }
        });
        AutomationSupervisorWorker { task }
    }

    pub fn notify(&self) {
        self.wake.notify_one();
    }

    pub(crate) async fn is_registered(&self, task: &ConversationIdentity) -> bool {
        self.tasks.read().await.contains_key(task.stable_key())
    }

    pub(crate) async fn pairing_continuation_for_unbound(
        &self,
        reference: &str,
    ) -> Result<(String, u64), String> {
        let context = self
            .rollover
            .get()
            .ok_or("Task continuation is unavailable")?;
        let index = super::AgentAutomationStore::for_config(&context.config)?;
        let key = index.pairing_task_key(reference)?;
        let mut task = self
            .tasks
            .read()
            .await
            .get(&key)
            .cloned()
            .ok_or("The intended pairing task is unavailable; retry chat pairing")?;
        let state = task.store.load(&task.task)?;
        let pending = state
            .pairing
            .pending
            .as_ref()
            .ok_or("The pairing request is no longer pending")?;
        let now_ms = crate::markdown_chat::now_ms();
        task.store.validate_pending_pairing(
            &task.task,
            state.pairing.generation,
            pending.owner_generation,
            reference,
            now_ms,
        )?;
        let owner = context.continuations.owner_generation_for_task_key(&key)?;
        let root = context
            .bindings
            .selected_project_root(&context.config, &task.task)?
            .ok_or("The intended pairing task has no selected workspace")?
            .canonicalize()
            .map_err(|_| "The intended pairing workspace is unavailable")?;
        // Validate the reference and intended owner before issuing any handoff.
        // This does not consume the pairing proof or transfer task ownership.
        task.store.prepare_pairing_handoff(
            &task.task,
            state.pairing.generation,
            owner,
            reference,
            now_ms,
        )?;
        let token = context
            .continuations
            .issue_token_for_owner(&task.task, owner, &root)?;
        task.owner_generation = owner
            .checked_add(1)
            .ok_or("Conversation owner generation overflow")?;
        self.register_task(task).await?;
        Ok((token, state.pairing.generation))
    }

    pub async fn register_task(&self, mut task: AutomationTaskHandle) -> Result<(), String> {
        if !self.config.enabled {
            return Err("Agent-chat browser automation is disabled".into());
        }
        let previous = self.tasks.read().await.get(task.task.stable_key()).cloned();
        let first_registration = previous.is_none();
        // Owner UI lookups can reconstruct only the current stable key. Keep
        // the full identity learned from MCP so legacy task state follows it.
        if task.task.legacy_stable_key() == task.task.stable_key()
            && let Some(previous) = previous
        {
            task.task = previous.task;
        }
        let state = task.store.load(&task.task)?;
        // A pending handoff belongs to the next owner, including when pairing
        // an existing empty chat. Late source activity must not revoke it.
        if let Some(pending) = &state.pairing.pending
            && task.owner_generation.checked_add(1) == Some(pending.owner_generation)
        {
            task.owner_generation = pending.owner_generation;
        }
        let owner_changed = state
            .pairing
            .verified_owner_generation
            .is_some_and(|generation| generation != task.owner_generation)
            || state
                .pairing
                .pending
                .as_ref()
                .is_some_and(|pending| pending.owner_generation != task.owner_generation);
        let orphaned_pairing = first_registration
            && matches!(
                state.action.status,
                AutomationActionStatus::Queued
                    | AutomationActionStatus::Navigating
                    | AutomationActionStatus::Sending
                    | AutomationActionStatus::Generating
            )
            && state.action.kind == Some(AutomationActionKind::PairVerify);
        let orphaned_action = (first_registration
            && state.action.status == AutomationActionStatus::Queued
            && state.browser_cooldown_until_ms().is_none()
            && state.action.kind != Some(AutomationActionKind::PairVerify))
        .then(|| state.action.id.clone())
        .flatten();
        if owner_changed || orphaned_pairing {
            task.store.invalidate_pairing_and_supersede_action(
                &task.task,
                crate::markdown_chat::now_ms(),
            )?;
            if orphaned_pairing {
                task.store.record_interrupted_pairing(&task.task)?;
            }
            self.queue
                .lock()
                .await
                .retain(|queued| queued.task_key != task.task.stable_key());
            self.live.write().await.remove(task.task.stable_key());
        } else if let Some(action_id) = orphaned_action {
            if !task
                .store
                .restore_disabled_queued_retry(&task.task, &action_id)?
            {
                task.store.clear_persisted_queued_action(
                    &task.task,
                    &action_id,
                    crate::markdown_chat::now_ms(),
                )?;
            }
            self.queue
                .lock()
                .await
                .retain(|queued| queued.task_key != task.task.stable_key());
            self.live.write().await.remove(task.task.stable_key());
        }
        task.chat.ensure().await?;
        let activity = task.chat.offline_activity().await?;
        task.store.sync_goal_presence(
            &task.task,
            activity.last_call_for_presence(crate::markdown_chat::now_ms()),
            activity.agent_waiting_until_ms,
            activity.agent_stop_requested,
            crate::markdown_chat::now_ms(),
        )?;
        if !activity.agent_stop_requested {
            let state = task.store.load(&task.task)?;
            if let Err(error) = self
                .reconcile_chat_delivery(&task, &state, crate::markdown_chat::now_ms())
                .await
            {
                tracing::warn!(%error, "could not reconcile agent message acknowledgment");
            }
        }
        self.tasks
            .write()
            .await
            .insert(task.task.stable_key().to_string(), task);
        self.notify();
        Ok(())
    }

    pub async fn prepare_new_chat(
        &self,
        project_url: Option<&str>,
    ) -> Result<super::CapabilityCatalog, String> {
        let cancellation = CancellationToken::new();
        let result = tokio::time::timeout(Duration::from_secs(180), async {
            let _guard = self.run_lock.lock().await;
            self.executor
                .prepare_new_chat(project_url, cancellation.clone())
                .await
        })
        .await;
        cancellation.cancel();
        result.map_err(|_| {
            "New-chat preparation timed out; retry when the browser is available".to_string()
        })?
    }

    pub async fn queued_count(&self) -> usize {
        self.queue.lock().await.len()
    }

    pub async fn queue_position(&self, task: &ConversationIdentity) -> Option<(usize, usize)> {
        let queue = self.queue.lock().await;
        let total = queue.len();
        queue
            .iter()
            .position(|execution| execution.task_key == task.stable_key())
            .map(|position| (position + 1, total))
    }

    pub async fn discard_queued_actions(&self, task: &ConversationIdentity) -> usize {
        let task_key = task.stable_key();
        let mut queue = self.queue.lock().await;
        let before = queue.len();
        queue.retain(|execution| execution.task_key != task_key);
        let removed = before.saturating_sub(queue.len());
        drop(queue);
        if removed > 0 {
            self.wake.notify_one();
        }
        removed
    }

    pub async fn discard_queued_work(&self, task: &ConversationIdentity) -> usize {
        let mut queue = self.queue.lock().await;
        let before = queue.len();
        queue.retain(|execution| {
            execution.task_key != task.stable_key()
                || matches!(
                    execution.request.prompt,
                    ActionPrompt::PairVerify { .. } | ActionPrompt::CreateChat { .. }
                )
        });
        before.saturating_sub(queue.len())
    }

    pub async fn run_queued_action_now(
        &self,
        task: &ConversationIdentity,
        expected_action_id: &str,
        now_ms: u64,
    ) -> Result<super::AutomationActionState, String> {
        let handle = self
            .tasks
            .read()
            .await
            .get(task.stable_key())
            .cloned()
            .ok_or("Browser automation task is not registered")?;
        let initial = handle.store.load(task)?;
        if initial.action.id.as_deref() != Some(expected_action_id) || !initial.can_run_queued_now()
        {
            return Err("The queued browser action changed or already started".into());
        }
        if self.prepare_schema_rollover(task).await? {
            self.scan_once(now_ms).await?;
            self.notify();
            return Ok(handle.store.load(task)?.action);
        }
        let scan_guard = self.scan_lock.lock().await;
        let mut queue = self.queue.lock().await;
        let state = handle.store.load(task)?;
        if state.action.id.as_deref() != Some(expected_action_id) || !state.can_run_queued_now() {
            return Err("The queued browser action changed or already started".into());
        }
        if let Some(position) = queue.iter().position(|execution| {
            execution.task_key == task.stable_key()
                && execution.request.action_id == expected_action_id
        }) {
            let action = handle
                .store
                .run_queued_action_now(task, expected_action_id, now_ms)?;
            let mut execution = queue.remove(position).expect("queued action position");
            execution.request.bypass_conversation_cooldown = true;
            execution.request.agent_ticket = None;
            let priority = action_priority(execution_kind(&execution));
            let first = queue
                .iter()
                .position(|other| action_priority(execution_kind(other)) >= priority)
                .unwrap_or(queue.len());
            queue.insert(first, execution);
            drop(queue);
            drop(scan_guard);
            self.notify();
            return Ok(action);
        }
        if state.browser_cooldown_until_ms().is_none() {
            return Err("The browser action already started or left the queue".into());
        }
        // A cooldown can survive restart without its in-memory request. Exclude
        // dispatch before rebuilding it, rather than racing a just-popped Send.
        let run_guard = self
            .run_lock
            .try_lock()
            .map_err(|_| "The browser action already started or left the queue")?;
        handle
            .store
            .reset_failed_action_for_retry(task, expected_action_id, now_ms)?;
        drop(queue);
        drop(run_guard);
        drop(scan_guard);
        self.scan_once(now_ms).await?;
        self.notify();
        Ok(handle.store.load(task)?.action)
    }

    pub async fn cancel_action(
        &self,
        task: &ConversationIdentity,
        expected_action_id: &str,
        now_ms: u64,
    ) -> Result<super::AutomationActionState, String> {
        // Serialize with automatic retry preparation. Hold the queue through the
        // state change so the runner cannot start the action being cancelled.
        let _scan_guard = self.scan_lock.lock().await;
        let handle = self
            .tasks
            .read()
            .await
            .get(task.stable_key())
            .cloned()
            .ok_or("Browser automation task is not registered")?;
        let mut queue = self.queue.lock().await;
        let state = handle.store.load(&handle.task)?;
        if state.action.id.as_deref() != Some(expected_action_id) {
            return Err("The browser action changed before it could be cancelled".into());
        }
        let _waiting_guard = if state.action.status == AutomationActionStatus::Queued
            && !queue.iter().any(|execution| {
                execution.task_key == task.stable_key()
                    && execution.request.action_id == expected_action_id
            }) {
            if state.browser_cooldown_until_ms().is_none() {
                return Err("The browser action already started or left the queue".into());
            }
            // A persisted cooldown can have no in-memory ticket after restart.
            // Serialize with dispatch before cancelling that pending intent.
            Some(
                self.run_lock
                    .try_lock()
                    .map_err(|_| "The browser action already started or left the queue")?,
            )
        } else {
            None
        };
        let action = handle
            .store
            .cancel_action(&handle.task, expected_action_id, now_ms)?;
        handle.store.clear_turn_renewal(&handle.task)?;
        queue.retain(|execution| {
            execution.task_key != task.stable_key()
                || execution.request.action_id != expected_action_id
        });
        let action = if action.kind == Some(AutomationActionKind::PairVerify) {
            handle.store.unlink_pairing(&handle.task, now_ms)?;
            handle.store.load(&handle.task)?.action
        } else {
            action
        };
        drop(queue);
        self.live.write().await.remove(handle.task.stable_key());
        self.notify();
        Ok(action)
    }

    pub async fn pause_action_retries(
        &self,
        task: &ConversationIdentity,
        expected_action_id: &str,
    ) -> Result<super::AutomationActionState, String> {
        let _scan_guard = self.scan_lock.lock().await;
        let handle = self
            .tasks
            .read()
            .await
            .get(task.stable_key())
            .cloned()
            .ok_or("Browser automation task is not registered")?;
        let mut queue = self.queue.lock().await;
        let state = handle.store.load(&handle.task)?;
        let _waiting_guard = if state.action.status == AutomationActionStatus::Queued
            && !queue.iter().any(|q| {
                q.request.action_id == expected_action_id && q.task_key == task.stable_key()
            }) {
            if state.browser_cooldown_until_ms().is_none() {
                return Err("The browser action already started or left the queue".into());
            }
            Some(
                self.run_lock
                    .try_lock()
                    .map_err(|_| "The browser action already started or left the queue")?,
            )
        } else {
            None
        };
        let action = handle
            .store
            .pause_action_retries(&handle.task, expected_action_id)?;
        queue.retain(|q| {
            q.request.action_id != expected_action_id || q.task_key != task.stable_key()
        });
        drop(queue);
        self.live.write().await.remove(task.stable_key());
        self.notify();
        Ok(action)
    }

    pub async fn retry_failed_action(
        &self,
        task: &ConversationIdentity,
        expected_action_id: &str,
        now_ms: u64,
    ) -> Result<super::AutomationActionState, String> {
        let handle = self
            .tasks
            .read()
            .await
            .get(task.stable_key())
            .cloned()
            .ok_or("Browser automation task is not registered")?;
        if handle.store.load(task)?.action.id.as_deref() == Some(expected_action_id)
            && self.prepare_schema_rollover(task).await?
        {
            self.scan_once(now_ms).await?;
            self.notify();
            return Ok(handle.store.load(task)?.action);
        }
        // An older companion rejected this exact condition before touching Send.
        // Preserve the evidence so the authenticated owner can retry that saved action.
        if let Some(live) = self.live_state(task).await
            && live.action_id.as_deref() == Some(expected_action_id)
            && live.status.as_deref()
                == Some("The shared ChatGPT conversation is already generating")
        {
            let saved = handle.store.load(&handle.task)?;
            if saved.action.id.as_deref() == Some(expected_action_id)
                && saved.action.status == AutomationActionStatus::Interference
                && saved.action.send_accepted != Some(true)
            {
                handle.store.update_action_status(
                    &handle.task,
                    expected_action_id,
                    AutomationActionStatus::Interference,
                    live.status.as_deref(),
                    None,
                    now_ms,
                )?;
            }
        }
        handle
            .store
            .reset_failed_action_for_retry(&handle.task, expected_action_id, now_ms)?;
        self.live.write().await.remove(handle.task.stable_key());
        let scanned = self.scan_once(now_ms).await;
        handle.store.clear_unused_manual_retry(&handle.task)?;
        scanned?;
        self.notify();
        Ok(handle.store.load(&handle.task)?.action)
    }

    #[doc(hidden)]
    pub async fn registered_count(&self) -> usize {
        self.tasks.read().await.len()
    }

    pub async fn queue_pair_verification(
        &self,
        task: &ConversationIdentity,
        reference: &str,
        generation: u64,
        now_ms: u64,
    ) -> Result<(), String> {
        self.queue_pair_request(task, reference, generation, now_ms, None, false)
            .await
    }

    pub async fn queue_new_chat(
        &self,
        task: &ConversationIdentity,
        reference: &str,
        generation: u64,
        now_ms: u64,
        continuation_token: String,
        setup_reference: Option<String>,
    ) -> Result<(), String> {
        self.queue_pair_request(
            task,
            reference,
            generation,
            now_ms,
            Some((continuation_token, setup_reference)),
            false,
        )
        .await
    }

    async fn queue_pair_request(
        &self,
        task: &ConversationIdentity,
        reference: &str,
        generation: u64,
        now_ms: u64,
        creation: Option<(String, Option<String>)>,
        automatic_rollover: bool,
    ) -> Result<(), String> {
        let handle = self
            .tasks
            .read()
            .await
            .get(task.stable_key())
            .cloned()
            .ok_or("Browser automation task is not registered")?;
        let connector_name = handle
            .store
            .connector_name(&self.config.connector_name)?
            .ok_or("Configure the exact ChatGPT connector name before using browser automation")?;
        let saved = handle.store.load(&handle.task)?;
        let owner_generation = if creation.is_some() && saved.pairing.creating_chat {
            saved
                .pairing
                .pending
                .as_ref()
                .map_or(handle.owner_generation, |pending| pending.owner_generation)
        } else {
            handle.owner_generation
        };
        let pairing = handle.store.validate_pending_pairing(
            &handle.task,
            generation,
            owner_generation,
            reference,
            now_ms,
        )?;
        let candidate_url = pairing
            .candidate_url
            .ok_or("Agent automation pairing has no candidate URL")?;
        let state = handle.store.load(&handle.task)?;
        let account_fingerprint = state
            .pairing
            .account_fingerprint
            .or(state.selection.account_fingerprint)
            .ok_or("Sign in to ChatGPT before pairing")?;
        let catalog_revision = if creation.is_some() {
            state
                .selection
                .catalog_revision
                .clone()
                .unwrap_or_else(|| "conversation-current".into())
        } else {
            "conversation-current".to_owned()
        };
        let (model, effort) = if creation.is_some() {
            (
                state.selection.model.clone(),
                state.selection.effort.clone(),
            )
        } else {
            (None, None)
        };
        let retain_pairing_window = creation.is_some()
            && (automatic_rollover || owner_generation > 1)
            && self.executor.supports_pairing_reservation();
        let mut queue = self.queue.lock().await;
        let previous = queue
            .iter()
            .position(|queued| queued.task_key == handle.task.stable_key());
        if queue.len().saturating_sub(usize::from(previous.is_some()))
            >= self.config.max_queued_actions
        {
            return Err("Browser automation queue capacity is exhausted".into());
        }
        let action_id = random_id("action")?;
        handle.store.queue_action(
            &handle.task,
            &action_id,
            AutomationActionKind::PairVerify,
            generation,
            None,
            now_ms,
        )?;
        if let Some(position) = previous {
            queue.remove(position);
        }
        queue.push_back(AutomationExecution {
            task_key: handle.task.stable_key().to_string(),
            owner_generation,
            account_fingerprint: account_fingerprint.clone(),
            catalog_revision,
            activity_epoch: if automatic_rollover {
                "conversation-rollover"
            } else {
                "pair-verification"
            }
            .into(),
            activity_sequence: generation,
            tool_call_count_at_start: 0,
            unread_message_ids: Vec::new(),
            transcript_revision: String::new(),
            pairing_commit: Some(PairingCommit::new(
                handle.task.clone(),
                handle.store.clone(),
                generation,
            )),
            request: ExecuteActionRequest {
                action_id,
                idempotency_key: random_id("execute")?,
                url: candidate_url.clone(),
                pairing_generation: generation,
                connector_name: connector_name.clone(),
                model,
                effort,
                expected_account_fingerprint: account_fingerprint,
                prompt: match creation {
                    Some((continuation_token, setup_reference)) => ActionPrompt::CreateChat {
                        continuation_token,
                        setup_reference,
                        reference: reference.to_string(),
                    },
                    None => ActionPrompt::PairVerify {
                        reference: reference.to_string(),
                    },
                },
                expected_url: candidate_url,
                auto_approve_tool_calls: self.config.auto_approve_tool_calls,
                bypass_conversation_cooldown: false,
                retain_pairing_window,
                pairing_continuation_of: None,
                agent_ticket: None,
            },
        });
        queue
            .make_contiguous()
            .sort_by_key(|execution| action_priority(execution_kind(execution)));
        self.notify();
        Ok(())
    }

    #[doc(hidden)]
    pub async fn next_queued_kind(&self) -> Option<AutomationActionKind> {
        self.queue.lock().await.front().map(execution_kind)
    }

    pub async fn live_state(&self, task: &ConversationIdentity) -> Option<AutomationLiveState> {
        self.live.read().await.get(task.stable_key()).cloned()
    }

    fn retry_scan_delay(&self, now_ms: u64) -> Duration {
        Duration::from_millis(
            self.next_retry_scan_ms
                .load(Ordering::Relaxed)
                .saturating_sub(now_ms)
                .min(30_000),
        )
    }

    pub async fn scan_once(&self, now_ms: u64) -> Result<(), String> {
        let _scan_guard = self.scan_lock.lock().await;
        self.next_retry_scan_ms
            .store(now_ms.saturating_add(30_000), Ordering::Relaxed);
        let check_approvals = self.config.auto_approve_tool_calls
            && now_ms >= self.approval_next_scan_ms.load(Ordering::Relaxed);
        if check_approvals {
            self.approval_next_scan_ms
                .store(now_ms.saturating_add(5_000), Ordering::Relaxed);
        }
        let tasks = self
            .tasks
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for task in tasks {
            let activity = task.chat.offline_activity().await?;
            task.store.sync_goal_presence(
                &task.task,
                activity.last_call_for_presence(now_ms),
                activity.agent_waiting_until_ms,
                activity.agent_stop_requested,
                now_ms,
            )?;
            let mut state = task.store.load(&task.task)?;
            if state.pairing.schema_rollover_pending
                && state.pairing.verified_owner_generation == Some(task.owner_generation)
            {
                if state.pairing.connector_schema_version == self.current_schema_version() {
                    task.store
                        .clear_schema_rollover_hold(&task.task, &state.pairing)?;
                    state = task.store.load(&task.task)?;
                } else if let Ok(_run_guard) = self.run_lock.try_lock() {
                    self.queue_replacement_conversation(&task, &state, true, now_ms)
                        .await?;
                }
                if state.pairing.schema_rollover_pending {
                    continue;
                }
            }
            if state.action.status == AutomationActionStatus::ConversationLimit {
                if let Some(action_id) = state.action.id.as_deref()
                    && let Ok(_run_guard) = self.run_lock.try_lock()
                {
                    self.queue_conversation_rollover(&task, action_id, now_ms)
                        .await?;
                }
                continue;
            }
            if !state.automatic_retries_enabled
                && state.action.status == AutomationActionStatus::Queued
                && state.action.automatic_retry_in_progress
                && let Some(action_id) = state.action.id.as_deref()
                && let Ok(_run_guard) = self.run_lock.try_lock()
                && task
                    .store
                    .restore_disabled_queued_retry(&task.task, action_id)?
            {
                self.queue.lock().await.retain(|q| {
                    q.request.action_id != action_id || q.task_key != task.task.stable_key()
                });
                self.clear_live_action(task.task.stable_key(), action_id)
                    .await;
                state = task.store.load(&task.task)?;
            }
            if state.browser_cooldown_until_ms().is_some()
                && !activity.agent_stop_requested
                && self.reconcile_chat_delivery(&task, &state, now_ms).await?
            {
                self.queue.lock().await.retain(|q| {
                    q.task_key != task.task.stable_key()
                        || Some(q.request.action_id.as_str()) != state.action.id.as_deref()
                });
                state = task.store.load(&task.task)?;
            }
            if let Some(until) = state.browser_cooldown_until_ms() {
                let online = activity.is_online(now_ms, task.store.offline_after_ms());
                let queued = self
                    .queue
                    .lock()
                    .await
                    .iter()
                    .any(|q| q.task_key == task.task.stable_key());
                if online || activity.agent_stop_requested || (!queued && now_ms >= until) {
                    if task
                        .store
                        .restore_missing_cooldown_action(&task.task, &state.action)?
                    {
                        self.queue.lock().await.retain(|q| {
                            q.task_key != task.task.stable_key()
                                || Some(q.request.action_id.as_str()) != state.action.id.as_deref()
                        });
                    }
                    state = task.store.load(&task.task)?;
                } else if now_ms < until {
                    self.next_retry_scan_ms.fetch_min(until, Ordering::Relaxed);
                    continue;
                }
            }
            // Wake at the actual jittered deadline, rather than rounding all
            // chats into the same 30-second scan and undoing their jitter.
            if let Some(at) = state
                .automatic_retry_at_ms()
                .or_else(|| state.goal_retry_at_ms())
                .filter(|at| *at > now_ms)
            {
                self.next_retry_scan_ms.fetch_min(at, Ordering::Relaxed);
            }
            if let Some(resume) = state.approval_resume.as_ref() {
                let made_progress = activity
                    .last_call_at_ms
                    .is_some_and(|last| last > resume.resumed_at_ms);
                let current = resume.owner_generation == task.owner_generation
                    && resume.pairing_generation == state.pairing.generation
                    && state.pairing.verified_owner_generation == Some(task.owner_generation)
                    && resume.goal_revision == state.goal.as_ref().map(|g| g.revision);
                if current
                    && !activity.agent_stop_requested
                    && !made_progress
                    && let Some(until) = state
                        .approval_resume_until_ms()
                        .filter(|until| now_ms < *until)
                {
                    self.next_retry_scan_ms.fetch_min(until, Ordering::Relaxed);
                    continue;
                }
                task.store
                    .clear_approval_resume(&task.task, &state, made_progress && current)?;
                state = task.store.load(&task.task)?;
            }
            if check_approvals
                && !activity.agent_stop_requested
                && state.pairing.verified_owner_generation == Some(task.owner_generation)
                && let (Some(url), Some(account), Some(connector)) = (
                    state.pairing.canonical_url.as_deref(),
                    state.pairing.account_fingerprint.as_deref(),
                    task.store.connector_name(&self.config.connector_name)?,
                )
                && let Ok(_run_guard) = self.run_lock.try_lock()
                && let Ok(true) = self
                    .executor
                    .approve_pending_conversation(url, account, &connector)
                    .await
            {
                let epoch = activity
                    .activity_epoch
                    .as_deref()
                    .unwrap_or("legacy-activity");
                let sequence = if activity.activity_sequence == 0 {
                    activity.tool_call_count
                } else {
                    activity.activity_sequence
                };
                self.record_approved_resume(&task, &state, epoch, sequence, now_ms)
                    .await?;
                continue;
            }
            let activity_epoch = activity
                .activity_epoch
                .as_deref()
                .unwrap_or("legacy-activity");
            let activity_sequence = if activity.activity_sequence == 0 {
                activity.tool_call_count
            } else {
                activity.activity_sequence
            };
            if task.store.reset_goal_recovery_after_activity(
                &task.task,
                &state,
                activity_epoch,
                activity_sequence,
            )? {
                state = task.store.load(&task.task)?;
            }
            task.store.notify_state(&task.chat, &state).await;
            let renewal_config = task.store.turn_renewal_settings()?;
            let renewal_enabled = state.turn_renewal.send.as_ref().is_some_and(|send| {
                renewal_config.enabled_for(send.model.as_deref(), send.effort.as_deref())
            });
            super::turn_analytics::observe(
                task.chat.path(),
                task.task.stable_key(),
                task.owner_generation,
                &state,
                activity.last_call_at_ms,
                activity.agent_stop_requested,
                renewal_enabled,
                now_ms,
            );
            let renewal_invalid = activity.agent_stop_requested
                || !renewal_enabled
                || state.turn_renewal.send.as_ref().is_some_and(|send| {
                    send.owner_generation != task.owner_generation
                        || send.pairing_generation != state.pairing.generation
                });
            if renewal_invalid
                && (state.turn_renewal.handoff.is_some()
                    || activity.agent_stop_requested && state.turn_renewal.send.is_some())
            {
                task.store.clear_turn_renewal(&task.task)?;
                self.queue.lock().await.retain(|q| {
                    q.task_key != task.task.stable_key()
                        || execution_kind(q) != AutomationActionKind::TurnRenewal
                });
                state = task.store.load(&task.task)?;
            }
            if let Some(handoff) = state.turn_renewal.handoff.clone() {
                if state.action.kind == Some(AutomationActionKind::TurnRenewal) {
                    let retry_goal = handoff.report_complete
                        && state
                            .goal
                            .as_ref()
                            .is_some_and(|g| g.status == GoalStatus::Active)
                        && !activity.is_online(now_ms, task.store.offline_after_ms())
                        && matches!(
                            state.action.status,
                            AutomationActionStatus::Failed
                                | AutomationActionStatus::Ambiguous
                                | AutomationActionStatus::Stalled
                        )
                        && state.automatic_retry_at_ms().is_some_and(|at| now_ms >= at);
                    if retry_goal {
                        // A goal retry contains only Continue, never user messages. Require
                        // fresh proof that its final handoff is still the latest completed
                        // answer; never stop an active response to retry renewal.
                        if let (Some(url), Some(account)) = (
                            state.pairing.canonical_url.as_deref(),
                            state.pairing.account_fingerprint.as_deref(),
                        ) && let Ok(_browser_guard) = self.run_lock.try_lock()
                            && let Ok(Ok(true)) = tokio::time::timeout(
                                Duration::from_secs(15),
                                self.executor
                                    .turn_handoff_complete(url, account, &handoff.id),
                            )
                            .await
                            && task.store.retry_goal_turn_renewal(
                                &task.task,
                                &state,
                                activity_epoch,
                                activity_sequence,
                                now_ms,
                            )?
                        {
                            state = task.store.load(&task.task)?;
                        } else {
                            continue;
                        }
                    } else if state.action.send_accepted == Some(true) {
                        continue;
                    }
                }
                if !handoff.report_complete {
                    if let (Some(url), Some(account)) = (
                        state.pairing.canonical_url.as_deref(),
                        state.pairing.account_fingerprint.as_deref(),
                    ) && state.pairing.verified_owner_generation == Some(task.owner_generation)
                        && now_ms.saturating_sub(handoff.requested_at_ms) >= 5_000
                        && let Ok(_browser_guard) = self.run_lock.try_lock()
                        && let Ok(Ok(true)) = tokio::time::timeout(
                            Duration::from_secs(15),
                            self.executor
                                .turn_handoff_complete(url, account, &handoff.id),
                        )
                        .await
                    {
                        task.store.confirm_turn_handoff(&task.task, &handoff.id)?;
                        state = task.store.load(&task.task)?;
                    } else {
                        continue;
                    }
                }
            }
            if !activity.agent_stop_requested
                && self.reconcile_chat_delivery(&task, &state, now_ms).await?
            {
                state = task.store.load(&task.task)?;
            }
            if !activity.agent_stop_requested
                && state.pairing.verified_owner_generation == Some(task.owner_generation)
                && state.action.kind == Some(AutomationActionKind::OfflineGoal)
                && state.action.updated_at_ms.is_some_and(|started| {
                    activity.last_call_at_ms.is_some_and(|last| last > started)
                })
                && (state.action.goal_turn_counted
                    || state
                        .goal_wake
                        .nudged_epoch
                        .as_deref()
                        .zip(state.goal_wake.nudged_sequence)
                        .is_some_and(|(epoch, sequence)| {
                            let current = if activity.activity_sequence == 0 {
                                activity.tool_call_count
                            } else {
                                activity.activity_sequence
                            };
                            activity
                                .activity_epoch
                                .as_deref()
                                .unwrap_or("legacy-activity")
                                != epoch
                                || current > sequence
                        }))
            {
                // Tool activity proves the submitted goal has reached the agent. Browser
                // ownership is no longer needed, even if the model is still working.
                let epoch = activity
                    .activity_epoch
                    .as_deref()
                    .unwrap_or("legacy-activity");
                let sequence = if activity.activity_sequence == 0 {
                    activity.tool_call_count
                } else {
                    activity.activity_sequence
                };
                if task.store.reconcile_completed_goal_action(
                    &task.task, &state, epoch, sequence, true, now_ms,
                )? {
                    if let Some(action_id) = state.action.id.as_deref() {
                        self.clear_live_action(task.task.stable_key(), action_id)
                            .await;
                    }
                    state = task.store.load(&task.task)?;
                }
            }
            if !activity.agent_stop_requested
                && state.action.kind == Some(AutomationActionKind::OfflineGoal)
                && activity.is_online(now_ms, task.store.offline_after_ms())
            {
                // Preserve the proven-delivery reconciliation above. A queued
                // retry is retired only while the worker cannot start its Send.
                let queued = state.action.status == AutomationActionStatus::Queued;
                let run_guard = if queued {
                    self.run_lock.try_lock().ok()
                } else {
                    None
                };
                if (!queued || run_guard.is_some())
                    && task.store.reconcile_resumed_goal_retry(
                        &task.task,
                        &state,
                        task.owner_generation,
                        activity.last_call_at_ms,
                        now_ms,
                    )?
                {
                    if let Some(action_id) = state.action.id.as_deref() {
                        self.queue
                            .lock()
                            .await
                            .retain(|queued| queued.request.action_id != action_id);
                        self.clear_live_action(task.task.stable_key(), action_id)
                            .await;
                    }
                    state = task.store.load(&task.task)?;
                    task.store.notify_state(&task.chat, &state).await;
                }
            }
            if state.action.status == AutomationActionStatus::Cancelled
                && state.goal_retry_at_ms().is_none()
            {
                continue;
            }
            let renewing = state
                .turn_renewal
                .handoff
                .as_ref()
                .is_some_and(|h| h.report_complete)
                || state.pairing.resume_task;
            if activity.agent_stop_requested
                || !renewing
                    && (activity.is_online(now_ms, task.store.offline_after_ms())
                        || activity
                            .agent_waiting_until_ms
                            .is_some_and(|until| now_ms < until))
            {
                continue;
            }
            let Some(url) = state.pairing.canonical_url.clone() else {
                continue;
            };
            if state.pairing.verified_owner_generation != Some(task.owner_generation) {
                continue;
            }
            if state.action.kind == Some(AutomationActionKind::OfflineGoal)
                && matches!(
                    state.action.status,
                    AutomationActionStatus::Navigating
                        | AutomationActionStatus::Sending
                        | AutomationActionStatus::Generating
                        | AutomationActionStatus::Ambiguous
                )
                && let (Some(action_id), Some(account)) = (
                    state.action.id.as_deref(),
                    state
                        .pairing
                        .account_fingerprint
                        .as_deref()
                        .or(state.selection.account_fingerprint.as_deref()),
                )
                && let Ok(_run_guard) = self.run_lock.try_lock()
            {
                // The old observer may have disappeared during a service restart. Recover only
                // an exact, completed goal-only receipt; never replay an uncertain user send.
                if let Ok(Ok(Some(snapshot))) = tokio::time::timeout(
                    Duration::from_secs(10),
                    self.executor.retained_action(action_id, account),
                )
                .await
                    && snapshot.action_id == action_id
                    && snapshot.state == CompanionActionState::Idle
                    && snapshot.send_accepted
                    && snapshot.attention.is_none()
                    && snapshot
                        .canonical_url
                        .as_deref()
                        .is_some_and(|observed| same_chatgpt_conversation_url(observed, &url))
                {
                    let made_progress = state.action.updated_at_ms.is_some_and(|started| {
                        activity.last_call_at_ms.is_some_and(|last| last >= started)
                    });
                    let epoch = activity
                        .activity_epoch
                        .as_deref()
                        .unwrap_or("legacy-activity");
                    let sequence = if activity.activity_sequence == 0 {
                        activity.tool_call_count
                    } else {
                        activity.activity_sequence
                    };
                    if task.store.reconcile_completed_goal_action(
                        &task.task,
                        &state,
                        epoch,
                        sequence,
                        made_progress,
                        now_ms,
                    )? {
                        self.live.write().await.remove(task.task.stable_key());
                        state = task.store.load(&task.task)?;
                    }
                }
            }
            let Some(connector_name) = task.store.connector_name(&self.config.connector_name)?
            else {
                continue;
            };
            let obsolete_action =
                renewing && state.action.kind != Some(AutomationActionKind::TurnRenewal);
            if !obsolete_action
                && matches!(
                    state.action.status,
                    AutomationActionStatus::Navigating
                        | AutomationActionStatus::Sending
                        | AutomationActionStatus::Generating
                )
                && state.action.updated_at_ms.is_some_and(|started| {
                    now_ms.saturating_sub(started)
                        >= self.action_timeouts.total.as_millis().min(u64::MAX as u128) as u64
                })
                && let Ok(_run_guard) = self.run_lock.try_lock()
                && task
                    .store
                    .expire_unobserved_action(&task.task, &state.action)?
            {
                state = task.store.load(&task.task)?;
            }
            if !obsolete_action
                && (state.action.automatic_retry_paused
                    || !state.automatic_retries_enabled && state.action_needs_attention_at(now_ms))
            {
                continue;
            }
            if !obsolete_action && state.goal_recovery_exhausted() {
                task.store
                    .mark_goal_recovery_exhausted(&task.task, &state)?;
                continue;
            }
            if let Some(retry_at) = state.goal_retry_at_ms().filter(|_| !obsolete_action) {
                if now_ms < retry_at {
                    continue;
                }
                if task
                    .store
                    .reset_offline_goal_for_retry(&task.task, &state, now_ms)?
                {
                    self.live.write().await.remove(task.task.stable_key());
                    state = task.store.load(&task.task)?;
                } else {
                    continue;
                }
            }
            if let Some(retry_at) = state.automatic_retry_at_ms().filter(|_| !obsolete_action) {
                if now_ms < retry_at {
                    continue;
                }
                if task.store.reset_automatic_failure_for_retry(
                    &task.task,
                    &state.action,
                    now_ms,
                )? {
                    self.live.write().await.remove(task.task.stable_key());
                    state = task.store.load(&task.task)?;
                } else {
                    continue;
                }
            }
            if !obsolete_action && blocks_automatic_requeue(state.action.status) {
                continue;
            }
            let Some(account_fingerprint) = state
                .pairing
                .account_fingerprint
                .clone()
                .or(state.selection.account_fingerprint.clone())
            else {
                continue;
            };
            if state
                .selection
                .account_fingerprint
                .as_ref()
                .is_some_and(|saved| saved != &account_fingerprint)
            {
                continue;
            }
            let model = state.selection.model.clone();
            let effort = state.selection.effort.clone();
            let catalog_revision = state
                .selection
                .catalog_revision
                .clone()
                .unwrap_or_else(|| "conversation-current".into());
            let unread = task.chat.unread_snapshot().await?;
            let unread_ids = unread
                .messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<std::collections::HashSet<_>>();
            let retained_accepted =
                if state.chat_mirror.owner_generation == Some(task.owner_generation) {
                    state
                        .chat_mirror
                        .message_ids
                        .iter()
                        .filter(|id| unread_ids.contains(id.as_str()))
                        .cloned()
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
            if state.chat_mirror.owner_generation != Some(task.owner_generation)
                || retained_accepted.len() != state.chat_mirror.message_ids.len()
            {
                task.store.retain_accepted_messages(
                    &task.task,
                    task.owner_generation,
                    &retained_accepted,
                )?;
            }
            let accepted = retained_accepted
                .iter()
                .collect::<std::collections::HashSet<_>>();
            let all_pending_messages = unread
                .messages
                .iter()
                .filter(|message| !accepted.contains(&message.id))
                .cloned()
                .collect::<Vec<_>>();
            let goal_revision = state
                .goal
                .as_ref()
                .filter(|goal| goal.status == GoalStatus::Active)
                .map(|goal| goal.revision);
            if !renewing
                && !all_pending_messages.is_empty()
                && retained_accepted.len() >= MAX_CHAT_MIRROR_IDS
            {
                let kind = if goal_revision.is_some() {
                    AutomationActionKind::OfflineCombined
                } else {
                    AutomationActionKind::OfflineChat
                };
                let action_id = random_id("action")?;
                task.store.record_queue_failure(
                    &task.task,
                    QueueFailure {
                        action_id: &action_id,
                        kind,
                        pairing_generation: state.pairing.generation,
                        goal_revision,
                        message: "Browser automation accepted chat history is full; consume pending messages before retrying",
                        now_ms,
                    },
                )?;
                continue;
            }
            let all_pending_messages = if renewing {
                Vec::new()
            } else {
                all_pending_messages
            };
            let ticket_overhead = if self
                .rollover
                .get()
                .is_some_and(|context| context.config.experimental.agent_tickets)
            {
                super::companion::browser_ticket_prompt_overhead()
            } else {
                0
            };
            let pending_messages =
                match bounded_unread_batch(&all_pending_messages, goal_revision, ticket_overhead) {
                    Ok(messages) => messages,
                    Err(error) => {
                        let kind = if goal_revision.is_some() {
                            AutomationActionKind::OfflineCombined
                        } else {
                            AutomationActionKind::OfflineChat
                        };
                        let action_id = random_id("action")?;
                        task.store.record_queue_failure(
                            &task.task,
                            QueueFailure {
                                action_id: &action_id,
                                kind,
                                pairing_generation: state.pairing.generation,
                                goal_revision,
                                message: &error,
                                now_ms,
                            },
                        )?;
                        continue;
                    }
                };
            let activity_epoch = activity
                .activity_epoch
                .clone()
                .unwrap_or_else(|| "legacy-activity".into());
            let activity_sequence = if activity.activity_sequence == 0 {
                activity.tool_call_count
            } else {
                activity.activity_sequence
            };
            if !renewing
                && pending_messages.is_empty()
                && goal_revision.is_some()
                && state.goal_wake.nudged_epoch.as_deref() == Some(activity_epoch.as_str())
                && state.goal_wake.nudged_sequence == Some(activity_sequence)
                && state
                    .goal_recovery_retry_at_ms()
                    .is_none_or(|at| now_ms < at)
            {
                continue;
            }
            if !renewing && pending_messages.is_empty() && goal_revision.is_none() {
                continue;
            }
            let batch_id = (!pending_messages.is_empty()).then(|| {
                unread_batch_id(
                    task.task.stable_key(),
                    task.owner_generation,
                    state.pairing.generation,
                    &unread.revision,
                    pending_messages.iter().map(|message| message.id.as_str()),
                )
            });
            let messages = pending_messages
                .iter()
                .map(|message| CompanionUnreadMessage {
                    id: message.id.clone(),
                    markdown: message.model_markdown(),
                })
                .collect::<Vec<_>>();
            let unread_message_ids = messages
                .iter()
                .map(|message| message.id.clone())
                .collect::<Vec<_>>();
            let (kind, prompt) = if state.pairing.resume_task
                && pending_messages.is_empty()
                && goal_revision.is_none()
            {
                (
                    AutomationActionKind::TurnRenewal,
                    ActionPrompt::TaskContinuation {},
                )
            } else if let Some(handoff) = state
                .turn_renewal
                .handoff
                .as_ref()
                .filter(|h| h.report_complete)
            {
                (
                    AutomationActionKind::TurnRenewal,
                    ActionPrompt::TurnRenewal {
                        handoff_id: handoff.id.clone(),
                    },
                )
            } else {
                match (batch_id, goal_revision) {
                    (Some(batch_id), Some(goal_revision)) => (
                        AutomationActionKind::OfflineCombined,
                        ActionPrompt::OfflineCombined {
                            batch_id,
                            messages,
                            goal_revision,
                        },
                    ),
                    (Some(batch_id), None) => (
                        AutomationActionKind::OfflineChat,
                        ActionPrompt::OfflineChat { batch_id, messages },
                    ),
                    (None, Some(goal_revision)) => (
                        AutomationActionKind::OfflineGoal,
                        ActionPrompt::OfflineGoal { goal_revision },
                    ),
                    (None, None) => continue,
                }
            };
            let goal_revision = if kind == AutomationActionKind::TurnRenewal {
                None
            } else {
                goal_revision
            };
            let mut queue = self.queue.lock().await;
            if let Some(position) = queue
                .iter()
                .position(|queued| queued.task_key == task.task.stable_key())
            {
                let can_coalesce = queued_context_matches(
                    queue.get(position).expect("queued action position"),
                    task.owner_generation,
                    state.pairing.generation,
                    &url,
                    &model,
                    &effort,
                    &catalog_revision,
                    &account_fingerprint,
                    goal_revision,
                );
                if can_coalesce {
                    let queued = queue.get_mut(position).expect("queued action position");
                    let queued_action = task.store.queue_action(
                        &task.task,
                        &queued.request.action_id,
                        kind,
                        state.pairing.generation,
                        goal_revision,
                        now_ms,
                    )?;
                    queued.activity_epoch = activity_epoch.clone();
                    queued.activity_sequence = activity_sequence;
                    queued.tool_call_count_at_start = activity.tool_call_count;
                    queued.unread_message_ids = unread_message_ids;
                    queued.transcript_revision = unread.revision;
                    queued.request.prompt = prompt;
                    queued.request.bypass_conversation_cooldown =
                        queued_action.bypass_conversation_cooldown;
                    queue
                        .make_contiguous()
                        .sort_by_key(|execution| action_priority(execution_kind(execution)));
                    continue;
                }
                queue.remove(position);
            }
            if queue.len() >= self.config.max_queued_actions {
                let action_id = random_id("action")?;
                task.store.record_queue_failure(
                    &task.task,
                    QueueFailure {
                        action_id: &action_id,
                        kind,
                        pairing_generation: state.pairing.generation,
                        goal_revision,
                        message: "Browser automation queue capacity is exhausted; pending chat and goal work were preserved",
                        now_ms,
                    },
                )?;
                continue;
            }
            let action_id = random_id("action")?;
            let idempotency_key = random_id("execute")?;
            let queued_action = task.store.queue_action(
                &task.task,
                &action_id,
                kind,
                state.pairing.generation,
                goal_revision,
                now_ms,
            )?;
            queue.push_back(AutomationExecution {
                task_key: task.task.stable_key().to_string(),
                owner_generation: task.owner_generation,
                account_fingerprint: account_fingerprint.clone(),
                catalog_revision,
                activity_epoch,
                activity_sequence,
                tool_call_count_at_start: activity.tool_call_count,
                unread_message_ids,
                transcript_revision: unread.revision,
                pairing_commit: None,
                request: ExecuteActionRequest {
                    action_id,
                    idempotency_key,
                    url: url.clone(),
                    pairing_generation: state.pairing.generation,
                    connector_name: connector_name.clone(),
                    model,
                    effort,
                    expected_account_fingerprint: account_fingerprint,
                    prompt,
                    expected_url: url,
                    auto_approve_tool_calls: self.config.auto_approve_tool_calls,
                    bypass_conversation_cooldown: queued_action.bypass_conversation_cooldown,
                    retain_pairing_window: false,
                    pairing_continuation_of: None,
                    agent_ticket: None,
                },
            });
            queue
                .make_contiguous()
                .sort_by_key(|execution| action_priority(execution_kind(execution)));
        }
        Ok(())
    }

    pub async fn run_next(&self, now_ms: u64) -> Result<Option<CompanionActionSnapshot>, String> {
        let _run_guard = self.run_lock.lock().await;
        let mut reservation = None;
        let outcome = self.run_next_locked(now_ms, None, &mut reservation).await;
        if let Some(reservation) = reservation {
            if outcome.as_ref().is_ok_and(|snapshot| {
                snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot.action_id == reservation.action_id
                        && snapshot.send_accepted
                        && snapshot.state == CompanionActionState::Idle
                        && snapshot.attention.is_none()
                })
            }) && let Err(error) = self.finish_pairing_without_yielding(&reservation).await
            {
                tracing::warn!(%error, "pairing continuation could not be dispatched");
            }
            if let Err(error) = self
                .executor
                .release_pairing_window(&reservation.action_id)
                .await
            {
                tracing::warn!(%error, "pairing browser reservation could not be released");
            }
        }
        outcome
    }

    async fn finish_pairing_without_yielding(
        &self,
        reservation: &PairingWindow,
    ) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + self.pairing_wait_timeout;
        while tokio::time::Instant::now() < deadline {
            let state = reservation.task.store.load(&reservation.task.task)?;
            let activity = reservation.task.chat.offline_activity().await?;
            let browser_stopped =
                super::BrowserCompanionControl::new(&self.config).is_ok_and(|control| {
                    let lifecycle = control.public_status();
                    lifecycle["manuallyStopped"] == true
                        || lifecycle["waitingForServerRestart"] == true
                });
            if state.pairing.generation != reservation.generation
                || activity.agent_stop_requested
                || browser_stopped
                || state.action.status == AutomationActionStatus::Cancelled
                || !self.schema_refresh_ready()
            {
                break;
            }
            if state.pairing.verified_owner_generation == Some(reservation.owner_generation) {
                self.scan_once(crate::markdown_chat::now_ms()).await?;
                let follow_up = self.queue.lock().await.iter().any(|queued| {
                    queued.task_key == reservation.task.task.stable_key()
                        && queued.owner_generation == reservation.owner_generation
                        && queued.request.pairing_generation == reservation.generation
                        && execution_kind(queued) != AutomationActionKind::PairVerify
                });
                if follow_up {
                    let mut unused = None;
                    self.run_next_locked(
                        crate::markdown_chat::now_ms(),
                        Some(reservation),
                        &mut unused,
                    )
                    .await?;
                    break;
                }
                let unread = reservation.task.chat.unread_snapshot().await?;
                if unread.messages.is_empty()
                    && !reservation.task.chat.has_wakeable_queued_messages().await?
                    && !state.pairing.resume_task
                    && !state
                        .goal
                        .as_ref()
                        .is_some_and(|goal| goal.status == GoalStatus::Active)
                {
                    break;
                }
            }
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                _ = tokio::time::sleep(Duration::from_millis(250)) => {},
                _ = self.wake.notified() => {},
            }
        }
        Ok(())
    }

    async fn run_next_locked(
        &self,
        now_ms: u64,
        preferred: Option<&PairingWindow>,
        reservation: &mut Option<PairingWindow>,
    ) -> Result<Option<CompanionActionSnapshot>, String> {
        let maximum_candidates = self.queue.lock().await.len();
        let mut considered = 0;
        let (mut execution, task) = loop {
            if considered >= maximum_candidates {
                return Ok(None);
            }
            considered += 1;
            let next = {
                let mut queue = self.queue.lock().await;
                if let Some(preferred) = preferred {
                    let position = queue.iter().position(|queued| {
                        queued.task_key == preferred.task.task.stable_key()
                            && queued.owner_generation == preferred.owner_generation
                            && queued.request.pairing_generation == preferred.generation
                            && execution_kind(queued) != AutomationActionKind::PairVerify
                    });
                    position.and_then(|position| queue.remove(position))
                } else {
                    queue.pop_front()
                }
            };
            let Some(mut execution) = next else {
                return Ok(None);
            };
            if let Some(preferred) = preferred {
                execution.request.pairing_continuation_of = Some(preferred.action_id.clone());
            }
            let Some(task) = self.tasks.read().await.get(&execution.task_key).cloned() else {
                continue;
            };
            // Recheck at execution time: settings may have changed while this
            // send/retry/renewal was waiting behind another browser action.
            if let Some(version) = self.current_schema_version() {
                let state = task.store.load(&task.task)?;
                let creation = matches!(execution.request.prompt, ActionPrompt::CreateChat { .. });
                if state.action.id.as_deref() != Some(&execution.request.action_id)
                    || state.pairing.generation != execution.request.pairing_generation
                {
                    continue;
                }
                let stale = execution_kind(&execution) != AutomationActionKind::PairVerify
                    && state.pairing.connector_schema_version.as_deref() != Some(&version)
                    && !task.chat.offline_activity().await?.agent_stop_requested;
                if creation || stale {
                    if stale {
                        task.store
                            .hold_messages_for_schema_rollover(&task.task, &state.pairing)?;
                    }
                    if !self.schema_refresh_ready() {
                        self.queue.lock().await.push_back(execution);
                        continue;
                    }
                    if stale {
                        let expected = task.store.load(&task.task)?;
                        if !self
                            .queue_replacement_conversation(&task, &expected, true, now_ms)
                            .await?
                        {
                            self.queue.lock().await.push_back(execution);
                        }
                        continue;
                    }
                    if !task.store.record_creation_schema(
                        &task.task,
                        &execution.request.action_id,
                        execution.request.pairing_generation,
                        &version,
                    )? {
                        continue;
                    }
                }
            }
            let cooldown = task.store.load(&task.task)?.browser_cooldown_until_ms();
            if let Some(until) = cooldown.filter(|until| now_ms < *until) {
                self.next_retry_scan_ms.fetch_min(until, Ordering::Relaxed);
                self.queue.lock().await.push_back(execution);
                continue;
            }
            if task
                .store
                .restore_disabled_queued_retry(&task.task, &execution.request.action_id)?
            {
                self.clear_live_if_action(&execution).await;
                continue;
            }
            if let Some(batch_id) = execution_batch_id(&execution) {
                if !task.store.record_chat_submission(
                    &task.task,
                    &execution.request.action_id,
                    execution.request.pairing_generation,
                    AcceptedChatBatch {
                        owner_generation: execution.owner_generation,
                        batch_id,
                        message_ids: &execution.unread_message_ids,
                        transcript_revision: &execution.transcript_revision,
                    },
                )? {
                    continue;
                }
                let state = task.store.load(&task.task)?;
                if !task.chat.offline_activity().await?.agent_stop_requested
                    && self.reconcile_chat_delivery(&task, &state, now_ms).await?
                {
                    continue;
                }
            }
            if execution.activity_epoch == "conversation-rollover"
                && task.chat.offline_activity().await?.agent_stop_requested
            {
                task.store.fail_creation_before_send(
                    &task.task,
                    execution.request.pairing_generation,
                    "Replacement conversation creation was stopped before Send",
                    now_ms,
                )?;
                continue;
            }
            if !execution_matches(&task, &execution, now_ms).await? {
                if execution_kind(&execution) == AutomationActionKind::OfflineGoal {
                    let state = task.store.load(&task.task)?;
                    let activity = task.chat.offline_activity().await?;
                    if state.action.id.as_deref() == Some(execution.request.action_id.as_str())
                        && !activity.agent_stop_requested
                        && activity.is_online(now_ms, task.store.offline_after_ms())
                        && task.store.reconcile_resumed_goal_retry(
                            &task.task,
                            &state,
                            task.owner_generation,
                            activity.last_call_at_ms,
                            now_ms,
                        )?
                    {
                        self.clear_live_if_action(&execution).await;
                        task.store
                            .notify_state(&task.chat, &task.store.load(&task.task)?)
                            .await;
                        continue;
                    }
                }
                let _ = task.store.update_action_status(
                    &task.task,
                    &execution.request.action_id,
                    AutomationActionStatus::Idle,
                    None,
                    None,
                    now_ms,
                );
                continue;
            }
            let Some(connector_name) = task.store.connector_name(&self.config.connector_name)?
            else {
                let error =
                    "Configure the exact ChatGPT connector name before using browser automation";
                task.store.update_action_status(
                    &task.task,
                    &execution.request.action_id,
                    AutomationActionStatus::Failed,
                    Some(error),
                    None,
                    now_ms,
                )?;
                return Err(error.into());
            };
            execution.request.connector_name = connector_name;
            if cooldown.is_some() {
                task.store.resume_cooldown_action(
                    &task.task,
                    &execution.request.action_id,
                    now_ms,
                )?;
            }
            break (execution, task);
        };

        if let Some(context) = self.rollover.get()
            && context.config.experimental.agent_tickets
            && execution.request.agent_ticket.is_none()
        {
            let issued = (|| {
                let creating = matches!(execution.request.prompt, ActionPrompt::CreateChat { .. });
                let conversation_key = if creating {
                    None
                } else {
                    Some(
                        context
                            .continuations
                            .physical_owner_for_task(&task.task, execution.owner_generation)?,
                    )
                };
                context
                    .tickets
                    .issue_browser_ticket(crate::agent_tickets::BrowserTicketScope {
                        task_key: task.task.stable_key().into(),
                        owner_generation: execution.owner_generation,
                        conversation_key,
                    })
            })();
            match issued {
                Ok(ticket) => execution.request.agent_ticket = Some(ticket),
                Err(error) => {
                    task.store.update_action_status(
                        &task.task,
                        &execution.request.action_id,
                        AutomationActionStatus::Failed,
                        Some(&error),
                        None,
                        now_ms,
                    )?;
                    self.notify();
                    return Err(error);
                }
            }
        }
        let pair_verification = execution_kind(&execution) == AutomationActionKind::PairVerify;
        if execution.request.retain_pairing_window {
            *reservation = Some(PairingWindow {
                task: task.clone(),
                action_id: execution.request.action_id.clone(),
                generation: execution.request.pairing_generation,
                owner_generation: execution.owner_generation,
            });
        }
        let checkpoint = if pair_verification {
            None
        } else {
            Some(task.chat.transcript_checkpoint().await?)
        };
        task.store.update_action_status(
            &task.task,
            &execution.request.action_id,
            AutomationActionStatus::Navigating,
            None,
            None,
            now_ms,
        )?;
        self.live.write().await.insert(
            execution.task_key.clone(),
            AutomationLiveState {
                action_id: Some(execution.request.action_id.clone()),
                phase: AutomationActionStatus::Navigating,
                updated_at_ms: now_ms,
                ..Default::default()
            },
        );
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let execute = self
            .executor
            .execute(execution.clone(), events_tx, cancellation.clone());
        tokio::pin!(execute);
        let mut accumulator = EventAccumulator::default();
        let mut events_open = true;
        let started = tokio::time::Instant::now();
        let start_deadline = started + self.action_timeouts.start;
        let total_deadline = started + self.action_timeouts.total;
        let mut progress_deadline = started + self.action_timeouts.progress;
        let mut watchdog_message = None;
        let outcome = loop {
            let now = tokio::time::Instant::now();
            if now >= total_deadline || (!accumulator.send_accepted && now >= start_deadline) {
                let message = if now >= total_deadline {
                    "Browser action reached its 32-minute time limit"
                } else {
                    "Browser action timed out before Send after 2 minutes"
                };
                watchdog_message = Some(message);
                break Err(execution_error(message.into(), true));
            }
            let deadline = total_deadline.min(if accumulator.send_accepted {
                progress_deadline
            } else {
                start_deadline
            });
            tokio::select! {
                biased;
                result = &mut execute => break result,
                event = events_rx.recv(), if events_open => match event {
                    Some(event) => {
                        self.apply_event(&execution, &mut accumulator, event).await?;
                        progress_deadline = tokio::time::Instant::now() + self.action_timeouts.progress;
                    }
                    None => events_open = false,
                },
                _ = tokio::time::sleep_until(deadline) => {
                    let message = if tokio::time::Instant::now() >= total_deadline {
                        "Browser action reached its 32-minute time limit"
                    } else if accumulator.send_accepted {
                        "Browser action timed out after 3 minutes without checked progress"
                    } else {
                        "Browser action timed out before Send after 2 minutes"
                    };
                    watchdog_message = Some(message);
                    break Err(execution_error(message.into(), true));
                },
            }
        };
        let outcome = if let Some(message) = watchdog_message {
            cancellation.cancel();
            tokio::time::timeout(
                Duration::from_secs(10),
                self.executor.timed_out_action(&execution, message),
            )
            .await
            .unwrap_or_else(|_| {
                Err(execution_error(
                    format!("{message}; browser status reconciliation timed out"),
                    true,
                ))
            })
        } else {
            outcome
        };
        while let Ok(event) = events_rx.try_recv() {
            self.apply_event(&execution, &mut accumulator, event)
                .await?;
        }

        let outcome = outcome.and_then(|snapshot| {
            if pair_verification
                && !snapshot.send_accepted
                && snapshot.state != CompanionActionState::Waiting
            {
                return Err(execution_error(
                    snapshot
                        .attention
                        .as_ref()
                        .and_then(|attention| attention.message.clone())
                        .unwrap_or_else(|| "Pairing verification failed before Send".into()),
                    false,
                ));
            }
            Ok(snapshot)
        });

        let snapshot = match outcome {
            Ok(snapshot) => {
                if snapshot.state == CompanionActionState::Waiting {
                    let until = snapshot
                        .cooldown_until_ms
                        .ok_or("Browser cooldown deadline is missing")?;
                    if snapshot.send_accepted || accumulator.send_accepted {
                        return Err("A submitted browser action cannot be deferred".into());
                    }
                    let next_id = random_id("action")?;
                    if task.store.defer_action_for_cooldown(
                        &task.task,
                        &execution.request.action_id,
                        &next_id,
                        until,
                    )? {
                        let mut queued = execution.clone();
                        queued.request.action_id = next_id;
                        queued.request.idempotency_key = random_id("execute")?;
                        if let Some(batch_id) = execution_batch_id(&queued) {
                            task.store.record_chat_submission(
                                &task.task,
                                &queued.request.action_id,
                                queued.request.pairing_generation,
                                AcceptedChatBatch {
                                    owner_generation: queued.owner_generation,
                                    batch_id,
                                    message_ids: &queued.unread_message_ids,
                                    transcript_revision: &queued.transcript_revision,
                                },
                            )?;
                        }
                        self.queue.lock().await.push_back(queued);
                        self.next_retry_scan_ms.fetch_min(until, Ordering::Relaxed);
                        self.notify();
                    }
                    self.clear_live_if_action(&execution).await;
                    return Ok(Some(snapshot));
                }
                if snapshot.send_accepted
                    && matches!(execution.request.prompt, ActionPrompt::CreateChat { .. })
                {
                    task.store.record_creation_send(
                        &task.task,
                        &execution.request.action_id,
                        true,
                    )?;
                }
                if snapshot.send_accepted && !pair_verification {
                    task.store
                        .record_goal_turn(&task.task, &execution.request.action_id)?;
                }
                if pair_verification && snapshot.send_accepted {
                    task.store.record_pairing_submission(
                        &task.task,
                        &execution.request.action_id,
                        now_ms,
                    )?;
                }
                if snapshot.send_accepted
                    && snapshot.state == CompanionActionState::Idle
                    && snapshot.attention.is_none()
                    && !pair_verification
                {
                    task.store.record_browser_turn_send(
                        &task.task,
                        super::turn_renewal::TurnSend {
                            action_id: execution.request.action_id.clone(),
                            at_ms: crate::markdown_chat::now_ms(),
                            model: snapshot.selected.as_ref().map(|s| s.model.clone()),
                            effort: snapshot.selected.as_ref().map(|s| s.effort.clone()),
                            pairing_generation: execution.request.pairing_generation,
                            owner_generation: execution.owner_generation,
                        },
                        &execution,
                    )?;
                }
                accumulator.send_accepted |= snapshot.send_accepted;
                snapshot
            }
            Err(error) => {
                let action_current = current_action_matches(&task, &execution)?;
                if action_current && !pair_verification {
                    let state = task.store.load(&task.task)?;
                    if !task.chat.offline_activity().await?.agent_stop_requested
                        && self.reconcile_chat_delivery(&task, &state, now_ms).await?
                    {
                        return Ok(None);
                    }
                }
                let pairing = task.store.load(&task.task)?.pairing;
                let pairing_verified = pair_verification
                    && pairing.generation == execution.request.pairing_generation
                    && pairing.verified_owner_generation == Some(execution.owner_generation);
                let send_accepted =
                    error.send_accepted || accumulator.send_accepted || pairing_verified;
                if pair_verification && send_accepted {
                    task.store.record_pairing_submission(
                        &task.task,
                        &execution.request.action_id,
                        now_ms,
                    )?;
                }
                if matches!(execution.request.prompt, ActionPrompt::CreateChat { .. }) {
                    task.store.record_creation_send(
                        &task.task,
                        &execution.request.action_id,
                        send_accepted,
                    )?;
                }
                if pair_verification
                    && action_current
                    && !send_accepted
                    && !matches!(execution.request.prompt, ActionPrompt::CreateChat { .. })
                {
                    task.store.invalidate_pairing(
                        &task.task,
                        execution.request.pairing_generation,
                        now_ms,
                    )?;
                }
                if send_accepted {
                    if let Some(batch_id) = execution_batch_id(&execution) {
                        if action_current {
                            task.store.record_accepted_batch(
                                &task.task,
                                &execution.request.action_id,
                                AcceptedChatBatch {
                                    owner_generation: execution.owner_generation,
                                    batch_id,
                                    message_ids: &execution.unread_message_ids,
                                    transcript_revision: &execution.transcript_revision,
                                },
                                now_ms,
                            )?;
                        } else {
                            task.store.record_superseded_accepted_batch(
                                &task.task,
                                &execution.request.action_id,
                                AcceptedChatBatch {
                                    owner_generation: execution.owner_generation,
                                    batch_id,
                                    message_ids: &execution.unread_message_ids,
                                    transcript_revision: &execution.transcript_revision,
                                },
                            )?;
                        }
                    }
                    if action_current && execution_goal_revision(&execution).is_some() {
                        task.store.record_goal_nudge(
                            &task.task,
                            &execution.request.action_id,
                            &execution.activity_epoch,
                            execution.activity_sequence,
                            now_ms.saturating_add(
                                started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                            ),
                        )?;
                        let _ = task
                            .chat
                            .claim_offline_automation_goal(now_ms, task.store.offline_after_ms())
                            .await?;
                    }
                }
                if !action_current {
                    self.clear_live_if_action(&execution).await;
                    return Err(error.message);
                }
                let status =
                    if error.failure_status == Some(AutomationActionStatus::ConversationLimit) {
                        AutomationActionStatus::ConversationLimit
                    } else if send_accepted {
                        AutomationActionStatus::Ambiguous
                    } else {
                        error
                            .failure_status
                            .unwrap_or(AutomationActionStatus::Failed)
                    };
                let terminal_at_ms = if !send_accepted {
                    now_ms
                        .saturating_add(started.elapsed().as_millis().min(u64::MAX as u128) as u64)
                } else {
                    now_ms
                };
                task.store.update_action_status(
                    &task.task,
                    &execution.request.action_id,
                    status,
                    Some(&error.message),
                    None,
                    terminal_at_ms,
                )?;
                if status == AutomationActionStatus::ConversationLimit
                    && self
                        .queue_conversation_rollover(
                            &task,
                            &execution.request.action_id,
                            terminal_at_ms,
                        )
                        .await?
                {
                    return Ok(None);
                }
                self.update_live_terminal(
                    &execution,
                    status,
                    Some(error.message.clone()),
                    terminal_at_ms,
                )
                .await;
                if let Ok(state) = task.store.load(&task.task) {
                    task.store.notify_state(&task.chat, &state).await;
                }
                return Err(error.message);
            }
        };

        if !current_action_matches(&task, &execution)? {
            if accumulator.send_accepted
                && let Some(batch_id) = execution_batch_id(&execution)
            {
                task.store.record_superseded_accepted_batch(
                    &task.task,
                    &execution.request.action_id,
                    AcceptedChatBatch {
                        owner_generation: execution.owner_generation,
                        batch_id,
                        message_ids: &execution.unread_message_ids,
                        transcript_revision: &execution.transcript_revision,
                    },
                )?;
            }
            self.clear_live_if_action(&execution).await;
            return Ok(Some(snapshot));
        }

        if let Some(commit) = &execution.pairing_commit {
            let pairing = commit.store.load(&commit.task)?.pairing;
            if pairing.pending.is_some() {
                let canonical_url = snapshot
                    .canonical_url
                    .as_deref()
                    .ok_or("Browser companion did not return the canonical pairing URL")?;
                commit.store.record_pairing_canonical_url(
                    &commit.task,
                    commit.generation,
                    canonical_url,
                    now_ms,
                )?;
            }
        }

        if accumulator.send_accepted
            && let Some(batch_id) = execution_batch_id(&execution)
        {
            task.store.record_accepted_batch(
                &task.task,
                &execution.request.action_id,
                AcceptedChatBatch {
                    owner_generation: execution.owner_generation,
                    batch_id,
                    message_ids: &execution.unread_message_ids,
                    transcript_revision: &execution.transcript_revision,
                },
                now_ms,
            )?;
        }
        if accumulator.send_accepted && execution_goal_revision(&execution).is_some() {
            task.store.record_goal_nudge(
                &task.task,
                &execution.request.action_id,
                &execution.activity_epoch,
                execution.activity_sequence,
                now_ms.saturating_add(started.elapsed().as_millis().min(u64::MAX as u128) as u64),
            )?;
            let _ = task
                .chat
                .claim_offline_automation_goal(now_ms, task.store.offline_after_ms())
                .await?;
        }
        if !pair_verification
            && let Some(answer) = accumulator
                .final_markdown
                .as_deref()
                .filter(|answer| !answer.trim().is_empty())
            && !task
                .chat
                .has_agent_message_after(checkpoint.as_ref().expect("non-pair checkpoint"))
                .await?
        {
            task.chat
                .append_browser_answer(&execution.request.action_id, answer)
                .await?;
        }

        let activity_after = task.chat.offline_activity().await?;
        let no_tool_activity = activity_after.tool_call_count == execution.tool_call_count_at_start;
        let terminal_status = accumulator.terminal_status.unwrap_or_else(|| {
            if snapshot.state == CompanionActionState::Failed {
                failed_companion_status(
                    snapshot
                        .attention
                        .as_ref()
                        .map(|a| a.code.as_str())
                        .unwrap_or_default(),
                )
            } else {
                companion_status(snapshot.state)
            }
        });
        let status = if pair_verification {
            let pairing = task.store.load(&task.task)?.pairing;
            if pairing.verified_owner_generation == Some(execution.owner_generation) {
                AutomationActionStatus::Idle
            } else {
                AutomationActionStatus::Unverified
            }
        } else if accumulator.send_accepted
            && matches!(
                terminal_status,
                AutomationActionStatus::Failed | AutomationActionStatus::ConversationUnavailable
            )
        {
            AutomationActionStatus::Ambiguous
        } else if accumulator.send_accepted
            && !accumulator.submission_complete
            && execution_goal_revision(&execution).is_some()
            && no_tool_activity
            && terminal_status == AutomationActionStatus::Idle
        {
            AutomationActionStatus::Stalled
        } else {
            terminal_status
        };
        let failure_message = snapshot.attention.as_ref().and_then(|attention| attention.message.as_deref())
            .or(watchdog_message).or(match status {
                AutomationActionStatus::AuthenticationRequired => Some("Sign in to ChatGPT, then retry this action."),
                AutomationActionStatus::ConversationUnavailable => Some("ChatGPT conversation temporarily not accessible"),
                AutomationActionStatus::Stalled => Some("The previous goal turn produced no tool activity. Continue the goal to start another turn."),
                _ => None,
            });
        let terminal_at_ms = if !accumulator.send_accepted {
            now_ms.saturating_add(started.elapsed().as_millis().min(u64::MAX as u128) as u64)
        } else {
            now_ms
        };
        if !pair_verification && status != AutomationActionStatus::Idle {
            let state = task.store.load(&task.task)?;
            if !activity_after.agent_stop_requested
                && self
                    .reconcile_chat_delivery(&task, &state, terminal_at_ms)
                    .await?
            {
                return Ok(Some(snapshot));
            }
        }
        task.store.update_action_status(
            &task.task,
            &execution.request.action_id,
            status,
            failure_message,
            (!pair_verification)
                .then_some(accumulator.final_markdown.as_deref())
                .flatten()
                .filter(|answer| answer.len() <= MAX_PRIVATE_TEXT_BYTES),
            terminal_at_ms,
        )?;
        if status == AutomationActionStatus::ConversationLimit
            && self
                .queue_conversation_rollover(&task, &execution.request.action_id, terminal_at_ms)
                .await?
        {
            return Ok(Some(snapshot));
        }
        if status == AutomationActionStatus::ApprovalRequired && self.config.auto_approve_tool_calls
        {
            self.approval_next_scan_ms.store(0, Ordering::Relaxed);
            // The action has opened its target and found an approval. Resolve it
            // before any new continuation, including when the passive scan had
            // been looking at a different conversation.
            let pending = task.store.load(&task.task)?;
            if pending.action.id.as_deref() == Some(execution.request.action_id.as_str())
                && pending.pairing.verified_owner_generation == Some(task.owner_generation)
                && let Some(connector) = task.store.connector_name(&self.config.connector_name)?
                && let Ok(true) = self
                    .executor
                    .approve_pending_conversation(
                        &execution.request.expected_url,
                        &execution.account_fingerprint,
                        &connector,
                    )
                    .await
                && self
                    .record_approved_resume(
                        &task,
                        &pending,
                        &execution.activity_epoch,
                        execution.activity_sequence,
                        terminal_at_ms,
                    )
                    .await?
            {
                return Ok(Some(snapshot));
            }
            self.notify();
        }
        self.update_live_terminal(
            &execution,
            status,
            failure_message.map(str::to_owned),
            terminal_at_ms,
        )
        .await;
        if let Ok(state) = task.store.load(&task.task) {
            task.store.notify_state(&task.chat, &state).await;
        }
        Ok(Some(snapshot))
    }

    async fn has_ready_action(&self, now_ms: u64) -> bool {
        if !self.schema_refresh_ready() {
            return false;
        }
        let tasks = self.tasks.read().await;
        let queue = self.queue.lock().await;
        queue.iter().any(|execution| {
            tasks
                .get(&execution.task_key)
                .and_then(|task| task.store.load(&task.task).ok())
                .is_none_or(|state| {
                    state
                        .browser_cooldown_until_ms()
                        .is_none_or(|until| now_ms >= until)
                })
        })
    }

    async fn record_approved_resume(
        &self,
        task: &AutomationTaskHandle,
        expected: &super::AutomationTaskState,
        activity_epoch: &str,
        activity_sequence: u64,
        now_ms: u64,
    ) -> Result<bool, String> {
        if !task.store.record_approval_resume(
            &task.task,
            expected,
            task.owner_generation,
            activity_epoch,
            activity_sequence,
            now_ms,
        )? {
            return Ok(false);
        }
        if let Some(id) = expected.action.id.as_deref() {
            self.queue.lock().await.retain(|queued| {
                queued.task_key != task.task.stable_key() || queued.request.action_id != id
            });
            self.clear_live_action(task.task.stable_key(), id).await;
        }
        let state = task.store.load(&task.task)?;
        task.store.notify_state(&task.chat, &state).await;
        Ok(true)
    }

    async fn reconcile_chat_delivery(
        &self,
        task: &AutomationTaskHandle,
        state: &super::AutomationTaskState,
        now_ms: u64,
    ) -> Result<bool, String> {
        let Some(submission) =
            task.store
                .chat_delivery_candidate(&task.task, state, task.owner_generation)?
        else {
            return Ok(false);
        };
        if !task
            .chat
            .user_messages_read(&submission.message_ids)
            .await?
        {
            return Ok(false);
        }
        if !task
            .store
            .reconcile_read_chat_action(&task.task, state, &submission, now_ms)?
        {
            return Ok(false);
        }
        if let Some(action_id) = state.action.id.as_deref() {
            self.clear_live_action(task.task.stable_key(), action_id)
                .await;
        }
        Ok(true)
    }

    async fn apply_event(
        &self,
        execution: &AutomationExecution,
        accumulator: &mut EventAccumulator,
        event: CompanionEvent,
    ) -> Result<(), String> {
        if let CompanionEventKind::ModelVerified {
            ref model,
            ref effort,
        } = event.kind
        {
            accumulator.selected = Some(super::CompanionSelection {
                model: model.clone(),
                effort: effort.clone(),
            });
        }
        if matches!(event.kind, CompanionEventKind::SendAccepted)
            && let Some(task) = self.tasks.read().await.get(&execution.task_key)
        {
            task.store.record_browser_turn_send(
                &task.task,
                super::turn_renewal::TurnSend {
                    action_id: execution.request.action_id.clone(),
                    at_ms: event.at_ms,
                    model: accumulator.selected.as_ref().map(|s| s.model.clone()),
                    effort: accumulator.selected.as_ref().map(|s| s.effort.clone()),
                    pairing_generation: execution.request.pairing_generation,
                    owner_generation: execution.owner_generation,
                },
                execution,
            )?;
        }
        if matches!(event.kind, CompanionEventKind::SendAccepted)
            && let Some(task) = self.tasks.read().await.get(&execution.task_key)
        {
            if execution_kind(execution) == AutomationActionKind::PairVerify {
                task.store.record_pairing_submission(
                    &task.task,
                    &execution.request.action_id,
                    event.at_ms,
                )?;
            } else {
                task.store
                    .record_goal_turn(&task.task, &execution.request.action_id)?;
            }
        }
        if let CompanionEventKind::ConversationCreated { ref canonical_url } = event.kind
            && let Some(commit) = &execution.pairing_commit
            && let Some(task) = self.tasks.read().await.get(&execution.task_key)
            && current_action_matches(task, execution)?
        {
            commit.store.record_pairing_canonical_url(
                &commit.task,
                commit.generation,
                canonical_url,
                event.at_ms,
            )?;
        }
        let mut live = self.live.write().await;
        let state = live
            .entry(execution.task_key.clone())
            .or_insert_with(AutomationLiveState::default);
        state.action_id = Some(execution.request.action_id.clone());
        state.updated_at_ms = event.at_ms;
        match event.kind {
            CompanionEventKind::Queued => state.phase = AutomationActionStatus::Queued,
            CompanionEventKind::Waiting { .. } => {
                state.phase = AutomationActionStatus::Queued;
                state.status = None;
            }
            CompanionEventKind::Navigating
            | CompanionEventKind::ComposerReady
            | CompanionEventKind::ModelVerified { .. }
            | CompanionEventKind::ConnectorVerified => {
                state.phase = AutomationActionStatus::Navigating;
            }
            CompanionEventKind::ConversationCreated { .. } => {
                state.phase = AutomationActionStatus::Sending;
            }
            CompanionEventKind::SendAccepted => {
                accumulator.send_accepted = true;
                state.phase = AutomationActionStatus::Sending;
            }
            CompanionEventKind::StatusDelta { text, continuation } => {
                apply_live_delta(&mut state.status, &text, continuation);
                state.phase = AutomationActionStatus::Generating;
            }
            CompanionEventKind::CommentaryDelta {
                markdown,
                continuation,
            } => {
                apply_live_delta(&mut state.commentary, &markdown, continuation);
                state.phase = AutomationActionStatus::Generating;
            }
            CompanionEventKind::AnswerDelta {
                markdown,
                continuation,
            } => {
                apply_live_delta(&mut state.answer, &markdown, continuation);
                accumulator.answer_truncated |=
                    apply_live_delta(&mut accumulator.answer_markdown, &markdown, continuation);
                state.phase = AutomationActionStatus::Generating;
            }
            CompanionEventKind::GenerationComplete { final_markdown, .. } => {
                let final_markdown = final_markdown.or_else(|| {
                    (!accumulator.answer_truncated)
                        .then(|| accumulator.answer_markdown.take())
                        .flatten()
                });
                if let Some(answer) = final_markdown.as_ref() {
                    state.answer = Some(answer.clone());
                }
                accumulator.final_markdown = final_markdown;
                accumulator.terminal_status = Some(AutomationActionStatus::Idle);
                state.phase = AutomationActionStatus::Idle;
            }
            CompanionEventKind::SubmissionComplete { .. } => {
                accumulator.submission_complete = true;
                accumulator.terminal_status = Some(AutomationActionStatus::Idle);
                state.phase = AutomationActionStatus::Idle;
                state.status = Some("Message sent to ChatGPT".into());
            }
            CompanionEventKind::ApprovalRequired { message } => {
                state.status = message;
                accumulator.terminal_status = Some(AutomationActionStatus::ApprovalRequired);
                state.phase = AutomationActionStatus::ApprovalRequired;
            }
            CompanionEventKind::AuthenticationRequired => {
                accumulator.terminal_status = Some(AutomationActionStatus::AuthenticationRequired);
                state.phase = AutomationActionStatus::AuthenticationRequired;
            }
            CompanionEventKind::RateLimited { message } => {
                state.status = message;
                accumulator.terminal_status = Some(AutomationActionStatus::RateLimited);
                state.phase = AutomationActionStatus::RateLimited;
            }
            CompanionEventKind::ExternalInterference { message } => {
                state.status = message;
                accumulator.terminal_status = Some(AutomationActionStatus::Interference);
                state.phase = AutomationActionStatus::Interference;
            }
            CompanionEventKind::AmbiguousSend { message } => {
                accumulator.send_accepted = true;
                state.status = message;
                accumulator.terminal_status = Some(AutomationActionStatus::Ambiguous);
                state.phase = AutomationActionStatus::Ambiguous;
            }
            CompanionEventKind::Failed { message, code } => {
                state.status = Some(message);
                let failure = failed_companion_status(&code);
                accumulator.terminal_status = Some(failure);
                state.phase = failure;
            }
        }
        Ok(())
    }

    async fn update_live_terminal(
        &self,
        execution: &AutomationExecution,
        phase: AutomationActionStatus,
        status: Option<String>,
        now_ms: u64,
    ) {
        let task = self.tasks.read().await.get(&execution.task_key).cloned();
        let mut live = self.live.write().await;
        // Read the durable receipt while holding the live-state lock so a
        // concurrent delivery reconciliation cannot resurrect a stale banner.
        if let Some(task) = task
            && let Ok(saved) = task.store.load(&task.task)
            && (saved.action.id.as_deref() != Some(execution.request.action_id.as_str())
                || phase != AutomationActionStatus::Idle
                    && saved.action.status == AutomationActionStatus::Idle
                    && saved.action.send_accepted == Some(true))
        {
            if live
                .get(&execution.task_key)
                .and_then(|state| state.action_id.as_deref())
                == Some(execution.request.action_id.as_str())
            {
                live.remove(&execution.task_key);
            }
            return;
        }
        let state = live
            .entry(execution.task_key.clone())
            .or_insert_with(AutomationLiveState::default);
        state.action_id = Some(execution.request.action_id.clone());
        state.phase = phase;
        if status.is_some() {
            state.status = status;
        }
        state.updated_at_ms = now_ms;
    }

    async fn clear_live_if_action(&self, execution: &AutomationExecution) {
        self.clear_live_action(&execution.task_key, &execution.request.action_id)
            .await;
    }

    async fn clear_live_action(&self, task_key: &str, action_id: &str) {
        let mut live = self.live.write().await;
        if live
            .get(task_key)
            .and_then(|state| state.action_id.as_deref())
            == Some(action_id)
        {
            live.remove(task_key);
        }
    }
}

#[derive(Default)]
struct EventAccumulator {
    submission_complete: bool,
    selected: Option<super::CompanionSelection>,
    send_accepted: bool,
    answer_markdown: Option<String>,
    answer_truncated: bool,
    final_markdown: Option<String>,
    terminal_status: Option<AutomationActionStatus>,
}

fn current_action_matches(
    task: &AutomationTaskHandle,
    execution: &AutomationExecution,
) -> Result<bool, String> {
    Ok(task.store.load(&task.task)?.action.id.as_deref()
        == Some(execution.request.action_id.as_str()))
}

const MAX_LIVE_TEXT_BYTES: usize = 1024 * 1024;

fn apply_live_delta(destination: &mut Option<String>, value: &str, continuation: bool) -> bool {
    if !continuation {
        let end = utf8_prefix_end(value, MAX_LIVE_TEXT_BYTES);
        *destination = Some(value[..end].to_string());
        return end < value.len();
    }
    let destination = destination.get_or_insert_with(String::new);
    let remaining = MAX_LIVE_TEXT_BYTES.saturating_sub(destination.len());
    let end = utf8_prefix_end(value, remaining);
    destination.push_str(&value[..end]);
    end < value.len()
}

fn utf8_prefix_end(value: &str, maximum: usize) -> usize {
    let mut end = value.len().min(maximum);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn execution_error(message: String, send_accepted: bool) -> AutomationExecutionError {
    AutomationExecutionError {
        message,
        send_accepted,
        failure_status: None,
    }
}

fn validate_terminal_snapshot(
    execution: &AutomationExecution,
    snapshot: &CompanionActionSnapshot,
) -> Result<(), AutomationExecutionError> {
    if snapshot.state == CompanionActionState::Waiting
        && (snapshot.send_accepted
            || snapshot
                .cooldown_until_ms
                .is_none_or(|at| at == 0 || at > 9_007_199_254_740_991))
    {
        return Err(execution_error(
            "Browser cooldown deferral is invalid".into(),
            true,
        ));
    }
    if snapshot.action_id != execution.request.action_id {
        return Err(execution_error(
            "Browser companion returned the wrong action".into(),
            true,
        ));
    }
    if !snapshot.send_accepted {
        if matches!(
            snapshot.state,
            CompanionActionState::Idle
                | CompanionActionState::ApprovalRequired
                | CompanionActionState::Ambiguous
                | CompanionActionState::Stalled
        ) {
            if snapshot.state == CompanionActionState::Ambiguous
                && let Some(message) = snapshot.attention.as_ref().and_then(|attention| {
                    attention
                        .message
                        .as_deref()
                        .filter(|message| !message.trim().is_empty())
                })
            {
                return Err(execution_error(message.to_owned(), true));
            }
            return Err(execution_error(
                "Browser companion reached a post-Send terminal state without proving Send acceptance"
                    .into(),
                true,
            ));
        }
        return Ok(());
    }
    if execution.request.model.is_some() {
        let selected = snapshot.selected.as_ref().ok_or_else(|| {
            execution_error(
                "Browser companion did not prove the selected model and effort".into(),
                true,
            )
        })?;
        if !super::model_fallback::selection_allowed(
            execution.request.model.as_deref(),
            execution.request.effort.as_deref(),
            selected,
        ) {
            return Err(execution_error(
                "Browser companion selected a different model or effort".into(),
                true,
            ));
        }
    }
    if !snapshot.canonical_url.as_deref().is_some_and(|observed| {
        if matches!(execution.request.prompt, ActionPrompt::CreateChat { .. }) {
            super::companion::canonical_chatgpt_conversation_url(observed)
                .is_ok_and(|url| url.starts_with("https://chatgpt.com/c/"))
        } else {
            same_chatgpt_conversation_url(observed, &execution.request.expected_url)
        }
    }) {
        return Err(execution_error(
            "Browser companion returned a different conversation URL".into(),
            true,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod model_fallback_snapshot_tests {
    use super::*;

    fn execution() -> AutomationExecution {
        AutomationExecution {
            task_key: "fallback".into(),
            owner_generation: 0,
            account_fingerprint: "account".into(),
            catalog_revision: "catalog".into(),
            activity_epoch: "epoch".into(),
            activity_sequence: 1,
            tool_call_count_at_start: 0,
            unread_message_ids: Vec::new(),
            transcript_revision: "revision".into(),
            pairing_commit: None,
            request: ExecuteActionRequest {
                action_id: "fallback".into(),
                idempotency_key: "fallback-key".into(),
                url: "https://chatgpt.com/c/fallback".into(),
                expected_url: "https://chatgpt.com/c/fallback".into(),
                pairing_generation: 1,
                connector_name: "Fixture".into(),
                expected_account_fingerprint: "account".into(),
                model: Some("gpt-6-astra".into()),
                effort: Some("max".into()),
                prompt: ActionPrompt::OfflineGoal { goal_revision: 1 },
                auto_approve_tool_calls: false,
                bypass_conversation_cooldown: false,
                retain_pairing_window: false,
                pairing_continuation_of: None,
                agent_ticket: None,
            },
        }
    }

    #[test]
    fn terminal_proof_accepts_ranked_fallbacks_but_keeps_identity_and_selection_guards() {
        let execution = execution();
        let mut snapshot: CompanionActionSnapshot = serde_json::from_value(serde_json::json!({
            "protocolVersion":1,"build":"test","actionId":"fallback","state":"idle","sendAccepted":true,
            "canonicalUrl":"https://chatgpt.com/c/fallback","selected":{"model":"gpt-5.6-sol","effort":"max"},
            "lastEventSequence":3,"attention":null
        })).unwrap();
        assert!(validate_terminal_snapshot(&execution, &snapshot).is_ok());
        assert_eq!(execution.request.model.as_deref(), Some("gpt-6-astra"));
        snapshot.selected = None;
        assert!(validate_terminal_snapshot(&execution, &snapshot).is_err());
        snapshot.selected = Some(super::super::CompanionSelection {
            model: "unexpected-model".into(),
            effort: "max".into(),
        });
        assert!(validate_terminal_snapshot(&execution, &snapshot).is_err());
        snapshot.selected = Some(super::super::CompanionSelection {
            model: "gpt-6-sol".into(),
            effort: "high".into(),
        });
        snapshot.canonical_url = Some("https://chatgpt.com/c/another-chat".into());
        assert!(validate_terminal_snapshot(&execution, &snapshot).is_err());
    }
}

#[cfg(test)]
mod retry_wake_tests {
    use super::*;
    #[test]
    fn jittered_retry_deadlines_are_not_rounded_into_one_scan() {
        let config = AgentAutomationConfig::default();
        let supervisor = AgentAutomationSupervisor::new(
            config.clone(),
            Arc::new(BrowserAutomationExecutor::new(config)),
        );
        supervisor
            .next_retry_scan_ms
            .store(30_000, Ordering::Relaxed);
        supervisor
            .next_retry_scan_ms
            .fetch_min(12_345, Ordering::Relaxed);
        assert_eq!(
            supervisor.retry_scan_delay(12_000),
            Duration::from_millis(345)
        );
        assert_eq!(supervisor.retry_scan_delay(12_345), Duration::ZERO);
        assert_eq!(supervisor.retry_scan_delay(13_000), Duration::ZERO);
        supervisor
            .next_retry_scan_ms
            .store(100_000, Ordering::Relaxed);
        assert_eq!(supervisor.retry_scan_delay(13_000), Duration::from_secs(30));
    }
}

fn failed_companion_status(code: &str) -> AutomationActionStatus {
    // These pre-Send availability failures share the long retry schedule while
    // keeping the companion's original error message for the owner interface.
    match code {
        "conversation_unavailable" | "editor_unavailable" => {
            AutomationActionStatus::ConversationUnavailable
        }
        "conversation_limit" => AutomationActionStatus::ConversationLimit,
        _ => AutomationActionStatus::Failed,
    }
}

fn blocks_automatic_requeue(status: AutomationActionStatus) -> bool {
    matches!(
        status,
        AutomationActionStatus::Navigating
            | AutomationActionStatus::Sending
            | AutomationActionStatus::Generating
            | AutomationActionStatus::ApprovalRequired
            | AutomationActionStatus::AuthenticationRequired
            | AutomationActionStatus::ConversationUnavailable
            | AutomationActionStatus::ConversationLimit
            | AutomationActionStatus::RateLimited
            | AutomationActionStatus::Interference
            | AutomationActionStatus::Ambiguous
            | AutomationActionStatus::Failed
            | AutomationActionStatus::Cancelled
    )
}

fn bounded_unread_batch(
    messages: &[crate::markdown_chat::UnreadMessage],
    goal_revision: Option<u64>,
    ticket_overhead: usize,
) -> Result<Vec<crate::markdown_chat::UnreadMessage>, String> {
    let mut selected = Vec::new();
    for message in messages {
        if selected.len() >= MAX_UNREAD_MESSAGES {
            break;
        }
        let candidate_messages = selected
            .iter()
            .chain(std::iter::once(message))
            .map(|message| CompanionUnreadMessage {
                id: message.id.clone(),
                markdown: message.model_markdown(),
            })
            .collect::<Vec<_>>();
        let prompt = if let Some(goal_revision) = goal_revision {
            ActionPrompt::OfflineCombined {
                batch_id: "0".repeat(64),
                messages: candidate_messages,
                goal_revision,
            }
        } else {
            ActionPrompt::OfflineChat {
                batch_id: "0".repeat(64),
                messages: candidate_messages,
            }
        };
        if rendered_action_prompt_bytes(&prompt).saturating_add(ticket_overhead) > MAX_PROMPT_BYTES
        {
            if selected.is_empty() {
                return Err(
                    "Browser automation unread message is too large; it was preserved without truncation"
                        .into(),
                );
            }
            break;
        }
        selected.push(message.clone());
    }
    Ok(selected)
}

fn action_is_terminal(state: CompanionActionState) -> bool {
    !matches!(
        state,
        CompanionActionState::Queued
            | CompanionActionState::Navigating
            | CompanionActionState::Sending
            | CompanionActionState::Generating
    )
}

async fn execution_matches(
    task: &AutomationTaskHandle,
    execution: &AutomationExecution,
    now_ms: u64,
) -> Result<bool, String> {
    if task.owner_generation != execution.owner_generation {
        return Ok(false);
    }
    let state = task.store.load(&task.task)?;
    if state.action.id.as_deref() != Some(execution.request.action_id.as_str())
        || state.action.status != AutomationActionStatus::Queued
        || state.pairing.generation != execution.request.pairing_generation
    {
        return Ok(false);
    }
    if let ActionPrompt::PairVerify { reference } | ActionPrompt::CreateChat { reference, .. } =
        &execution.request.prompt
    {
        if state.pairing.candidate_url.as_deref() != Some(execution.request.url.as_str())
            || state.pairing.pending.as_ref().is_none_or(|pending| {
                pending.generation != execution.request.pairing_generation
                    || pending.owner_generation != execution.owner_generation
            })
            || execution.pairing_commit.as_ref().is_none_or(|commit| {
                commit.generation != execution.request.pairing_generation
                    || commit.task.stable_key() != task.task.stable_key()
            })
        {
            return Ok(false);
        }
        return task
            .store
            .activate_queued_pairing(
                &task.task,
                &execution.request.action_id,
                execution.request.pairing_generation,
                execution.owner_generation,
                reference,
                now_ms,
            )
            .map(|_| true)
            .or_else(|error| {
                if error.contains("invalid, expired, or already used") {
                    Ok(false)
                } else {
                    Err(error)
                }
            });
    }
    if state.selection.model != execution.request.model
        || state.selection.effort != execution.request.effort
        || state
            .pairing
            .account_fingerprint
            .as_deref()
            .or(state.selection.account_fingerprint.as_deref())
            != Some(execution.account_fingerprint.as_str())
        || (execution.request.model.is_some()
            && state.selection.catalog_revision.as_deref()
                != Some(execution.catalog_revision.as_str()))
    {
        return Ok(false);
    }
    let activity = task.chat.offline_activity().await?;
    let renewing =
        if let ActionPrompt::TurnRenewal { ref handoff_id } = execution.request.prompt {
            let config = task.store.turn_renewal_settings()?;
            if !state.turn_renewal.send.as_ref().is_some_and(|send| {
                config.enabled_for(send.model.as_deref(), send.effort.as_deref())
            }) || state
                .turn_renewal
                .handoff
                .as_ref()
                .is_none_or(|h| h.id != *handoff_id || !h.report_complete)
            {
                return Ok(false);
            }
            true
        } else if matches!(execution.request.prompt, ActionPrompt::TaskContinuation {}) {
            if !state.pairing.resume_task {
                return Ok(false);
            }
            true
        } else {
            false
        };
    if activity.agent_stop_requested
        || !renewing
            && (activity.is_online(now_ms, task.store.offline_after_ms())
                || activity
                    .agent_waiting_until_ms
                    .is_some_and(|until| now_ms < until))
        || state.pairing.canonical_url.as_deref() != Some(execution.request.url.as_str())
        || state.pairing.verified_owner_generation != Some(execution.owner_generation)
    {
        return Ok(false);
    }
    if let Some(revision) = execution_goal_revision(execution)
        && state
            .goal
            .as_ref()
            .is_none_or(|goal| goal.revision != revision || goal.status != GoalStatus::Active)
    {
        return Ok(false);
    }
    if !execution.unread_message_ids.is_empty() {
        let unread = task.chat.unread_snapshot().await?;
        let queued_messages = execution_unread_messages(execution);
        let accepted = if state.chat_mirror.owner_generation == Some(execution.owner_generation) {
            state
                .chat_mirror
                .message_ids
                .iter()
                .collect::<std::collections::HashSet<_>>()
        } else {
            std::collections::HashSet::new()
        };
        let current_messages = unread
            .messages
            .iter()
            .filter(|message| !accepted.contains(&message.id))
            .take(queued_messages.len())
            .collect::<Vec<_>>();
        if queued_messages.len() != execution.unread_message_ids.len()
            || current_messages.len() != queued_messages.len()
            || queued_messages
                .iter()
                .zip(&execution.unread_message_ids)
                .any(|(message, id)| &message.id != id)
            || queued_messages
                .iter()
                .zip(current_messages)
                .any(|(queued, current)| {
                    current.id != queued.id || current.model_markdown() != queued.markdown
                })
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn execution_goal_revision(execution: &AutomationExecution) -> Option<u64> {
    match &execution.request.prompt {
        ActionPrompt::OfflineGoal { goal_revision }
        | ActionPrompt::OfflineCombined { goal_revision, .. } => Some(*goal_revision),
        ActionPrompt::PairVerify { .. }
        | ActionPrompt::CreateChat { .. }
        | ActionPrompt::OfflineChat { .. }
        | ActionPrompt::TurnRenewal { .. }
        | ActionPrompt::TaskContinuation {} => None,
    }
}

fn execution_kind(execution: &AutomationExecution) -> AutomationActionKind {
    match &execution.request.prompt {
        ActionPrompt::PairVerify { .. } | ActionPrompt::CreateChat { .. } => {
            AutomationActionKind::PairVerify
        }
        ActionPrompt::TurnRenewal { .. } | ActionPrompt::TaskContinuation {} => {
            AutomationActionKind::TurnRenewal
        }
        ActionPrompt::OfflineChat { .. } => AutomationActionKind::OfflineChat,
        ActionPrompt::OfflineGoal { .. } => AutomationActionKind::OfflineGoal,
        ActionPrompt::OfflineCombined { .. } => AutomationActionKind::OfflineCombined,
    }
}

fn action_priority(kind: AutomationActionKind) -> u8 {
    match kind {
        AutomationActionKind::PairVerify => 0,
        AutomationActionKind::OfflineChat | AutomationActionKind::OfflineCombined => 1,
        AutomationActionKind::OfflineGoal | AutomationActionKind::TurnRenewal => 2,
    }
}

fn execution_batch_id(execution: &AutomationExecution) -> Option<&str> {
    match &execution.request.prompt {
        ActionPrompt::OfflineChat { batch_id, .. }
        | ActionPrompt::OfflineCombined { batch_id, .. } => Some(batch_id),
        ActionPrompt::PairVerify { .. }
        | ActionPrompt::CreateChat { .. }
        | ActionPrompt::OfflineGoal { .. }
        | ActionPrompt::TurnRenewal { .. }
        | ActionPrompt::TaskContinuation {} => None,
    }
}

fn execution_unread_messages(execution: &AutomationExecution) -> &[CompanionUnreadMessage] {
    match &execution.request.prompt {
        ActionPrompt::OfflineChat { messages, .. }
        | ActionPrompt::OfflineCombined { messages, .. } => messages,
        ActionPrompt::PairVerify { .. }
        | ActionPrompt::CreateChat { .. }
        | ActionPrompt::OfflineGoal { .. }
        | ActionPrompt::TurnRenewal { .. }
        | ActionPrompt::TaskContinuation {} => &[],
    }
}

#[allow(clippy::too_many_arguments)]
fn queued_context_matches(
    queued: &AutomationExecution,
    owner_generation: u64,
    pairing_generation: u64,
    url: &str,
    model: &Option<String>,
    effort: &Option<String>,
    catalog_revision: &str,
    account_fingerprint: &str,
    goal_revision: Option<u64>,
) -> bool {
    queued.owner_generation == owner_generation
        && queued.request.pairing_generation == pairing_generation
        && queued.request.url == url
        && queued.request.expected_url == url
        && &queued.request.model == model
        && &queued.request.effort == effort
        && queued.catalog_revision == catalog_revision
        && queued.account_fingerprint == account_fingerprint
        && execution_goal_revision(queued) == goal_revision
}

fn companion_status(status: super::CompanionActionState) -> AutomationActionStatus {
    match status {
        super::CompanionActionState::Unpaired => AutomationActionStatus::Unpaired,
        super::CompanionActionState::Unverified => AutomationActionStatus::Unverified,
        super::CompanionActionState::Queued => AutomationActionStatus::Queued,
        super::CompanionActionState::Waiting => AutomationActionStatus::Queued,
        super::CompanionActionState::Navigating => AutomationActionStatus::Navigating,
        super::CompanionActionState::Sending => AutomationActionStatus::Sending,
        super::CompanionActionState::Generating => AutomationActionStatus::Generating,
        super::CompanionActionState::Idle => AutomationActionStatus::Idle,
        super::CompanionActionState::ApprovalRequired => AutomationActionStatus::ApprovalRequired,
        super::CompanionActionState::AuthenticationRequired => {
            AutomationActionStatus::AuthenticationRequired
        }
        super::CompanionActionState::RateLimited => AutomationActionStatus::RateLimited,
        super::CompanionActionState::ExternalInterference => AutomationActionStatus::Interference,
        super::CompanionActionState::Ambiguous => AutomationActionStatus::Ambiguous,
        super::CompanionActionState::Stalled => AutomationActionStatus::Stalled,
        super::CompanionActionState::Failed => AutomationActionStatus::Failed,
    }
}

fn random_id(prefix: &str) -> Result<String, String> {
    let mut bytes = [0u8; 18];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| format!("Could not generate a browser automation ID: {error}"))?;
    Ok(format!("{prefix}-{}", URL_SAFE_NO_PAD.encode(bytes)))
}

fn unread_batch_id<'a>(
    task_key: &str,
    owner_generation: u64,
    pairing_generation: u64,
    revision: &str,
    message_ids: impl Iterator<Item = &'a str>,
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"codexify-unread-batch-v1\0");
    hash.update(task_key.as_bytes());
    hash.update(owner_generation.to_le_bytes());
    hash.update(pairing_generation.to_le_bytes());
    hash.update(revision.as_bytes());
    for id in message_ids {
        hash.update((id.len() as u64).to_le_bytes());
        hash.update(id.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

#[cfg(all(test, unix))]
mod browser_startup_tests {
    use super::super::recovery::companion_client_or_start;
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn publish_descriptor(path: &std::path::Path, endpoint: &str) {
        std::fs::set_permissions(
            path.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let data = serde_json::json!({"version":1,"kind":"codexify-browser-companion",
            "pid":std::process::id(),"endpoint":endpoint,
            "token":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "build":format!("startup-test{}",super::super::PROTOCOL_BUILD_SUFFIX),
            "createdAt":chrono::Utc::now().to_rfc3339()});
        std::fs::write(path, serde_json::to_vec(&data).unwrap()).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    async fn health_server(
        ready: Arc<std::sync::atomic::AtomicBool>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let app=axum::Router::new().route("/v1/health",axum::routing::get(move||{
            let ready=ready.clone();async move{axum::Json(serde_json::json!({"protocolVersion":1,"kind":"codexify-browser-companion","build":format!("startup-test{}",super::super::PROTOCOL_BUILD_SUFFIX),"status":if ready.load(Ordering::SeqCst){"idle"}else{"unavailable"},"authenticated":ready.load(Ordering::SeqCst),"accountFingerprint":if ready.load(Ordering::SeqCst){Some("startup-account")}else{None},"activeActionId":null}))}
        }));
        (
            endpoint,
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }),
        )
    }

    #[tokio::test]
    async fn simultaneous_actions_start_a_missing_companion_once_and_reuse_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("descriptor.json");
        let (endpoint, server) =
            health_server(Arc::new(std::sync::atomic::AtomicBool::new(true))).await;
        let config = AgentAutomationConfig {
            enabled: true,
            descriptor_path: path.display().to_string(),
            ..Default::default()
        };
        let lock = Mutex::new(());
        let cancellation = CancellationToken::new();
        let starts = AtomicUsize::new(0);
        let start = || async {
            starts.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(10)).await;
            publish_descriptor(&path, &endpoint);
            Ok(())
        };
        let (first, second) = tokio::join!(
            companion_client_or_start(&config, &lock, &cancellation, start),
            companion_client_or_start(&config, &lock, &cancellation, start)
        );
        assert!(first.is_ok(), "{:?}", first.err());
        assert!(second.is_ok(), "{:?}", second.err());
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(
            companion_client_or_start(&config, &lock, &cancellation, || async {
                panic!("Healthy companion must be reused")
            })
            .await
            .is_ok()
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_valid_descriptor_with_unavailable_health_must_be_repaired() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("descriptor.json");
        let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (endpoint, server) = health_server(ready.clone()).await;
        publish_descriptor(&path, &endpoint);
        let config = AgentAutomationConfig {
            enabled: true,
            descriptor_path: path.display().to_string(),
            ..Default::default()
        };
        let starts = AtomicUsize::new(0);
        companion_client_or_start(
            &config,
            &Mutex::new(()),
            &CancellationToken::new(),
            || async {
                starts.fetch_add(1, Ordering::SeqCst);
                ready.store(true, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn cancellation_and_launch_failure_never_produce_a_ready_client() {
        let root = tempfile::tempdir().unwrap();
        let config = AgentAutomationConfig {
            enabled: true,
            descriptor_path: root.path().join("descriptor.json").display().to_string(),
            ..Default::default()
        };
        let lock = Mutex::new(());
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(
            companion_client_or_start(&config, &lock, &cancellation, || async {
                panic!("Cancelled action must not launch Chrome")
            })
            .await
            .err()
            .unwrap()
            .contains("cancelled")
        );
        let error =
            companion_client_or_start(&config, &lock, &CancellationToken::new(), || async {
                Err("Installed Chrome could not launch".into())
            })
            .await
            .err()
            .unwrap();
        assert_eq!(error, "Installed Chrome could not launch");
    }

    #[tokio::test]
    #[ignore = "requires an isolated home containing the signed companion runtime"]
    async fn signed_companion_is_launched_by_an_action_without_sending_a_message() {
        assert_eq!(
            std::env::var("CODEXIFY_TEST_RECOVERY_SMOKE").as_deref(),
            Ok("1")
        );
        let root = std::path::PathBuf::from(std::env::var_os("CODEXIFY_BROWSER_HOME").unwrap());
        assert!(root.join("recovery-smoke-owned").is_file());
        let config = AgentAutomationConfig {
            enabled: true,
            descriptor_path: root.join("descriptor.json").display().to_string(),
            ..Default::default()
        };
        let manager = super::super::BrowserCompanionManager::from_config(&config).unwrap();
        assert_eq!(
            manager.status().await.state,
            super::super::BrowserLifecycleState::Stopped
        );
        let executor = BrowserAutomationExecutor::new(config.clone());
        let execution = AutomationExecution {
            task_key: "a".repeat(64),
            owner_generation: 0,
            account_fingerprint: "deliberately-not-the-real-account".into(),
            catalog_revision: "smoke".into(),
            activity_epoch: "smoke".into(),
            activity_sequence: 1,
            tool_call_count_at_start: 0,
            unread_message_ids: Vec::new(),
            transcript_revision: String::new(),
            pairing_commit: None,
            request: ExecuteActionRequest {
                action_id: "recovery-smoke".into(),
                idempotency_key: "recovery-smoke".into(),
                url: "https://chatgpt.com/c/never-sent".into(),
                expected_url: "https://chatgpt.com/c/never-sent".into(),
                pairing_generation: 1,
                connector_name: "Codexify".into(),
                model: None,
                effort: None,
                expected_account_fingerprint: "deliberately-not-the-real-account".into(),
                prompt: ActionPrompt::PairVerify {
                    reference: "A".repeat(43),
                },
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
        let client = BrowserCompanionClient::from_config(&config);
        let health = match &client {
            Ok(client) => client.health(&CancellationToken::new()).await,
            Err(error) => Err(error.clone()),
        };
        let stopped = manager.stop().await;
        let error = result
            .err()
            .expect("Account guard must prevent the smoke message from being sent");
        assert!(!error.send_accepted);
        assert!(client.is_ok());
        assert!(health.is_ok());
        assert!(stopped.is_ok());
        assert!(!root.join("descriptor.json").exists());
        assert!(root.join("chrome-profile").is_dir());
        println!(
            "Signed companion was launched by the action; account guard prevented any Send; isolated profile was retained"
        );
    }
}
