use std::fmt;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Method, StatusCode, Url, header};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::markdown_chat::AgentAutomationConfig;

pub const PROTOCOL_VERSION: u32 = 1;
pub const COMPANION_KIND: &str = "codexify-browser-companion";
pub const PROTOCOL_SCHEMA_HASH: &str =
    "36957a042ed31cae11d0401411999119d39c22af98278a46b25c412b0be765c6";
pub const PROTOCOL_BUILD_SUFFIX: &str =
    "+protocol.36957a042ed31cae11d0401411999119d39c22af98278a46b25c412b0be765c6";

const MAX_DESCRIPTOR_BYTES: u64 = 8192;
const MAX_JSON_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = MAX_JSON_BYTES;
const MAX_EVENT_STREAM_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVENT_LINE_BYTES: usize = 128 * 1024;
const MAX_DELTA_BYTES: usize = 64 * 1024;
const MAX_FINAL_ANSWER_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_PROMPT_BYTES: usize = 256 * 1024;
pub(crate) const MAX_UNREAD_MESSAGES: usize = 128;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub struct BrowserCompanionClient {
    endpoint: Url,
    token: Arc<Zeroizing<String>>,
    build: Arc<str>,
    pid: u32,
    http: reqwest::Client,
    request_timeout: Duration,
    event_idle_timeout: Duration,
}

impl fmt::Debug for BrowserCompanionClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrowserCompanionClient")
            .field("build", &self.build)
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CompanionDescriptor {
    version: u32,
    kind: String,
    pid: u32,
    endpoint: String,
    token: String,
    build: String,
    created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanionSessionStatus {
    Idle,
    Busy,
    AuthenticationRequired,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CompanionHealth {
    pub protocol_version: u32,
    pub kind: String,
    pub build: String,
    pub status: CompanionSessionStatus,
    pub authenticated: bool,
    pub account_fingerprint: Option<String>,
    pub active_action_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_capacity: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_action_slots: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_action_ids: Vec<String>,
}

impl CompanionHealth {
    pub(crate) fn owns_action(&self, action_id: &str) -> bool {
        self.active_action_id.as_deref() == Some(action_id)
            || self.active_action_ids.iter().any(|id| id == action_id)
    }

    pub(crate) fn free_action_slots(&self) -> usize {
        self.available_action_slots.unwrap_or_else(|| {
            usize::from(
                self.status == CompanionSessionStatus::Idle
                    && self.active_action_id.is_none()
                    && self.active_action_ids.is_empty(),
            )
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CapabilityCatalog {
    pub protocol_version: u32,
    pub build: String,
    pub account_fingerprint: String,
    pub catalog_revision: String,
    pub models: Vec<ModelCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ModelCapability {
    pub id: String,
    pub label: String,
    pub efforts: Vec<EffortCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EffortCapability {
    pub id: String,
    pub label: String,
    pub available: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RevealWindowRequest {
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RevealWindowReceipt {
    pub protocol_version: u32,
    pub build: String,
    pub revealed: bool,
    pub active_action_id: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ExecuteActionRequest {
    pub action_id: String,
    pub idempotency_key: String,
    pub url: String,
    pub pairing_generation: u64,
    pub connector_name: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub expected_account_fingerprint: String,
    pub prompt: ActionPrompt,
    pub expected_url: String,
    pub auto_approve_tool_calls: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub bypass_conversation_cooldown: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub retain_pairing_window: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairing_continuation_of: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_ticket: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ActionPrompt {
    CreateChat {
        continuation_token: String,
        reference: String,
        setup_reference: Option<String>,
    },
    PairVerify {
        reference: String,
    },
    OfflineChat {
        batch_id: String,
        messages: Vec<CompanionUnreadMessage>,
    },
    OfflineGoal {
        goal_revision: u64,
    },
    TurnRenewal {
        handoff_id: String,
    },
    TaskContinuation {},
    OfflineCombined {
        batch_id: String,
        messages: Vec<CompanionUnreadMessage>,
        goal_revision: u64,
    },
}

#[derive(Clone, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CompanionUnreadMessage {
    pub id: String,
    pub markdown: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanionActionState {
    Unpaired,
    Unverified,
    Queued,
    Waiting,
    Navigating,
    Sending,
    Generating,
    Idle,
    ApprovalRequired,
    AuthenticationRequired,
    RateLimited,
    ExternalInterference,
    Ambiguous,
    Stalled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CompanionSelection {
    pub model: String,
    pub effort: String,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CompanionAttention {
    pub code: String,
    pub message: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CompanionActionSnapshot {
    pub protocol_version: u32,
    pub build: String,
    pub action_id: String,
    pub state: CompanionActionState,
    pub send_accepted: bool,
    pub canonical_url: Option<String>,
    pub selected: Option<CompanionSelection>,
    pub last_event_sequence: u64,
    #[serde(default)]
    pub cooldown_until_ms: Option<u64>,
    pub attention: Option<CompanionAttention>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CancelActionRequest<'a> {
    action_id: &'a str,
    idempotency_key: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CancelActionReceipt {
    pub protocol_version: u32,
    pub build: String,
    pub action_id: String,
    pub state: CompanionActionState,
    pub cancelled: bool,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionEvent {
    pub sequence: u64,
    pub action_id: String,
    pub at_ms: u64,
    #[serde(flatten)]
    pub kind: CompanionEventKind,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum CompanionEventKind {
    Queued,
    Waiting {
        available_at_ms: u64,
    },
    Navigating,
    ComposerReady,
    ModelVerified {
        model: String,
        effort: String,
    },
    ConnectorVerified,
    SendAccepted,
    ConversationCreated {
        canonical_url: String,
    },
    SubmissionComplete {
        canonical_url: String,
    },
    StatusDelta {
        text: String,
        continuation: bool,
    },
    CommentaryDelta {
        markdown: String,
        continuation: bool,
    },
    AnswerDelta {
        markdown: String,
        continuation: bool,
    },
    GenerationComplete {
        final_markdown: Option<String>,
        canonical_url: Option<String>,
    },
    ApprovalRequired {
        message: Option<String>,
    },
    AuthenticationRequired,
    RateLimited {
        message: Option<String>,
    },
    ExternalInterference {
        message: Option<String>,
    },
    AmbiguousSend {
        message: Option<String>,
    },
    Failed {
        code: String,
        message: String,
    },
}

enum ExecuteAttempt {
    Complete(CompanionActionSnapshot),
    Failed {
        message: String,
        retryable_if_absent: bool,
        ambiguous_if_unreconciled: bool,
    },
}

pub(crate) struct CompanionExecuteError {
    pub message: String,
    pub send_accepted: bool,
}

struct RequestFailure {
    message: String,
    may_have_reached_companion: bool,
}

impl CompanionEventKind {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::SubmissionComplete { .. }
                | Self::Waiting { .. }
                | Self::GenerationComplete { .. }
                | Self::ApprovalRequired { .. }
                | Self::AuthenticationRequired
                | Self::RateLimited { .. }
                | Self::ExternalInterference { .. }
                | Self::AmbiguousSend { .. }
                | Self::Failed { .. }
        )
    }
}

impl BrowserCompanionClient {
    pub fn from_config(config: &AgentAutomationConfig) -> Result<Self, String> {
        let path = expand_descriptor_path(&config.descriptor_path)?;
        Self::from_descriptor_path(&path)
    }

    pub fn from_descriptor_path(path: &Path) -> Result<Self, String> {
        Self::from_descriptor_path_with_timeout(path, DEFAULT_REQUEST_TIMEOUT)
    }

    #[doc(hidden)]
    pub fn from_descriptor_path_with_timeout(
        path: &Path,
        request_timeout: Duration,
    ) -> Result<Self, String> {
        Self::from_descriptor_path_with_timeouts(path, request_timeout, EVENT_IDLE_TIMEOUT)
    }

    #[doc(hidden)]
    pub fn from_descriptor_path_with_timeouts(
        path: &Path,
        request_timeout: Duration,
        event_idle_timeout: Duration,
    ) -> Result<Self, String> {
        if request_timeout.is_zero()
            || request_timeout > Duration::from_secs(600)
            || event_idle_timeout.is_zero()
            || event_idle_timeout > Duration::from_secs(600)
        {
            return Err("Browser companion request timeout is invalid".into());
        }
        let descriptor = read_descriptor(path)?;
        validate_descriptor(&descriptor)?;
        let endpoint = Url::parse(&descriptor.endpoint)
            .map_err(|_| "Browser companion descriptor endpoint is invalid".to_string())?;
        crate::tls::ensure_crypto_provider();
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(request_timeout.min(Duration::from_secs(10)))
            .build()
            .map_err(|_| "Could not initialize the browser companion client".to_string())?;
        Ok(Self {
            endpoint,
            token: Arc::new(Zeroizing::new(descriptor.token)),
            build: Arc::from(descriptor.build),
            pid: descriptor.pid,
            http,
            request_timeout,
            event_idle_timeout,
        })
    }

    pub fn build(&self) -> &str {
        &self.build
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Optional browser-only probe. It performs no ChatGPT account/session request.
    pub async fn liveness(&self, cancellation: &CancellationToken) -> Result<Option<bool>, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        struct Liveness {
            protocol_version: u32,
            build: String,
            available: bool,
        }
        let response = self
            .send_json(
                Method::GET,
                "/v1/liveness",
                Option::<&Value>::None,
                cancellation,
            )
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(self.response_status_error(response, cancellation).await);
        }
        let value: Liveness = self.decode_json_response(response, cancellation).await?;
        if value.protocol_version != PROTOCOL_VERSION || value.build != self.build.as_ref() {
            return Err("Browser liveness response has an incompatible protocol".into());
        }
        Ok(Some(value.available))
    }

    pub async fn health(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<CompanionHealth, String> {
        let health: CompanionHealth = self
            .request_json(
                Method::GET,
                "/v1/health",
                Option::<&Value>::None,
                cancellation,
            )
            .await?;
        self.validate_health(&health)?;
        Ok(health)
    }

    /// Action preflight waits for a free page's fresh account observation. UI
    /// health remains passive and may report warming without proving sign-out.
    pub async fn ready_health(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<CompanionHealth, String> {
        let health: CompanionHealth = self
            .request_json(
                Method::GET,
                "/v1/health?ready=1",
                Option::<&Value>::None,
                cancellation,
            )
            .await?;
        self.validate_health(&health)?;
        Ok(health)
    }

    pub async fn turn_handoff_complete(
        &self,
        url: &str,
        account: &str,
        handoff_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        struct Receipt {
            protocol_version: u32,
            build: String,
            complete: bool,
        }
        let receipt:Receipt=self.request_json(Method::POST,"/v1/turns/handoff",Some(&serde_json::json!({"url":url,"expectedAccountFingerprint":account,"handoffId":handoff_id})),cancellation).await?;
        if receipt.protocol_version != PROTOCOL_VERSION || receipt.build != self.build.as_ref() {
            return Err("Turn handoff receipt has an incompatible protocol".into());
        }
        Ok(receipt.complete)
    }

    pub async fn approve_pending_conversation(
        &self,
        url: &str,
        account: &str,
        connector_name: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, String> {
        validate_chatgpt_url(url)?;
        validate_opaque(account, "browser account fingerprint")?;
        validate_visible(connector_name, 128, "browser connector name")?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        struct Receipt {
            protocol_version: u32,
            build: String,
            approved: bool,
        }
        let receipt: Receipt = self.request_json(Method::POST, "/v1/approvals/conversation",
            Some(&serde_json::json!({"url":url,"expectedAccountFingerprint":account,"connectorName":connector_name})), cancellation).await?;
        self.validate_envelope(receipt.protocol_version, &receipt.build)?;
        Ok(receipt.approved)
    }

    pub async fn login(&self, cancellation: &CancellationToken) -> Result<CompanionHealth, String> {
        let health: CompanionHealth = self
            .request_json(
                Method::POST,
                "/v1/session/login",
                Some(&serde_json::json!({})),
                cancellation,
            )
            .await?;
        self.validate_health(&health)?;
        Ok(health)
    }

    pub async fn account_plan(
        &self,
        epoch: &str,
        cancellation: &CancellationToken,
    ) -> Result<serde_json::Value, String> {
        let response = self
            .send_json_with_timeout(
                Method::POST,
                "/v1/account/plan",
                Some(&serde_json::json!({"epoch":epoch})),
                cancellation,
                Some(Duration::from_secs(75)),
            )
            .await?;
        if !response.status().is_success() {
            return Err(self.response_status_error(response, cancellation).await);
        }
        self.decode_json_response(response, cancellation).await
    }

    pub async fn navigation_available_at(
        &self,
        url: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<u64>, String> {
        validate_chatgpt_url(url)?;
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Availability {
            protocol_version: u32,
            build: String,
            available_at_ms: Option<u64>,
        }
        let response = self
            .send_json_with_timeout(
                Method::POST,
                "/v1/navigation/availability",
                Some(&serde_json::json!({"url":url})),
                cancellation,
                Some(Duration::from_secs(2)),
            )
            .await?;
        if !response.status().is_success() {
            return Err(self.response_status_error(response, cancellation).await);
        }
        let value: Availability = self.decode_json_response(response, cancellation).await?;
        self.validate_envelope(value.protocol_version, &value.build)?;
        if value
            .available_at_ms
            .is_some_and(|at| at == 0 || at > 9_007_199_254_740_991)
        {
            return Err("Browser availability deadline is invalid".into());
        }
        Ok(value.available_at_ms)
    }

    pub async fn refresh_connector(
        &self,
        name: &str,
        cancellation: &CancellationToken,
    ) -> Result<serde_json::Value, String> {
        let response = self
            .send_json_with_timeout(
                Method::POST,
                "/v1/connectors/refresh",
                Some(&serde_json::json!({"name":name})),
                cancellation,
                Some(Duration::from_secs(150)),
            )
            .await?;
        if !response.status().is_success() {
            return Err(self.response_status_error(response, cancellation).await);
        }
        self.decode_json_response(response, cancellation).await
    }

    pub async fn capabilities(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<CapabilityCatalog, String> {
        let catalog: CapabilityCatalog = self
            .request_json(
                Method::POST,
                "/v1/session/capabilities",
                Some(&serde_json::json!({})),
                cancellation,
            )
            .await?;
        self.validate_envelope(catalog.protocol_version, &catalog.build)?;
        validate_catalog(&catalog)?;
        Ok(catalog)
    }

    pub async fn new_chat_capabilities(
        &self,
        project_url: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<CapabilityCatalog, String> {
        let response = self
            .send_json_with_timeout(
                Method::POST,
                "/v1/new-chat/capabilities",
                Some(&serde_json::json!({"projectUrl":project_url})),
                cancellation,
                Some(Duration::from_secs(180)),
            )
            .await?;
        if !response.status().is_success() {
            return Err(self.response_status_error(response, cancellation).await);
        }
        let catalog: CapabilityCatalog = self.decode_json_response(response, cancellation).await?;
        validate_catalog(&catalog)?;
        self.validate_envelope(catalog.protocol_version, &catalog.build)?;
        Ok(catalog)
    }

    pub async fn reveal(
        &self,
        request: &RevealWindowRequest,
        cancellation: &CancellationToken,
    ) -> Result<RevealWindowReceipt, String> {
        if let Some(url) = &request.url {
            validate_chatgpt_url(url)?;
        }
        let receipt: RevealWindowReceipt = self
            .request_json(
                Method::POST,
                "/v1/window/reveal",
                Some(request),
                cancellation,
            )
            .await?;
        self.validate_envelope(receipt.protocol_version, &receipt.build)?;
        if let Some(action_id) = &receipt.active_action_id {
            validate_opaque(action_id, "browser action ID")?;
        }
        Ok(receipt)
    }

    pub async fn execute(
        &self,
        request: &ExecuteActionRequest,
        cancellation: &CancellationToken,
    ) -> Result<CompanionActionSnapshot, String> {
        self.execute_with_acceptance(request, cancellation)
            .await
            .map_err(|error| error.message)
    }

    pub(crate) async fn execute_with_acceptance(
        &self,
        request: &ExecuteActionRequest,
        cancellation: &CancellationToken,
    ) -> Result<CompanionActionSnapshot, CompanionExecuteError> {
        validate_execute_request(request).map_err(|message| CompanionExecuteError {
            message,
            send_accepted: false,
        })?;
        let first = self.execute_attempt(request, cancellation).await;
        match first {
            ExecuteAttempt::Complete(snapshot) => Ok(snapshot),
            ExecuteAttempt::Failed {
                message,
                retryable_if_absent,
                ambiguous_if_unreconciled,
            } => {
                let first_ambiguous = ambiguous_if_unreconciled;
                if cancellation.is_cancelled() {
                    return Err(CompanionExecuteError {
                        message,
                        send_accepted: first_ambiguous,
                    });
                }
                match self.action_optional(&request.action_id, cancellation).await {
                    Ok(Some(snapshot)) => Ok(snapshot),
                    Ok(None) if retryable_if_absent => {
                        match self.execute_attempt(request, cancellation).await {
                            ExecuteAttempt::Complete(snapshot) => Ok(snapshot),
                            ExecuteAttempt::Failed {
                                message,
                                ambiguous_if_unreconciled,
                                ..
                            } => {
                                let ambiguous_if_unreconciled =
                                    first_ambiguous || ambiguous_if_unreconciled;
                                match self.action_optional(&request.action_id, cancellation).await {
                                    Ok(Some(snapshot)) => Ok(snapshot),
                                    Ok(None) => Err(CompanionExecuteError {
                                        message,
                                        send_accepted: ambiguous_if_unreconciled,
                                    }),
                                    Err(_) => Err(CompanionExecuteError {
                                        message,
                                        send_accepted: ambiguous_if_unreconciled,
                                    }),
                                }
                            }
                        }
                    }
                    Ok(None) => Err(CompanionExecuteError {
                        message,
                        send_accepted: first_ambiguous,
                    }),
                    Err(_) => Err(CompanionExecuteError {
                        message,
                        send_accepted: first_ambiguous,
                    }),
                }
            }
        }
    }

    pub async fn cancel(
        &self,
        action_id: &str,
        idempotency_key: &str,
        cancellation: &CancellationToken,
    ) -> Result<CancelActionReceipt, String> {
        validate_opaque(action_id, "browser action ID")?;
        validate_opaque(idempotency_key, "browser action idempotency key")?;
        let request = CancelActionRequest {
            action_id,
            idempotency_key,
        };
        let receipt: CancelActionReceipt = self
            .request_json(
                Method::POST,
                "/v1/actions/cancel",
                Some(&request),
                cancellation,
            )
            .await?;
        self.validate_envelope(receipt.protocol_version, &receipt.build)?;
        if receipt.action_id != action_id {
            return Err("Browser companion returned the wrong action ID".into());
        }
        Ok(receipt)
    }

    pub async fn refresh_pairing_window(&self, action_id: &str) -> Result<(), String> {
        validate_opaque(action_id, "browser action ID")?;
        let value: serde_json::Value = self
            .request_json(
                Method::POST,
                "/v1/actions/refresh-pairing",
                Some(&serde_json::json!({"actionId":action_id})),
                &CancellationToken::new(),
            )
            .await?;
        if value["retained"] != true {
            return Err("The browser pairing reservation is no longer held".into());
        }
        Ok(())
    }

    pub async fn release_pairing_window(&self, action_id: &str) -> Result<(), String> {
        validate_opaque(action_id, "browser action ID")?;
        let _: serde_json::Value = self
            .request_json(
                Method::POST,
                "/v1/actions/release-pairing",
                Some(&serde_json::json!({"actionId":action_id})),
                &CancellationToken::new(),
            )
            .await?;
        Ok(())
    }

    pub async fn action(
        &self,
        action_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<CompanionActionSnapshot, String> {
        validate_opaque(action_id, "browser action ID")?;
        let path = format!("/v1/actions/{action_id}");
        let snapshot: CompanionActionSnapshot = self
            .request_json(Method::GET, &path, Option::<&Value>::None, cancellation)
            .await?;
        self.validate_action_snapshot(&snapshot, Some(action_id))?;
        Ok(snapshot)
    }

    pub async fn stream_action_events<F>(
        &self,
        action_id: &str,
        after_sequence: u64,
        cancellation: &CancellationToken,
        mut receive: F,
    ) -> Result<(), String>
    where
        F: FnMut(CompanionEvent) -> Result<(), String>,
    {
        validate_opaque(action_id, "browser action ID")?;
        let path = format!("/v1/actions/{action_id}/events?after={after_sequence}");
        let mut response = self
            .send_json_with_timeout::<Value>(Method::GET, &path, None, cancellation, None)
            .await?;
        if !response.status().is_success() {
            return Err(self.response_status_error(response, cancellation).await);
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type
            .split(';')
            .next()
            .is_some_and(|value| value.trim() == "application/x-ndjson")
        {
            return Err("Browser companion event stream has an invalid content type".into());
        }

        let mut total = 0usize;
        let mut buffer = Vec::new();
        let mut last_sequence = after_sequence;
        let mut semantic_deadline = tokio::time::Instant::now() + self.event_idle_timeout;
        loop {
            let chunk = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(cancelled_error()),
                chunk = tokio::time::timeout_at(semantic_deadline, response.chunk()) => chunk
                    .map_err(|_| "Browser companion event stream stalled".to_string())?
                    .map_err(|_| "Browser companion event stream failed".to_string())?,
            };
            let Some(chunk) = chunk else {
                break;
            };
            total = total
                .checked_add(chunk.len())
                .ok_or("Browser companion event stream is too large")?;
            if total > MAX_EVENT_STREAM_BYTES {
                return Err("Browser companion event stream is too large".into());
            }
            buffer.extend_from_slice(&chunk);
            let mut received_event = false;
            while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                let line = buffer.drain(..=newline).collect::<Vec<_>>();
                let line = &line[..line.len() - 1];
                if line.is_empty() {
                    continue;
                }
                if line.len() > MAX_EVENT_LINE_BYTES {
                    return Err("Browser companion event is too large".into());
                }
                let event: CompanionEvent = serde_json::from_slice(line)
                    .map_err(|_| "Browser companion event is invalid".to_string())?;
                validate_event(&event, action_id, last_sequence)?;
                last_sequence = event.sequence;
                received_event = true;
                let terminal = event.kind.is_terminal();
                receive(event)?;
                if terminal {
                    return Ok(());
                }
            }
            if buffer.len() > MAX_EVENT_LINE_BYTES {
                return Err("Browser companion event is too large".into());
            }
            if received_event {
                semantic_deadline = tokio::time::Instant::now() + self.event_idle_timeout;
            }
        }
        if !buffer.is_empty() {
            return Err("Browser companion event stream ended with an incomplete event".into());
        }
        Err("Browser companion event stream ended before a terminal event".into())
    }

    async fn execute_attempt(
        &self,
        request: &ExecuteActionRequest,
        cancellation: &CancellationToken,
    ) -> ExecuteAttempt {
        let response = self
            .send_json_with_timeout_detailed(
                Method::POST,
                "/v1/actions/execute",
                Some(request),
                cancellation,
                Some(self.request_timeout),
            )
            .await;
        match response {
            Ok(response) if response.status().is_success() => {
                match self
                    .decode_json_response::<CompanionActionSnapshot>(response, cancellation)
                    .await
                {
                    Ok(snapshot) => {
                        match self.validate_action_snapshot(&snapshot, Some(&request.action_id)) {
                            Ok(()) => ExecuteAttempt::Complete(snapshot),
                            Err(message) => ExecuteAttempt::Failed {
                                message,
                                retryable_if_absent: false,
                                ambiguous_if_unreconciled: true,
                            },
                        }
                    }
                    Err(message) => ExecuteAttempt::Failed {
                        message,
                        retryable_if_absent: false,
                        ambiguous_if_unreconciled: true,
                    },
                }
            }
            Ok(response) => {
                let code = response.status();
                ExecuteAttempt::Failed {
                    message: self.response_status_error(response, cancellation).await,
                    retryable_if_absent: matches!(
                        code,
                        StatusCode::BAD_GATEWAY
                            | StatusCode::SERVICE_UNAVAILABLE
                            | StatusCode::GATEWAY_TIMEOUT
                    ),
                    ambiguous_if_unreconciled: false,
                }
            }
            Err(error) => ExecuteAttempt::Failed {
                message: error.message,
                retryable_if_absent: !cancellation.is_cancelled(),
                ambiguous_if_unreconciled: error.may_have_reached_companion,
            },
        }
    }

    pub(crate) async fn action_optional(
        &self,
        action_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<CompanionActionSnapshot>, String> {
        validate_opaque(action_id, "browser action ID")?;
        let path = format!("/v1/actions/{action_id}");
        let response = self
            .send_json::<Value>(Method::GET, &path, None, cancellation)
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(self.response_status_error(response, cancellation).await);
        }
        let snapshot = self
            .decode_json_response::<CompanionActionSnapshot>(response, cancellation)
            .await?;
        self.validate_action_snapshot(&snapshot, Some(action_id))?;
        Ok(Some(snapshot))
    }

    fn validate_health(&self, health: &CompanionHealth) -> Result<(), String> {
        self.validate_envelope(health.protocol_version, &health.build)?;
        if health.kind != COMPANION_KIND {
            return Err("Browser companion kind does not match Codexify".into());
        }
        if let Some(fingerprint) = &health.account_fingerprint {
            validate_opaque(fingerprint, "browser account fingerprint")?;
        }
        if health.authenticated != health.account_fingerprint.is_some() {
            return Err("Browser companion authentication state is inconsistent".into());
        }
        if let Some(action_id) = &health.active_action_id {
            validate_opaque(action_id, "browser action ID")?;
        }
        let capacity = health.action_capacity.unwrap_or(1);
        if !(1..=4).contains(&capacity)
            || health
                .available_action_slots
                .is_some_and(|slots| slots > capacity)
            || health.active_action_ids.len() > capacity
        {
            return Err("Browser companion action capacity is invalid".into());
        }
        let mut unique = std::collections::HashSet::new();
        for action_id in &health.active_action_ids {
            validate_opaque(action_id, "browser action ID")?;
            if !unique.insert(action_id) {
                return Err("Browser companion repeats an active action ID".into());
            }
        }
        if let Some(action_id) = &health.active_action_id {
            unique.insert(action_id);
        }
        if unique.len() > capacity
            || health
                .available_action_slots
                .is_some_and(|slots| unique.len() + slots > capacity)
        {
            return Err("Browser companion action capacity is inconsistent".into());
        }
        Ok(())
    }

    fn validate_action_snapshot(
        &self,
        snapshot: &CompanionActionSnapshot,
        expected_action_id: Option<&str>,
    ) -> Result<(), String> {
        self.validate_envelope(snapshot.protocol_version, &snapshot.build)?;
        validate_opaque(&snapshot.action_id, "browser action ID")?;
        if expected_action_id.is_some_and(|expected| snapshot.action_id != expected) {
            return Err("Browser companion returned the wrong action ID".into());
        }
        if let Some(url) = &snapshot.canonical_url {
            validate_chatgpt_url(url)?;
        }
        if let Some(selected) = &snapshot.selected {
            validate_opaque(&selected.model, "browser model ID")?;
            validate_opaque(&selected.effort, "browser effort ID")?;
        }
        if let Some(attention) = &snapshot.attention {
            validate_opaque(&attention.code, "browser attention code")?;
            validate_optional_text(
                attention.message.as_deref(),
                MAX_DELTA_BYTES,
                "browser attention message",
            )?;
        }
        Ok(())
    }

    fn validate_envelope(&self, version: u32, build: &str) -> Result<(), String> {
        if version != PROTOCOL_VERSION {
            return Err("Browser companion protocol version is incompatible".into());
        }
        if build != self.build.as_ref() {
            return Err("Browser companion build does not match its descriptor".into());
        }
        Ok(())
    }

    async fn request_json<T, B>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        cancellation: &CancellationToken,
    ) -> Result<T, String>
    where
        T: for<'de> Deserialize<'de>,
        B: Serialize + ?Sized,
    {
        let response = self.send_json(method, path, body, cancellation).await?;
        if !response.status().is_success() {
            return Err(self.response_status_error(response, cancellation).await);
        }
        self.decode_json_response(response, cancellation).await
    }

    async fn send_json<B>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        cancellation: &CancellationToken,
    ) -> Result<reqwest::Response, String>
    where
        B: Serialize + ?Sized,
    {
        self.send_json_with_timeout(method, path, body, cancellation, Some(self.request_timeout))
            .await
    }

    async fn send_json_with_timeout<B>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        cancellation: &CancellationToken,
        timeout: Option<Duration>,
    ) -> Result<reqwest::Response, String>
    where
        B: Serialize + ?Sized,
    {
        self.send_json_with_timeout_detailed(method, path, body, cancellation, timeout)
            .await
            .map_err(|error| error.message)
    }

    async fn send_json_with_timeout_detailed<B>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        cancellation: &CancellationToken,
        timeout: Option<Duration>,
    ) -> Result<reqwest::Response, RequestFailure>
    where
        B: Serialize + ?Sized,
    {
        if cancellation.is_cancelled() {
            return Err(RequestFailure {
                message: cancelled_error(),
                may_have_reached_companion: false,
            });
        }
        let url = self.request_url(path).map_err(|message| RequestFailure {
            message,
            may_have_reached_companion: false,
        })?;
        // Only static operation labels are logged/displayed: never the loopback
        // endpoint, bearer, action IDs, query strings, or raw reqwest error URL.
        let operation = companion_operation(&method, path);
        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(self.token.as_str())
            .header(header::ACCEPT, "application/json, application/x-ndjson");
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        if let Some(body) = body {
            let encoded = serde_json::to_vec(body).map_err(|_| RequestFailure {
                message: "Could not encode browser companion request".to_string(),
                may_have_reached_companion: false,
            })?;
            if encoded.len() > MAX_JSON_BYTES {
                return Err(RequestFailure {
                    message: "Browser companion request is too large".into(),
                    may_have_reached_companion: false,
                });
            }
            request = request
                .header(header::CONTENT_TYPE, "application/json")
                .body(encoded);
        }
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(RequestFailure {
                message: cancelled_error(),
                may_have_reached_companion: true,
            }),
            response = request.send() => response.map_err(|error| {
                let reason = if error.is_timeout() && error.is_connect() {
                    format!("connection timed out after {}s", self.request_timeout.min(Duration::from_secs(10)).as_secs_f64())
                } else if error.is_timeout() {
                    timeout.map_or_else(|| "timed out".to_string(), |limit| format!("timed out after {}s", limit.as_secs_f64()))
                } else if error.is_connect() {
                    "could not connect".to_string()
                } else {
                    "connection interrupted".to_string()
                };
                tracing::warn!(operation, reason, "Browser companion request failed");
                RequestFailure {
                    message: format!("Browser companion request failed before a verified response ({operation}: {reason})"),
                    may_have_reached_companion: !error.is_connect() && !error.is_builder(),
                }
            }),
        }
    }

    async fn response_status_error(
        &self,
        response: reqwest::Response,
        cancellation: &CancellationToken,
    ) -> String {
        let fallback = status_error(response.status());
        if response.status() != StatusCode::CONFLICT {
            return fallback;
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        struct Failure {
            protocol_version: u32,
            build: String,
            error: Detail,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Detail {
            code: String,
            phase: String,
            message: String,
        }
        let Ok(value) = self
            .decode_json_response::<Failure>(response, cancellation)
            .await
        else {
            return fallback;
        };
        const PHASES: &[&str] = &[
            "composer_lookup",
            "effort_control_lookup",
            "picker_open",
            "model_options",
            "model_identity",
            "effort_selection",
            "effort_restore",
            "capability_discovery",
        ];
        const CODES: &[&str] = &[
            "selector_drift",
            "model_unavailable",
            "external_interference",
            "cancelled",
            "failed",
        ];
        if self
            .validate_envelope(value.protocol_version, &value.build)
            .is_err()
            || !PHASES.contains(&value.error.phase.as_str())
            || !CODES.contains(&value.error.code.as_str())
            || validate_visible(&value.error.message, 4096, "browser failure message").is_err()
        {
            return fallback;
        }
        format!(
            "{} [{}; {}]",
            value.error.message, value.error.phase, value.error.code
        )
    }

    async fn decode_json_response<T>(
        &self,
        response: reqwest::Response,
        cancellation: &CancellationToken,
    ) -> Result<T, String>
    where
        T: for<'de> Deserialize<'de>,
    {
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type
            .split(';')
            .next()
            .is_some_and(|value| value.trim() == "application/json")
        {
            return Err("Browser companion JSON response has an invalid content type".into());
        }
        let bytes = read_bounded_response(response, cancellation, MAX_RESPONSE_BYTES).await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| "Browser companion returned invalid JSON".to_string())
    }

    fn request_url(&self, path: &str) -> Result<Url, String> {
        if !path.starts_with('/') || path.starts_with("//") || path.contains('#') {
            return Err("Browser companion request path is invalid".into());
        }
        self.endpoint
            .join(path.trim_start_matches('/'))
            .map_err(|_| "Browser companion request path is invalid".to_string())
    }
}

fn companion_operation(method: &Method, path: &str) -> &'static str {
    match (method.as_str(), path.split('?').next().unwrap_or_default()) {
        ("GET", "/v1/liveness") => "checking browser liveness",
        ("GET", "/v1/health") => "checking browser account/session",
        ("POST", "/v1/actions/execute") => "admitting browser action",
        ("POST", "/v1/actions/cancel") => "cancelling browser action",
        ("GET", route) if route.starts_with("/v1/actions/") && route.ends_with("/events") => {
            "reading browser action events"
        }
        ("GET", route) if route.starts_with("/v1/actions/") => "reading browser action status",
        ("POST", "/v1/account/plan") => "checking subscription tier",
        ("POST", "/v1/connectors/refresh") => "refreshing connector",
        ("POST", "/v1/turns/handoff") => "checking turn handoff",
        ("POST", "/v1/approvals/conversation") => "checking conversation approval",
        ("POST", "/v1/session/capabilities" | "/v1/new-chat/capabilities") => {
            "reading model catalog"
        }
        ("POST", "/v1/session/login") => "opening browser login",
        ("POST", "/v1/window/reveal") => "showing browser window",
        _ => "browser operation",
    }
}

fn expand_descriptor_path(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed == "~" {
        return crate::util::home_dir()
            .ok_or_else(|| "Browser companion descriptor path requires a home directory".into());
    }
    let path = if let Some(relative) = trimmed.strip_prefix("~/") {
        crate::util::home_dir()
            .ok_or("Browser companion descriptor path requires a home directory")?
            .join(relative)
    } else {
        PathBuf::from(trimmed)
    };
    if !path.is_absolute() || trimmed.starts_with('~') && !trimmed.starts_with("~/") {
        return Err("Browser companion descriptor path must be absolute or start with ~/".into());
    }
    Ok(path)
}

fn read_descriptor(path: &Path) -> Result<CompanionDescriptor, String> {
    if !path.is_absolute() {
        return Err("Browser companion descriptor path must be absolute".into());
    }
    let parent = path
        .parent()
        .ok_or("Browser companion descriptor has no parent directory")?;
    validate_private_directory(parent)?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| "Browser companion descriptor is unavailable".to_string())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("Browser companion descriptor must be a regular non-symlink file".into());
    }
    if metadata.len() > MAX_DESCRIPTOR_BYTES {
        return Err("Browser companion descriptor is too large".into());
    }
    validate_private_file(&metadata, "Browser companion descriptor")?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|_| "Browser companion descriptor is unavailable".to_string())?;
    let metadata = file
        .metadata()
        .map_err(|_| "Browser companion descriptor is unavailable".to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_DESCRIPTOR_BYTES {
        return Err("Browser companion descriptor is invalid".into());
    }
    validate_private_file(&metadata, "Browser companion descriptor")?;
    let mut bytes = Vec::new();
    file.take(MAX_DESCRIPTOR_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Browser companion descriptor is unreadable".to_string())?;
    if bytes.len() as u64 > MAX_DESCRIPTOR_BYTES {
        return Err("Browser companion descriptor is too large".into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| "Browser companion descriptor is invalid JSON".to_string())
}

fn validate_descriptor(descriptor: &CompanionDescriptor) -> Result<(), String> {
    if descriptor.version != PROTOCOL_VERSION || descriptor.kind != COMPANION_KIND {
        return Err("Browser companion descriptor protocol is incompatible".into());
    }
    if descriptor.pid == 0 || !process_is_alive(descriptor.pid) {
        return Err("Browser companion descriptor process is not running".into());
    }
    let endpoint = Url::parse(&descriptor.endpoint)
        .map_err(|_| "Browser companion descriptor endpoint is invalid".to_string())?;
    if endpoint.scheme() != "http"
        || endpoint.host_str() != Some("127.0.0.1")
        || endpoint.port().is_none()
        || endpoint.username() != ""
        || endpoint.password().is_some()
        || endpoint.path() != "/"
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(
            "Browser companion descriptor endpoint must be a private IPv4 loopback URL".into(),
        );
    }
    if descriptor.token.len() != 43
        || URL_SAFE_NO_PAD
            .decode(&descriptor.token)
            .ok()
            .is_none_or(|bytes| bytes.len() != 32)
    {
        return Err("Browser companion descriptor bearer secret is invalid".into());
    }
    validate_build(&descriptor.build)?;
    chrono::DateTime::parse_from_rfc3339(&descriptor.created_at)
        .map_err(|_| "Browser companion descriptor timestamp is invalid".to_string())?;
    Ok(())
}

fn validate_private_directory(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| "Browser companion descriptor directory is unavailable".to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Browser companion descriptor directory must be a real directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(
                "Browser companion descriptor directory must be private to the current user".into(),
            );
        }
    }
    Ok(())
}

fn validate_private_file(metadata: &std::fs::Metadata, label: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(format!("{label} must be private to the current user"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    if pid > i32::MAX as u32 {
        return false;
    }
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    // OpenProcess can succeed for an exited process while another owner holds
    // its process object. A live process is unsignalled; a terminated process
    // is signalled even when its exit code happens to be STILL_ACTIVE (259).
    let running = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
    unsafe { CloseHandle(handle) };
    running
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

fn validate_build(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
        || !value.ends_with(PROTOCOL_BUILD_SUFFIX)
    {
        return Err("Browser companion build is not bound to the current protocol schema".into());
    }
    Ok(())
}

fn validate_catalog(catalog: &CapabilityCatalog) -> Result<(), String> {
    validate_opaque(&catalog.account_fingerprint, "browser account fingerprint")?;
    validate_opaque(&catalog.catalog_revision, "browser catalog revision")?;
    if catalog.models.is_empty() || catalog.models.len() > 32 {
        return Err("Browser companion model catalog has an invalid size".into());
    }
    let mut model_ids = std::collections::HashSet::new();
    for model in &catalog.models {
        validate_opaque(&model.id, "browser model ID")?;
        validate_visible(&model.label, 256, "browser model label")?;
        if !model_ids.insert(&model.id) || model.efforts.is_empty() || model.efforts.len() > 16 {
            return Err("Browser companion model catalog is invalid".into());
        }
        let mut effort_ids = std::collections::HashSet::new();
        for effort in &model.efforts {
            validate_opaque(&effort.id, "browser effort ID")?;
            validate_visible(&effort.label, 256, "browser effort label")?;
            validate_optional_text(effort.reason.as_deref(), 1024, "browser effort reason")?;
            if !effort.available && effort.reason.is_none() {
                return Err("Browser companion unavailable effort has no explanation".into());
            }
            if !effort_ids.insert(&effort.id) {
                return Err("Browser companion model catalog contains duplicate efforts".into());
            }
        }
    }
    Ok(())
}

impl BrowserCompanionClient {
    pub(crate) fn validate_cached_catalog(catalog: &CapabilityCatalog) -> Result<(), String> {
        if catalog.protocol_version != PROTOCOL_VERSION {
            return Err("Cached browser catalog protocol is invalid".into());
        }
        // Cached model metadata may outlive the request protocol that discovered
        // it. Live companion envelopes still require the exact current build.
        validate_visible(&catalog.build, 128, "cached browser build")?;
        if !catalog
            .build
            .split_once("+protocol.")
            .is_some_and(|(release, hash)| {
                !release.is_empty()
                    && release
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
                    && hash.len() == 64
                    && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Err("Cached browser build is invalid".into());
        }
        validate_catalog(catalog)
    }

    pub(crate) fn catalog_cache_path(config: &AgentAutomationConfig) -> Result<PathBuf, String> {
        let descriptor = expand_descriptor_path(&config.descriptor_path)?;
        Ok(descriptor
            .parent()
            .ok_or("Browser descriptor has no parent directory")?
            .join("catalog-cache.json"))
    }
}

fn validate_execute_request(request: &ExecuteActionRequest) -> Result<(), String> {
    if let Some(ticket) = &request.agent_ticket
        && (ticket.len() != 8
            || !ticket
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    {
        return Err("Invalid browser agent ticket".into());
    }
    if request.retain_pairing_window && !matches!(request.prompt, ActionPrompt::CreateChat { .. })
        || request
            .pairing_continuation_of
            .as_deref()
            .is_some_and(|parent| {
                parent == request.action_id
                    || matches!(
                        request.prompt,
                        ActionPrompt::CreateChat { .. } | ActionPrompt::PairVerify { .. }
                    )
            })
    {
        return Err("Invalid pairing window reservation".into());
    }
    if let Some(parent) = &request.pairing_continuation_of {
        validate_opaque(parent, "pairing browser action ID")?;
    }
    validate_opaque(&request.action_id, "browser action ID")?;
    validate_opaque(&request.idempotency_key, "browser action idempotency key")?;
    validate_chatgpt_url(&request.url)?;
    validate_chatgpt_url(&request.expected_url)?;
    if request.pairing_generation == 0 {
        return Err("Browser action pairing generation must be positive".into());
    }
    validate_visible(&request.connector_name, 128, "browser connector name")?;
    validate_opaque(
        &request.expected_account_fingerprint,
        "browser account fingerprint",
    )?;
    match (&request.model, &request.effort) {
        (Some(model), Some(effort)) if !matches!(request.prompt, ActionPrompt::PairVerify { .. }) => {
            validate_opaque(model, "browser model ID")?;
            validate_opaque(effort, "browser effort ID")?;
        }
        (None, None) => {}
        _ => return Err("Browser model and effort must be supplied together; pairing uses the conversation settings".into()),
    }
    validate_prompt(&request.prompt)?;
    if render_action_prompt_with_ticket(&request.prompt, request.agent_ticket.as_deref()).len()
        > MAX_PROMPT_BYTES
    {
        return Err("Browser automation prompt exceeds the safety limit".into());
    }
    let encoded = serde_json::to_vec(request)
        .map_err(|_| "Could not encode browser action request".to_string())?;
    if encoded.len() > MAX_JSON_BYTES {
        return Err("Browser action request is too large".into());
    }
    Ok(())
}

fn validate_prompt(prompt: &ActionPrompt) -> Result<(), String> {
    match prompt {
        ActionPrompt::PairVerify { reference } | ActionPrompt::CreateChat { reference, .. } => {
            if reference.len() != 43
                || URL_SAFE_NO_PAD
                    .decode(reference)
                    .ok()
                    .is_none_or(|bytes| bytes.len() != 32)
            {
                return Err("Browser pairing reference is invalid".into());
            }
        }
        ActionPrompt::TurnRenewal { handoff_id } => {
            if handoff_id.len() != 70
                || !handoff_id.starts_with("renew-")
                || !handoff_id[6..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err("Turn handoff ID is invalid".into());
            }
        }
        ActionPrompt::TaskContinuation {} => {}
        ActionPrompt::OfflineChat { batch_id, messages } => {
            validate_batch(batch_id, messages)?;
        }
        ActionPrompt::OfflineGoal { goal_revision } => {
            if *goal_revision == 0 {
                return Err("Browser goal revision must be positive".into());
            }
        }
        ActionPrompt::OfflineCombined {
            batch_id,
            messages,
            goal_revision,
        } => {
            validate_batch(batch_id, messages)?;
            if *goal_revision == 0 {
                return Err("Browser goal revision must be positive".into());
            }
        }
    }
    if let ActionPrompt::CreateChat {
        continuation_token,
        setup_reference,
        ..
    } = prompt
    {
        if continuation_token.len() != 43
            || !continuation_token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err("Invalid new-chat continuation token".into());
        }
        if let Some(reference) = setup_reference {
            validate_text(reference, 4096, "setup reference")?;
        }
    }
    if rendered_action_prompt_bytes(prompt) > MAX_PROMPT_BYTES {
        return Err("Browser automation prompt exceeds the safety limit".into());
    }
    Ok(())
}

pub(crate) fn rendered_action_prompt_bytes(prompt: &ActionPrompt) -> usize {
    render_action_prompt(prompt).len()
}

pub(crate) fn browser_ticket_prompt_overhead() -> usize {
    render_action_prompt_with_ticket(&ActionPrompt::TaskContinuation {}, Some("AAAAAAAA")).len()
        - render_action_prompt(&ActionPrompt::TaskContinuation {}).len()
}

fn render_action_prompt_with_ticket(prompt: &ActionPrompt, ticket: Option<&str>) -> String {
    let mut rendered = render_action_prompt(prompt);
    if let Some(ticket) = ticket {
        rendered.push_str(&format!("\n\nCodexify supplied a fresh agent ticket for this prompt: `{ticket}`. Pass it as `codexify_ticket` on the first ticketed tool call in this turn, including setup or pairing verification. `continue_task` is exempt and does not consume it. After that first call, use the latest `new_codexify_ticket` returned by each response. This ticket replaces any ticket remembered from a previous turn; do not omit it or reuse an older one."));
    }
    rendered
}

fn validate_batch(batch_id: &str, messages: &[CompanionUnreadMessage]) -> Result<(), String> {
    validate_opaque(batch_id, "browser unread batch ID")?;
    if messages.is_empty() || messages.len() > MAX_UNREAD_MESSAGES {
        return Err("Browser unread batch has an invalid size".into());
    }
    let mut total = 0usize;
    let mut ids = std::collections::HashSet::new();
    for message in messages {
        validate_opaque(&message.id, "browser unread message ID")?;
        if !ids.insert(&message.id)
            || message.markdown.trim().is_empty()
            || message.markdown.contains('\0')
        {
            return Err("Browser unread batch is invalid".into());
        }
        total = total
            .checked_add(message.markdown.len())
            .ok_or("Browser unread batch is too large")?;
        if total > MAX_PROMPT_BYTES {
            return Err("Browser unread batch is too large".into());
        }
    }
    Ok(())
}

fn render_action_prompt(prompt: &ActionPrompt) -> String {
    const GOAL_CLAUSE: &str = concat!(
        "This task also has an active Codexify goal. Call `goal_read` for the current ",
        "revision. Continue autonomously until the goal is implemented and verified. Never stop at a plan, partial result, recoverable error, or because a turn or context is running out. Diagnose failures, try alternatives, and keep doing independent work. Preserve progress with rollout_update and resume through managed handoffs; respect user stops, pauses, approvals, genuine blockers, and server retry deadlines. When delegation is enabled, create agents at any stage without waiting for the Agents tab, give distinct scopes, and verify their work. If it is complete, call `goal_finish` with status `completed`, a summary of work/results/verification, and nextSteps (explicitly none if appropriate). If work ",
        "cannot proceed, call `goal_finish` with status `blocked` and explain the concrete ",
        "blocker in reason and the exact action/input/change needed in nextSteps. Both reports appear inline. After completed call `turn_completed`; after blocked call `chat_await`. Repeat waits on timeout/cancellation without input; never end the assistant turn or write a normal final answer merely because of elapsed time. Infinite goals require the identical concrete blocker to persist across three distinct goal turns spanning at least 60 seconds. Check alternatives, continue independent work, and restore progress with rollout_read after context loss. Otherwise continue working toward the goal. Do not request a separate ",
        "turn merely to evaluate goal status."
    );

    fn escape_xml_text(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    fn unread_clause(messages: &[CompanionUnreadMessage]) -> String {
        let body = messages
            .iter()
            .map(|message| format!("<message>{}</message>", escape_xml_text(&message.markdown)))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            concat!(
                "The Codexify standalone chat has unread user messages. Their Markdown is mirrored ",
                "below so you can see why you were woken. Call `chat_read` before acting; the tool ",
                "result is authoritative and advances the existing read cursor. Treat matching ",
                "messages once, not once from this mirror and again from `chat_read`.\n\n",
                "<codexify-unread>\n",
                "{}\n",
                "</codexify-unread>"
            ),
            body
        )
    }

    let rendered = match prompt {
        ActionPrompt::CreateChat {
            continuation_token,
            reference,
            setup_reference,
        } => format!(
            "{}Call `continue_task` with the non-secret `{}` to attach this new conversation to the workspace already selected by the user. Then call `automation_verify_pairing` with the non-secret `{}`. If pairing reports that creation is not ready, retry verification. Finish this initialization turn after verification; do not inspect project files or perform task work.",
            setup_reference
                .as_ref()
                .map(|reference| format!("First call `setup` with ref `{reference}`. "))
                .unwrap_or_default(),
            continuation_token,
            reference,
        ),

        ActionPrompt::PairVerify { reference } => format!(
            concat!(
                "Verify that this ChatGPT conversation is paired with the intended Codexify task by ",
                "calling `automation_verify_pairing` with the non-secret: {} ",
                "Do not inspect project files or perform task work in this verification turn."
            ),
            reference
        ),
        ActionPrompt::OfflineChat { messages, .. } => unread_clause(messages),
        ActionPrompt::OfflineGoal { .. } => GOAL_CLAUSE.to_string(),
        ActionPrompt::TurnRenewal { .. } => "Continue.".to_string(),
        ActionPrompt::TaskContinuation {} => "Continue the previous user request in this Codexify task using its existing workspace, CHAT.md and progress history. Initialization and pairing are already complete. Call get_agent_brief, read the task history and continue from the saved state; do not treat this as a new task or repeat completed work.".to_string(),
        ActionPrompt::OfflineCombined { messages, .. } => {
            format!("{}\n\n{GOAL_CLAUSE}", unread_clause(messages))
        }
    };
    format!(
        "{rendered}\n\n{}",
        crate::markdown_chat::rollout::RESTORE_GUIDANCE
    )
}

fn validate_event(
    event: &CompanionEvent,
    expected_action_id: &str,
    last_sequence: u64,
) -> Result<(), String> {
    let expected_sequence = last_sequence
        .checked_add(1)
        .ok_or("Browser companion event sequence overflow")?;
    if event.sequence != expected_sequence {
        return Err("Browser companion event sequence is not contiguous".into());
    }
    if event.action_id != expected_action_id {
        return Err("Browser companion event belongs to the wrong action".into());
    }
    match &event.kind {
        CompanionEventKind::Waiting { available_at_ms } => {
            if *available_at_ms == 0 || *available_at_ms > 9_007_199_254_740_991 {
                return Err("Browser cooldown deadline is invalid".into());
            }
        }
        CompanionEventKind::ModelVerified { model, effort } => {
            validate_opaque(model, "browser model ID")?;
            validate_opaque(effort, "browser effort ID")?;
        }
        CompanionEventKind::StatusDelta { text, .. } => {
            validate_text(text, MAX_DELTA_BYTES, "browser status delta")?;
        }
        CompanionEventKind::CommentaryDelta { markdown, .. }
        | CompanionEventKind::AnswerDelta { markdown, .. } => {
            validate_text(markdown, MAX_DELTA_BYTES, "browser Markdown delta")?;
        }
        CompanionEventKind::GenerationComplete {
            final_markdown,
            canonical_url,
        } => {
            validate_optional_text(
                final_markdown.as_deref(),
                MAX_FINAL_ANSWER_BYTES,
                "browser final answer",
            )?;
            if let Some(url) = canonical_url {
                validate_chatgpt_url(url)?;
            }
        }
        CompanionEventKind::SubmissionComplete { canonical_url }
        | CompanionEventKind::ConversationCreated { canonical_url } => {
            canonical_chatgpt_conversation_url(canonical_url)?;
        }
        CompanionEventKind::ApprovalRequired { message }
        | CompanionEventKind::RateLimited { message }
        | CompanionEventKind::ExternalInterference { message }
        | CompanionEventKind::AmbiguousSend { message } => {
            validate_optional_text(
                message.as_deref(),
                MAX_DELTA_BYTES,
                "browser attention message",
            )?;
        }
        CompanionEventKind::Failed { code, message } => {
            validate_opaque(code, "browser failure code")?;
            validate_text(message, MAX_DELTA_BYTES, "browser failure message")?;
        }
        CompanionEventKind::Queued
        | CompanionEventKind::Navigating
        | CompanionEventKind::ComposerReady
        | CompanionEventKind::ConnectorVerified
        | CompanionEventKind::SendAccepted
        | CompanionEventKind::AuthenticationRequired => {}
    }
    Ok(())
}

pub(crate) fn canonical_chatgpt_conversation_url(value: &str) -> Result<String, String> {
    validate_chatgpt_url(value)?;
    let mut url =
        Url::parse(value).map_err(|_| "ChatGPT conversation URL is invalid".to_string())?;
    let parts = url.path().split('/').collect::<Vec<_>>();
    let conversation = if (parts.len() == 3 || (parts.len() == 4 && parts[3].is_empty()))
        && parts[1] == "c"
        && !parts[2].is_empty()
    {
        Some(parts[2])
    } else if (parts.len() == 5 || (parts.len() == 6 && parts[5].is_empty()))
        && parts[1] == "g"
        && !parts[2].is_empty()
        && parts[3] == "c"
        && !parts[4].is_empty()
    {
        Some(parts[4])
    } else {
        None
    };
    if let Some(conversation) = conversation {
        let path = format!("/c/{conversation}");
        url.set_path(&path);
    }
    url.set_query(None);
    Ok(url.to_string())
}

pub(crate) fn same_chatgpt_conversation_url(left: &str, right: &str) -> bool {
    match (
        canonical_chatgpt_conversation_url(left),
        canonical_chatgpt_conversation_url(right),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn validate_chatgpt_url(value: &str) -> Result<(), String> {
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return Err("ChatGPT conversation URL is invalid".into());
    }
    let url = Url::parse(value).map_err(|_| "ChatGPT conversation URL is invalid".to_string())?;
    if url.scheme() != "https"
        || url.host_str() != Some("chatgpt.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.path() == "/"
    {
        return Err("ChatGPT conversation URL is invalid".into());
    }
    Ok(())
}

fn validate_opaque(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(format!("{label} is invalid"));
    }
    Ok(())
}

fn validate_visible(value: &str, max: usize, label: &str) -> Result<(), String> {
    if value.trim().is_empty()
        || value.len() > max
        || value.chars().any(|character| character.is_control())
    {
        return Err(format!("{label} is invalid"));
    }
    Ok(())
}

fn validate_text(value: &str, max: usize, label: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > max || value.contains('\0') {
        return Err(format!("{label} is invalid"));
    }
    Ok(())
}

fn validate_optional_text(value: Option<&str>, max: usize, label: &str) -> Result<(), String> {
    if let Some(value) = value {
        validate_text(value, max, label)?;
    }
    Ok(())
}

async fn read_bounded_response(
    mut response: reqwest::Response,
    cancellation: &CancellationToken,
    limit: usize,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err("Browser companion response is too large".into());
    }
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(cancelled_error()),
            chunk = response.chunk() => chunk
                .map_err(|_| "Browser companion response stream failed".to_string())?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err("Browser companion response is too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn status_error(status: StatusCode) -> String {
    match status {
        StatusCode::UNAUTHORIZED => "Browser companion rejected its bearer authorization".into(),
        StatusCode::NOT_FOUND => "Browser companion resource is unavailable".into(),
        StatusCode::CONFLICT => {
            "Browser companion rejected the operation because its state changed".into()
        }
        StatusCode::TOO_MANY_REQUESTS => "Browser companion is rate limited".into(),
        _ => format!("Browser companion returned HTTP status {}", status.as_u16()),
    }
}

fn cancelled_error() -> String {
    "Browser companion request was cancelled".into()
}

#[cfg(all(test, windows))]
mod process_liveness_tests {
    use super::process_is_alive;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    #[test]
    fn current_process_is_alive() {
        assert!(process_is_alive(std::process::id()));
        assert!(!process_is_alive(0));
    }

    #[test]
    fn an_exited_process_with_a_retained_handle_is_not_alive() {
        for exit_code in [0, 259] {
            let mut child = Command::new("cmd.exe")
                .args(["/D", "/C", &format!("exit {exit_code}")])
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let pid = child.id();
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            assert!(
                !handle.is_null(),
                "retain the disposable child's process object"
            );
            let _retained = unsafe { OwnedHandle::from_raw_handle(handle) };
            assert_eq!(child.wait().unwrap().code(), Some(exit_code));
            // Reproduce the old successful-OpenProcess condition explicitly.
            let exited_handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            assert!(
                !exited_handle.is_null(),
                "the exited process object must still be openable"
            );
            let _exited = unsafe { OwnedHandle::from_raw_handle(exited_handle) };
            assert!(
                !process_is_alive(pid),
                "exit code {exit_code} is not a running process"
            );
        }
    }
}

#[cfg(test)]
mod conversation_url_tests {
    use super::{canonical_chatgpt_conversation_url, same_chatgpt_conversation_url};

    #[test]
    fn chat_id_identity_ignores_project_and_gpt_routes() {
        let project = "https://chatgpt.com/g/g-p-0123456789abcdef0123456789abcdef/c/12345678-1234-4321-8765-123456789abc";
        let friendly = project.replace(
            "0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef-original-project",
        );
        let plain = "https://chatgpt.com/c/12345678-1234-4321-8765-123456789abc";
        assert_eq!(
            canonical_chatgpt_conversation_url(&friendly).unwrap(),
            plain
        );
        for alias in [
            project.to_string(),
            friendly.clone(),
            format!("{plain}/?model=gpt-6"),
            project.replace(
                "0123456789abcdef0123456789abcdef",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            "https://chatgpt.com/g/g-custom-gpt/c/12345678-1234-4321-8765-123456789abc".into(),
        ] {
            assert!(same_chatgpt_conversation_url(&alias, plain));
        }
        assert!(!same_chatgpt_conversation_url(
            &friendly,
            &project.replace("12345678-1234-4321-8765-123456789abc", "other-conversation")
        ));
        assert!(!same_chatgpt_conversation_url(
            &friendly,
            "https://example.com/c/12345678-1234-4321-8765-123456789abc"
        ));
    }
}

#[cfg(test)]
mod prompt_display_tests {
    use super::*;

    #[test]
    fn prompt_omits_delivery_ids_but_keeps_message_times_and_xml_boundaries() {
        let prompt = ActionPrompt::OfflineCombined {
            batch_id: "private-batch-identity".into(),
            messages: vec![CompanionUnreadMessage {
                id: "private-message-identity".into(),
                markdown: "[2026-10-17T14:00:00.123Z]\nKeep <data> & **all text**.".into(),
            }],
            goal_revision: 1,
        };
        let shown = render_action_prompt(&prompt);
        for internal in [
            "private-batch-identity",
            "private-message-identity",
            "batch=",
            "id=",
        ] {
            assert!(!shown.contains(internal));
        }
        assert!(shown.contains("[2026-10-17T14:00:00.123Z]"));
        assert!(shown.contains("Keep &lt;data&gt; &amp; **all text**."));
        assert!(shown.contains("<codexify-unread>\n<message>"));
        assert!(shown.contains("goal_read"));
        assert!(!shown.split("\n\n").next().unwrap().contains('\n'));
        let goal_paragraph = shown
            .split("\n\n")
            .find(|paragraph| paragraph.starts_with("This task also"))
            .unwrap();
        assert!(!goal_paragraph.contains('\n'));
        assert!(shown.contains(
            "<message>[2026-10-17T14:00:00.123Z]\nKeep &lt;data&gt; &amp; **all text**.</message>"
        ));
        let wire = serde_json::to_value(prompt).unwrap();
        assert_eq!(wire["batchId"], "private-batch-identity");
        assert_eq!(wire["messages"][0]["id"], "private-message-identity");
    }
}
