use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::markdown_chat::{MAX_AUTOMATIC_RETRY_ATTEMPTS, RetryTimings};
use crate::project_bindings::ConversationIdentity;
use crate::types::AppConfig;

const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_PRIVATE_TEXT_BYTES: usize = 64 * 1024;
const MAX_GOAL_RESULT_BYTES: usize = 4096;
pub(crate) const MAX_CHAT_MIRROR_IDS: usize = 1024;
const MAX_OWNER_GOAL_REQUESTS: usize = 32;
const STATE_FILE: &str = "automation.json";
const LOCK_FILE: &str = "automation.lock";
const CHAT_SUBMISSION_FILE: &str = "chat-submission.json";
const PAIRING_INDEX_DIR: &str = ".pairings";

pub const MAX_GOAL_BYTES: usize = 10 * 1024;
pub const PAIRING_REFERENCE_TTL_MS: u64 = 60 * 60 * 1000;
pub const PAIRING_VERIFICATION_GRACE_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Configured,
    Active,
    Completed,
    Blocked,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GoalRecord {
    pub revision: u64,
    pub markdown: String,
    pub status: GoalStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub summary: Option<String>,
    pub blocked_reason: Option<String>,
    #[serde(default)]
    pub elapsed_active_ms: u64,
    #[serde(default)]
    pub active_since_ms: Option<u64>,
    #[serde(default)]
    pub timer_until_ms: Option<u64>,
    #[serde(default = "initial_goal_turn")]
    pub turn_count: u64,
    // Historical field names; these anchor both completed and blocked reports.
    #[serde(default)]
    pub completed_chat_end: Option<u64>,
    // The durable count lives in the timeline journal. Keep automation.json
    // readable by earlier builds with strict goal-field validation.
    #[serde(default, skip_serializing)]
    pub completed_tool_call_count: Option<u64>,
}

fn initial_goal_turn() -> u64 {
    1
}

impl GoalRecord {
    fn pause_timer(&mut self, now_ms: u64) {
        if let Some(since) = self.active_since_ms.take() {
            self.elapsed_active_ms = self.elapsed_active_ms.saturating_add(
                now_ms
                    .min(self.timer_until_ms.unwrap_or(now_ms))
                    .saturating_sub(since),
            );
        }
        self.timer_until_ms = None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalFinish {
    Completed { summary: Option<String> },
    Blocked { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OwnerGoalCommand {
    Set { markdown: String },
    Run,
    Stop,
    Clear,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerGoalCommandKind {
    Set,
    Run,
    Stop,
    Clear,
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OwnerGoalCommandReceipt {
    pub kind: OwnerGoalCommandKind,
    pub goal: Option<GoalRecord>,
    pub replayed: bool,
    #[serde(default)]
    pub updated_active_goal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct OwnerGoalRequestRecord {
    request_digest: String,
    command_digest: String,
    receipt: OwnerGoalCommandReceipt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PendingPairing {
    pub generation: u64,
    pub owner_generation: u64,
    pub reference_digest: String,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct PairingState {
    pub creating_chat: bool,
    pub candidate_url: Option<String>,
    pub canonical_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_attempted_url: Option<String>,
    pub generation: u64,
    pub verified_owner_generation: Option<u64>,
    pub verified_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_sent_at_ms: Option<u64>,
    pub pending: Option<PendingPairing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connector_schema_version: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub schema_rollover_pending: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub resume_task: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingIssue {
    pub reference: String,
    pub generation: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct SelectionState {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub catalog_revision: Option<String>,
    pub account_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationActionKind {
    PairVerify,
    OfflineChat,
    OfflineGoal,
    OfflineCombined,
    TurnRenewal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutomationActionStatus {
    Unpaired,
    Unverified,
    Queued,
    Navigating,
    Sending,
    Generating,
    #[default]
    Idle,
    ApprovalRequired,
    AuthenticationRequired,
    ConversationUnavailable,
    ConversationLimit,
    RateLimited,
    Interference,
    Ambiguous,
    Stalled,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct AutomationActionState {
    pub id: Option<String>,
    pub kind: Option<AutomationActionKind>,
    pub status: AutomationActionStatus,
    pub pairing_generation: Option<u64>,
    pub goal_revision: Option<u64>,
    pub last_error: Option<String>,
    pub last_stable_answer: Option<String>,
    pub updated_at_ms: Option<u64>,
    pub goal_turn_counted: bool,
    pub send_accepted: Option<bool>,
    pub messages_fenced: bool,
    pub automatic_retry_attempts: u32,
    pub chatgpt_access_retry_attempts: u32,
    pub chatgpt_access_retry_in_progress: bool,
    // Presentation only: keep recovery visible while a scheduled retry runs.
    pub automatic_retry_in_progress: bool,
    pub automatic_retry_paused: bool,
    // Granted by the owner Retry action; retained only until this attempt starts.
    #[serde(skip_serializing_if = "is_false")]
    pub bypass_conversation_cooldown: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_until_ms: Option<u64>,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ChatMirrorState {
    pub owner_generation: Option<u64>,
    pub last_accepted_batch_id: Option<String>,
    pub message_ids: Vec<String>,
    pub transcript_revision_at_start: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct AcceptedChatBatch<'a> {
    pub owner_generation: u64,
    pub batch_id: &'a str,
    pub message_ids: &'a [String],
    pub transcript_revision: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ChatSubmission {
    version: u32,
    action_id: String,
    owner_generation: u64,
    pairing_generation: u64,
    batch_id: String,
    pub(crate) message_ids: Vec<String>,
    transcript_revision: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueFailure<'a> {
    pub action_id: &'a str,
    pub kind: AutomationActionKind,
    pub pairing_generation: u64,
    pub goal_revision: Option<u64>,
    pub message: &'a str,
    pub now_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct GoalWakeState {
    pub activity_epoch: Option<String>,
    pub activity_sequence: Option<u64>,
    pub nudged_epoch: Option<String>,
    pub nudged_sequence: Option<u64>,
    pub last_nudged_at_ms: Option<u64>,
    pub recovery_attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ApprovalResumeState {
    pub resumed_at_ms: u64,
    pub owner_generation: u64,
    pub pairing_generation: u64,
    pub goal_revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SuspendedRetryState {
    pub action: AutomationActionState,
    pub goal_wake: GoalWakeState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct AutomationTaskState {
    pub creation_request_digest: Option<String>,
    pub version: u32,
    pub pairing: PairingState,
    pub selection: SelectionState,
    pub goal: Option<GoalRecord>,
    pub chat_mirror: ChatMirrorState,
    pub action: AutomationActionState,
    pub goal_wake: GoalWakeState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_resume: Option<ApprovalResumeState>,
    // Restore the exact failure if an automatic retry is stopped while queued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suspended_retry: Option<SuspendedRetryState>,
    // Runtime policy, read from global settings on each load; never persisted per chat.
    #[serde(skip, default = "retries_enabled_by_default")]
    pub automatic_retries_enabled: bool,
    // Startup policy; changing timings requires restarting the shared browser too.
    #[serde(skip)]
    pub retry_timings: RetryTimings,
    #[serde(skip_serializing_if = "super::turn_renewal::TurnRenewalState::is_empty")]
    pub turn_renewal: super::turn_renewal::TurnRenewalState,
    #[serde(default)]
    owner_goal_requests: Vec<OwnerGoalRequestRecord>,
}

impl Default for AutomationTaskState {
    fn default() -> Self {
        Self {
            creation_request_digest: None,
            version: STATE_VERSION,
            pairing: PairingState::default(),
            selection: SelectionState::default(),
            goal: None,
            chat_mirror: ChatMirrorState::default(),
            action: AutomationActionState::default(),
            goal_wake: GoalWakeState::default(),
            approval_resume: None,
            suspended_retry: None,
            automatic_retries_enabled: true,
            retry_timings: RetryTimings::default(),
            turn_renewal: Default::default(),
            owner_goal_requests: Vec::new(),
        }
    }
}

fn retries_enabled_by_default() -> bool {
    true
}

impl AutomationTaskState {
    pub fn browser_cooldown_until_ms(&self) -> Option<u64> {
        (self.action.status == AutomationActionStatus::Queued)
            .then_some(self.action.cooldown_until_ms)
            .flatten()
    }
    pub fn can_run_queued_now(&self) -> bool {
        self.action.status == AutomationActionStatus::Queued
            && self.action.id.is_some()
            && matches!(
                self.action.kind,
                Some(
                    AutomationActionKind::OfflineChat
                        | AutomationActionKind::OfflineCombined
                        | AutomationActionKind::OfflineGoal
                        | AutomationActionKind::TurnRenewal
                )
            )
            && self.action.send_accepted == Some(false)
            && !self.action.messages_fenced
            && !self.action.goal_turn_counted
            && self.action.pairing_generation == Some(self.pairing.generation)
            && self.pairing.verified_owner_generation.is_some()
    }
    fn can_resume_cancelled_action(&self) -> bool {
        self.action.status == AutomationActionStatus::Cancelled
            && (self.action.send_accepted == Some(false) || self.action.messages_fenced)
    }

    pub fn retry_mode(&self) -> Option<&'static str> {
        action_retry_mode(self)
    }

    fn can_reconcile_chat_delivery(&self, owner_generation: u64) -> bool {
        matches!(
            self.action.kind,
            Some(AutomationActionKind::OfflineChat | AutomationActionKind::OfflineCombined)
        ) && matches!(
            self.action.status,
            AutomationActionStatus::Queued
                | AutomationActionStatus::Navigating
                | AutomationActionStatus::Sending
                | AutomationActionStatus::Generating
                | AutomationActionStatus::Ambiguous
                | AutomationActionStatus::Failed
                | AutomationActionStatus::Stalled
        ) && self.action.pairing_generation == Some(self.pairing.generation)
            && self.pairing.verified_owner_generation == Some(owner_generation)
    }

    fn offline_goal_retry_eligible(&self) -> bool {
        let Some(goal) = self.goal.as_ref() else {
            return false;
        };
        let action = &self.action;
        if goal.status != GoalStatus::Active
            || action.goal_revision != Some(goal.revision)
            || action.id.is_none()
            || action.status == AutomationActionStatus::Idle
                && self.goal_wake.last_nudged_at_ms.is_none()
            || !matches!(
                action.status,
                AutomationActionStatus::Idle
                    | AutomationActionStatus::Stalled
                    | AutomationActionStatus::Failed
                    | AutomationActionStatus::Ambiguous
                    | AutomationActionStatus::Cancelled
            )
            || !(action.kind == Some(AutomationActionKind::OfflineGoal)
                || action.kind == Some(AutomationActionKind::OfflineCombined)
                    && action.status != AutomationActionStatus::Cancelled
                    && action.send_accepted == Some(true)
                    && action.messages_fenced)
        {
            return false;
        }
        true
    }

    pub(crate) fn goal_retry_at_ms(&self) -> Option<u64> {
        if self.browser_cooldown_until_ms().is_some()
            || !self.automatic_retries_enabled
            || self.action.automatic_retry_paused
            || !self.offline_goal_retry_eligible()
        {
            return None;
        }
        let delay = if confirmed_unsent_failure(&self.action) {
            *self
                .retry_timings
                .action_delays_seconds
                .get(self.action.automatic_retry_attempts as usize)?
                * 1000
        } else {
            *self
                .retry_timings
                .goal_recovery_delays_seconds
                .get(self.goal_wake.recovery_attempts as usize)?
                * 1000
        };
        let last = self
            .action
            .updated_at_ms
            .into_iter()
            .chain(self.goal_wake.last_nudged_at_ms)
            .max()?;
        let attempt = if confirmed_unsent_failure(&self.action) {
            self.action.automatic_retry_attempts
        } else {
            self.goal_wake.recovery_attempts
        };
        Some(retry_deadline(
            &self.action,
            attempt,
            last,
            delay,
            self.retry_timings.jitter_percent,
        ))
    }

    pub(crate) fn goal_recovery_retry_at_ms(&self) -> Option<u64> {
        if self.browser_cooldown_until_ms().is_some()
            || !self.automatic_retries_enabled
            || self.action.automatic_retry_paused
        {
            return None;
        }
        let delay = self
            .retry_timings
            .goal_recovery_delays_seconds
            .get(self.goal_wake.recovery_attempts as usize)?
            * 1000;
        let last = self
            .action
            .updated_at_ms
            .into_iter()
            .chain(self.goal_wake.last_nudged_at_ms)
            .max()?;
        Some(retry_deadline(
            &self.action,
            self.goal_wake.recovery_attempts,
            last,
            delay,
            self.retry_timings.jitter_percent,
        ))
    }

    pub(crate) fn goal_recovery_exhausted(&self) -> bool {
        self.browser_cooldown_until_ms().is_none()
            && self.offline_goal_retry_eligible()
            && !confirmed_unsent_failure(&self.action)
            && self.goal_wake.recovery_attempts as usize
                >= self.retry_timings.goal_recovery_delays_seconds.len()
    }

    pub fn automatic_retry_limit(&self) -> usize {
        if self.uses_chatgpt_access_retry() {
            self.retry_timings.chatgpt_access_delays_seconds.len()
        } else if self.uses_goal_recovery() {
            self.retry_timings.goal_recovery_delays_seconds.len()
        } else {
            self.retry_timings.action_delays_seconds.len()
        }
    }

    pub fn automatic_retry_attempts(&self) -> u32 {
        if self.uses_chatgpt_access_retry() {
            self.action.chatgpt_access_retry_attempts
        } else if self.uses_goal_recovery() {
            self.goal_wake.recovery_attempts
        } else {
            self.action.automatic_retry_attempts
        }
    }

    fn uses_chatgpt_access_retry(&self) -> bool {
        chatgpt_access_failure(self.action.status)
            || self.action.chatgpt_access_retry_in_progress
                && self.action.automatic_retry_in_progress
                && matches!(
                    self.action.status,
                    AutomationActionStatus::Idle
                        | AutomationActionStatus::Queued
                        | AutomationActionStatus::Navigating
                        | AutomationActionStatus::Sending
                        | AutomationActionStatus::Generating
                )
    }

    fn chatgpt_access_retry_at_ms(&self) -> Option<u64> {
        let action = &self.action;
        if !chatgpt_access_failure(action.status)
            || action_retry_mode(self) != Some("retry")
            || action.send_accepted != Some(false)
            || action.messages_fenced
            || action.goal_turn_counted
            || action.goal_revision.is_some_and(|revision| {
                self.goal.as_ref().is_none_or(|goal| {
                    goal.status != GoalStatus::Active || goal.revision != revision
                })
            })
        {
            return None;
        }
        let delay = *self
            .retry_timings
            .chatgpt_access_delays_seconds
            .get(action.chatgpt_access_retry_attempts as usize)?
            * 1000;
        Some(retry_deadline(
            action,
            action.chatgpt_access_retry_attempts,
            action.updated_at_ms?,
            delay,
            self.retry_timings.jitter_percent,
        ))
    }

    fn uses_goal_recovery(&self) -> bool {
        self.goal
            .as_ref()
            .is_some_and(|goal| goal.status == GoalStatus::Active)
            && !confirmed_unsent_failure(&self.action)
            && matches!(
                self.action.kind,
                Some(
                    AutomationActionKind::OfflineGoal
                        | AutomationActionKind::OfflineCombined
                        | AutomationActionKind::TurnRenewal
                )
            )
    }

    pub fn approval_resume_until_ms(&self) -> Option<u64> {
        self.approval_resume.as_ref().map(|resume| {
            retry_deadline(
                &self.action,
                0,
                resume.resumed_at_ms,
                self.retry_timings.approval_resume_grace_seconds * 1000,
                self.retry_timings.jitter_percent,
            )
        })
    }

    pub fn automatic_retry_at_ms(&self) -> Option<u64> {
        if self.browser_cooldown_until_ms().is_some()
            || !self.automatic_retries_enabled
            || self.action.automatic_retry_paused
        {
            return None;
        }
        if chatgpt_access_failure(self.action.status) {
            return self.chatgpt_access_retry_at_ms();
        }
        if self.action.kind == Some(AutomationActionKind::TurnRenewal)
            && self.uses_goal_recovery()
            && self
                .turn_renewal
                .handoff
                .as_ref()
                .is_some_and(|h| h.report_complete)
            && matches!(
                self.action.status,
                AutomationActionStatus::Failed
                    | AutomationActionStatus::Ambiguous
                    | AutomationActionStatus::Stalled
            )
        {
            return self.goal_recovery_retry_at_ms();
        }
        if matches!(
            self.action.status,
            AutomationActionStatus::Failed
                | AutomationActionStatus::Ambiguous
                | AutomationActionStatus::Stalled
        ) && let Some(deadline) = self.goal_retry_at_ms()
        {
            return Some(deadline);
        }
        let action = &self.action;
        if action.status != AutomationActionStatus::Failed
            || action.send_accepted != Some(false)
            || action.messages_fenced
            || action.goal_turn_counted
            // Cancelling a queued action is an explicit user decision.
            || action.last_error.as_deref() == Some("Browser action cancelled before Send")
            || action.id.is_none()
        {
            return None;
        }
        match action.kind {
            Some(AutomationActionKind::OfflineChat | AutomationActionKind::TurnRenewal) => {}
            Some(AutomationActionKind::OfflineGoal | AutomationActionKind::OfflineCombined)
                if self.goal.as_ref().is_some_and(|goal| {
                    goal.status == GoalStatus::Active && Some(goal.revision) == action.goal_revision
                }) => {}
            _ => return None,
        }
        let delay = self
            .retry_timings
            .action_delays_seconds
            .get(action.automatic_retry_attempts as usize)?
            * 1000;
        action.updated_at_ms.map(|finished| {
            retry_deadline(
                action,
                action.automatic_retry_attempts,
                finished,
                delay,
                self.retry_timings.jitter_percent,
            )
        })
    }

    pub fn is_automatically_retrying(&self) -> bool {
        if self.browser_cooldown_until_ms().is_some() {
            return false;
        }
        self.automatic_retry_at_ms().is_some()
            || self.automatic_retries_enabled
                && !self.action.automatic_retry_paused
                && self.action.automatic_retry_in_progress
                && matches!(
                    self.action.status,
                    AutomationActionStatus::Idle
                        | AutomationActionStatus::Queued
                        | AutomationActionStatus::Navigating
                        | AutomationActionStatus::Sending
                        | AutomationActionStatus::Generating
                )
    }

    pub fn pairing_verification_wait_until_ms(&self) -> Option<u64> {
        (self.action.kind == Some(AutomationActionKind::PairVerify)
            && self.pairing.verified_owner_generation.is_none()
            && self.action.send_accepted == Some(true))
        .then_some(self.pairing.verification_sent_at_ms)
        .flatten()
        .map(|sent| sent.saturating_add(PAIRING_VERIFICATION_GRACE_MS))
    }

    pub fn action_needs_attention(&self) -> bool {
        self.action_needs_attention_at(crate::markdown_chat::now_ms())
    }

    pub fn action_needs_attention_at(&self, now_ms: u64) -> bool {
        if self.browser_cooldown_until_ms().is_some() {
            return false;
        }
        if let Some(until) = self.pairing_verification_wait_until_ms() {
            return now_ms >= until;
        }
        self.raw_action_needs_attention()
    }

    fn raw_action_needs_attention(&self) -> bool {
        matches!(
            self.action.status,
            AutomationActionStatus::Failed
                | AutomationActionStatus::Ambiguous
                | AutomationActionStatus::Stalled
                | AutomationActionStatus::Interference
                | AutomationActionStatus::AuthenticationRequired
                | AutomationActionStatus::ConversationUnavailable
                | AutomationActionStatus::ConversationLimit
                | AutomationActionStatus::RateLimited
                | AutomationActionStatus::ApprovalRequired
        ) || (self.action.status == AutomationActionStatus::Unverified
            && self.action.last_error.is_some())
    }
}

// Stable per failed action/attempt: polling and restarts must not move the
// deadline. Different random action IDs spread chats after the configured base wait.
fn retry_deadline(
    action: &AutomationActionState,
    attempt: u32,
    finished: u64,
    delay: u64,
    jitter_percent: u64,
) -> u64 {
    let mut hash = Sha256::new();
    hash.update(b"retry-jitter-v1\0");
    hash.update(action.id.as_deref().unwrap_or_default().as_bytes());
    hash.update(attempt.to_le_bytes());
    hash.update(finished.to_le_bytes());
    hash.update(delay.to_le_bytes());
    let seed = u64::from_le_bytes(hash.finalize()[..8].try_into().expect("SHA-256 prefix"));
    finished
        .saturating_add(delay)
        .saturating_add(seed % (delay.saturating_mul(jitter_percent) / 100 + 1))
}

fn confirmed_unsent_failure(action: &AutomationActionState) -> bool {
    action.status == AutomationActionStatus::Failed
        && action.send_accepted == Some(false)
        && !action.messages_fenced
        && !action.goal_turn_counted
}

fn chatgpt_access_failure(status: AutomationActionStatus) -> bool {
    matches!(
        status,
        AutomationActionStatus::AuthenticationRequired
            | AutomationActionStatus::ConversationUnavailable
            | AutomationActionStatus::RateLimited
    )
}

fn action_retry_mode(state: &AutomationTaskState) -> Option<&'static str> {
    let action = &state.action;
    if action.cooldown_until_ms.is_some()
        && action.status == AutomationActionStatus::Queued
        && action.kind != Some(AutomationActionKind::PairVerify)
        && action.send_accepted == Some(false)
        && !action.messages_fenced
        && !action.goal_turn_counted
    {
        return Some("retry");
    }
    if action.status == AutomationActionStatus::Cancelled {
        return (action.id.is_some()
            && action.kind != Some(AutomationActionKind::PairVerify)
            && action.send_accepted == Some(false))
        .then_some("retry");
    }
    if action.id.is_none()
        || action.kind == Some(AutomationActionKind::PairVerify)
        || !matches!(
            action.status,
            AutomationActionStatus::Failed
                | AutomationActionStatus::Ambiguous
                | AutomationActionStatus::Stalled
                | AutomationActionStatus::Interference
                | AutomationActionStatus::ApprovalRequired
                | AutomationActionStatus::AuthenticationRequired
                | AutomationActionStatus::ConversationUnavailable
                | AutomationActionStatus::RateLimited
        )
    {
        return None;
    }
    if action.send_accepted != Some(true)
        && (action.status == AutomationActionStatus::Failed
            || (action.send_accepted == Some(false)
                && !matches!(
                    action.status,
                    AutomationActionStatus::Ambiguous | AutomationActionStatus::Stalled
                ))
            || (action.status == AutomationActionStatus::Interference
                && action.last_error.as_deref()
                    == Some("The shared ChatGPT conversation is already generating")))
    {
        return Some("retry");
    }
    if state.goal.as_ref().is_some_and(|goal| {
        goal.status == GoalStatus::Active && action.goal_revision == Some(goal.revision)
    }) && (action.kind == Some(AutomationActionKind::OfflineGoal)
        || (action.kind == Some(AutomationActionKind::OfflineCombined)
            && action.send_accepted == Some(true)
            && action.messages_fenced))
    {
        return Some("continue_goal");
    }
    None
}

#[derive(Debug, Clone)]
pub struct AgentAutomationStore {
    offline_after_ms: u64,
    root: PathBuf,
    pairing_index_root: PathBuf,
    notifications: Option<crate::markdown_chat::NotificationsConfig>,
    notification_workspace: String,
    pub(super) renewal_config: super::turn_renewal::TurnRenewalConfig,
    pub(super) settings_path: Option<PathBuf>,
    automatic_retries: bool,
    retry_timings: RetryTimings,
    pub(super) usage_options: crate::usage_analytics::Options,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PairingIndexRecord {
    task_root: String,
    task_key: String,
}

static PROCESS_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

impl AgentAutomationStore {
    pub fn for_config(config: &AppConfig) -> Result<Self, String> {
        let root = crate::memory::task_state_dir(config).join("chats");
        let pairing_index_root = pairing_index_root(config, &root);
        Self::new_with_pairing_index(root, pairing_index_root)
            .map(|store| store.with_notifications(config))
    }

    pub fn for_state_root(config: &AppConfig, state_root: &Path) -> Result<Self, String> {
        if !state_root.is_absolute() {
            return Err("Agent automation state root must be absolute".into());
        }
        let root = state_root.join("chats");
        let pairing_index_root = pairing_index_root(config, &root);
        Self::new_with_pairing_index(root, pairing_index_root)
            .map(|store| store.with_notifications(config))
    }

    pub fn new(root: PathBuf) -> Result<Self, String> {
        let pairing_index_root = root.join(PAIRING_INDEX_DIR);
        Self::new_with_pairing_index(root, pairing_index_root)
    }

    pub fn new_with_pairing_index(
        root: PathBuf,
        pairing_index_root: PathBuf,
    ) -> Result<Self, String> {
        if !root.is_absolute() {
            return Err("Agent automation state root must be absolute".into());
        }
        if !pairing_index_root.is_absolute() {
            return Err("Agent automation pairing index root must be absolute".into());
        }
        if root.exists() {
            ensure_private_directory(&root, "Agent automation state root")?;
        }
        if pairing_index_root.exists() {
            ensure_private_directory(&pairing_index_root, "Agent automation pairing index")?;
        }
        Ok(Self {
            root,
            offline_after_ms: crate::markdown_chat::OFFLINE_AFTER_MS,
            pairing_index_root,
            notifications: None,
            notification_workspace: "Agent chat".into(),
            renewal_config: Default::default(),
            settings_path: None,
            automatic_retries: true,
            retry_timings: RetryTimings::default(),
            usage_options: Default::default(),
        })
    }

    fn with_notifications(mut self, config: &AppConfig) -> Self {
        self.offline_after_ms = config.markdown_chat.offline_after_ms();
        self.notifications = config.markdown_chat.notifications.clone();
        self.renewal_config = config.markdown_chat.turn_renewal.clone();
        self.settings_path = config.loaded_config_path.clone();
        self.automatic_retries = config.markdown_chat.automation.automatic_retries;
        self.retry_timings = config.markdown_chat.automation.retry_timings.clone();
        self.usage_options = crate::usage_analytics::Options::from_config(config);
        self.notification_workspace = config
            .work_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        self
    }

    pub(crate) async fn notify_state(
        &self,
        chat: &Arc<crate::markdown_chat::ChatFile>,
        state: &AutomationTaskState,
    ) {
        if state
            .pairing_verification_wait_until_ms()
            .is_some_and(|until| crate::markdown_chat::now_ms() < until)
        {
            return;
        }
        if let Some(config) = &self.notifications
            && let Err(error) = crate::markdown_chat::notification::notify_automation(
                chat,
                config,
                &self.notification_workspace,
                state,
            )
            .await
        {
            tracing::warn!(%error, "could not claim automation notification");
        }
    }

    pub(crate) fn connector_name(&self, fallback: &str) -> Result<Option<String>, String> {
        crate::global_settings::connector_name(self.settings_path.as_deref(), fallback)
    }

    pub(crate) fn automatic_retries_enabled(&self) -> Result<bool, String> {
        crate::global_settings::automatic_retries(
            self.settings_path.as_deref(),
            self.automatic_retries,
        )
    }

    pub fn task_directory(&self, task: &ConversationIdentity) -> Result<PathBuf, String> {
        validate_task_key(task.stable_key())?;
        Ok(self.root.join(task.stable_key()))
    }

    pub fn load(&self, task: &ConversationIdentity) -> Result<AutomationTaskState, String> {
        self.with_locked_task(task, |path| {
            let mut state = load_state(path)?;
            state.automatic_retries_enabled = self.automatic_retries_enabled()?;
            state.retry_timings = self.retry_timings.clone();
            Ok(state)
        })
    }

    pub(crate) fn offline_after_ms(&self) -> u64 {
        self.offline_after_ms
    }

    pub fn sync_goal_presence(
        &self,
        task: &ConversationIdentity,
        last_call_at_ms: Option<u64>,
        waiting_until_ms: Option<u64>,
        stopped: bool,
        now_ms: u64,
    ) -> Result<(), String> {
        self.with_locked_task(task, |path| {
            let mut state = load_state(path)?;
            let Some(goal) = state
                .goal
                .as_mut()
                .filter(|goal| goal.status == GoalStatus::Active)
            else {
                return Ok(());
            };
            let previous = goal.clone();
            let until = (!stopped)
                .then(|| {
                    last_call_at_ms
                        .map(|last| last.saturating_add(self.offline_after_ms()))
                        .into_iter()
                        .chain(waiting_until_ms)
                        .max()
                })
                .flatten();
            if let Some(old_until) = goal.timer_until_ms {
                let continuous = last_call_at_ms.is_some_and(|last| last <= old_until);
                if until.is_none()
                    || (!continuous && now_ms >= old_until)
                    || (old_until > now_ms && until.is_some_and(|until| until <= now_ms))
                {
                    goal.pause_timer(now_ms);
                }
            }
            if let Some(until) = until {
                if goal.active_since_ms.is_none() && until > now_ms {
                    goal.active_since_ms = Some(
                        last_call_at_ms
                            .unwrap_or(now_ms)
                            .max(goal.updated_at_ms)
                            .min(now_ms),
                    );
                }
                if goal.active_since_ms.is_some() {
                    goal.timer_until_ms = Some(until);
                    if until <= now_ms {
                        goal.pause_timer(now_ms);
                    }
                }
            }
            if *goal != previous {
                persist_state(path, &state)?;
            }
            Ok(())
        })
    }

    pub fn set_selection(
        &self,
        task: &ConversationIdentity,
        model: &str,
        effort: &str,
        catalog_revision: &str,
        account_fingerprint: &str,
        now_ms: u64,
    ) -> Result<SelectionState, String> {
        for (value, label) in [
            (model, "model"),
            (effort, "effort"),
            (catalog_revision, "catalog revision"),
            (account_fingerprint, "account fingerprint"),
        ] {
            if !valid_opaque(value) {
                return Err(format!("Agent automation {label} is invalid"));
            }
        }
        self.with_locked_task(task, |path| {
            let mut state = load_state(path)?;
            let account_changed = state
                .pairing
                .account_fingerprint
                .as_deref()
                .or(state.selection.account_fingerprint.as_deref())
                .is_some_and(|current| current != account_fingerprint);
            let selection_changed = state.selection.model.as_deref() != Some(model)
                || state.selection.effort.as_deref() != Some(effort)
                || state.selection.catalog_revision.as_deref() != Some(catalog_revision)
                || account_changed;
            let previous_digest = account_changed
                .then(|| {
                    state
                        .pairing
                        .pending
                        .as_ref()
                        .map(|pending| pending.reference_digest.clone())
                })
                .flatten();
            state.selection = SelectionState {
                model: Some(model.to_string()),
                effort: Some(effort.to_string()),
                catalog_revision: Some(catalog_revision.to_string()),
                account_fingerprint: Some(account_fingerprint.to_string()),
            };
            if state.pairing.account_fingerprint.is_none()
                && state.pairing.verified_owner_generation.is_some()
            {
                state.pairing.account_fingerprint = state.selection.account_fingerprint.clone();
            }
            if account_changed {
                state.pairing.pending = None;
                state.pairing.verified_owner_generation = None;
                state.pairing.verified_at_ms = None;
                state.action = AutomationActionState {
                    status: AutomationActionStatus::Unverified,
                    pairing_generation: (state.pairing.generation > 0)
                        .then_some(state.pairing.generation),
                    updated_at_ms: Some(now_ms),
                    ..Default::default()
                };
            } else if selection_changed
                && state.action.status == AutomationActionStatus::Queued
                && state.action.kind != Some(AutomationActionKind::PairVerify)
            {
                state.action = AutomationActionState::default();
                state.suspended_retry = None;
            }
            validate_state(&state)?;
            persist_state(path, &state)?;
            if let Some(previous_digest) = previous_digest {
                let _ = self.remove_pairing_index(&previous_digest);
            }
            Ok(state.selection)
        })
    }

    pub fn clear_selection(&self, task: &ConversationIdentity) -> Result<SelectionState, String> {
        self.mutate(task, |state| {
            if state.pairing.account_fingerprint.is_none() {
                state.pairing.account_fingerprint = state.selection.account_fingerprint.clone();
            }
            state.selection = SelectionState::default();
            if state.action.status == AutomationActionStatus::Queued
                && state.action.kind != Some(AutomationActionKind::PairVerify)
            {
                state.action = AutomationActionState::default();
            }
            Ok(state.selection.clone())
        })
    }

    pub fn bind_pairing_account(
        &self,
        task: &ConversationIdentity,
        generation: u64,
        fingerprint: &str,
    ) -> Result<(), String> {
        if !valid_opaque(fingerprint) {
            return Err("Agent automation account fingerprint is invalid".into());
        }
        self.mutate(task, |state| {
            if state.pairing.generation != generation || state.pairing.pending.is_none() {
                return Err("The agent-chat pairing request is stale".into());
            }
            state.pairing.account_fingerprint = Some(fingerprint.to_owned());
            if state
                .selection
                .account_fingerprint
                .as_deref()
                .is_some_and(|saved| saved != fingerprint)
            {
                state.selection = SelectionState::default();
            }
            Ok(())
        })
    }

    pub fn set_goal(
        &self,
        task: &ConversationIdentity,
        markdown: &str,
        now_ms: u64,
    ) -> Result<GoalRecord, String> {
        validate_goal_markdown(markdown)?;
        self.mutate(task, |state| {
            let revision = state.goal.as_ref().map_or(Ok(1), |goal| {
                goal.revision
                    .checked_add(1)
                    .ok_or("Agent-chat goal revision overflow")
            })?;
            let goal = GoalRecord {
                revision,
                markdown: markdown.to_string(),
                status: GoalStatus::Configured,
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
                finished_at_ms: None,
                summary: None,
                blocked_reason: None,
                elapsed_active_ms: 0,
                active_since_ms: None,
                timer_until_ms: None,
                turn_count: 1,
                completed_chat_end: None,
                completed_tool_call_count: None,
            };
            state.goal = Some(goal.clone());
            state.goal_wake = GoalWakeState::default();
            if state.action.goal_revision.is_some() {
                state.action = AutomationActionState::default();
            }
            Ok(goal)
        })
    }

    pub fn run_goal(
        &self,
        task: &ConversationIdentity,
        expected_revision: u64,
        now_ms: u64,
    ) -> Result<GoalRecord, String> {
        self.mutate(task, |state| start_goal(state, expected_revision, now_ms))
    }

    pub fn stop_goal(
        &self,
        task: &ConversationIdentity,
        expected_revision: u64,
        now_ms: u64,
    ) -> Result<GoalRecord, String> {
        self.mutate(task, |state| {
            let result = {
                let goal = exact_goal_mut(state, expected_revision)?;
                match goal.status {
                    GoalStatus::Active => {
                        goal.pause_timer(now_ms);
                        goal.status = GoalStatus::Configured;
                        goal.updated_at_ms = now_ms;
                        goal.finished_at_ms = None;
                        goal.summary = None;
                        goal.blocked_reason = None;
                    }
                    GoalStatus::Configured => {}
                    GoalStatus::Completed | GoalStatus::Blocked | GoalStatus::Error => {
                        return Err("This agent-chat goal is already terminal".into());
                    }
                }
                goal.clone()
            };
            state.goal_wake = GoalWakeState::default();
            state.turn_renewal = Default::default();
            if state.action.goal_revision == Some(expected_revision) {
                state.action = AutomationActionState::default();
            }
            Ok(result)
        })
    }

    pub fn clear_goal(
        &self,
        task: &ConversationIdentity,
        expected_revision: u64,
    ) -> Result<(), String> {
        self.mutate(task, |state| {
            let goal = exact_goal(state, expected_revision)?;
            if goal.status == GoalStatus::Active {
                return Err("Stop the active agent-chat goal before clearing it".into());
            }
            state.goal = None;
            state.goal_wake = GoalWakeState::default();
            if state.action.goal_revision == Some(expected_revision) {
                state.action = AutomationActionState::default();
            }
            Ok(())
        })
    }

    pub fn apply_owner_goal_command(
        &self,
        task: &ConversationIdentity,
        request_id: &str,
        command: OwnerGoalCommand,
        now_ms: u64,
    ) -> Result<OwnerGoalCommandReceipt, String> {
        self.owner_goal_command(task, request_id, command, now_ms, false, None)
    }

    /// Agent mutations share the owner's activation rules and durable receipts,
    /// but must compare the revision under the same lock as the mutation.
    pub fn apply_agent_goal_command(
        &self,
        task: &ConversationIdentity,
        request_id: &str,
        expected_revision: u64,
        command: OwnerGoalCommand,
        now_ms: u64,
    ) -> Result<OwnerGoalCommandReceipt, String> {
        if !valid_opaque(request_id) {
            return Err("Agent-chat goal command request ID is invalid".into());
        }
        if !matches!(
            command,
            OwnerGoalCommand::Set { .. } | OwnerGoalCommand::Run
        ) {
            return Err("Agents can only set or resume a goal with this operation".into());
        }
        // Keep agent retry identifiers separate from owner UI request identifiers.
        let request_id = format!("agent-goal-{:x}", Sha256::digest(request_id.as_bytes()));
        self.owner_goal_command(
            task,
            &request_id,
            command,
            now_ms,
            false,
            Some(expected_revision),
        )
    }

    pub fn pause_goal_for_agent_stop(
        &self,
        task: &ConversationIdentity,
        request_id: &str,
        now_ms: u64,
    ) -> Result<OwnerGoalCommandReceipt, String> {
        self.owner_goal_command(task, request_id, OwnerGoalCommand::Stop, now_ms, true, None)
    }

    fn owner_goal_command(
        &self,
        task: &ConversationIdentity,
        request_id: &str,
        command: OwnerGoalCommand,
        now_ms: u64,
        agent_stop: bool,
        expected_revision: Option<u64>,
    ) -> Result<OwnerGoalCommandReceipt, String> {
        if !valid_opaque(request_id) {
            return Err("Agent-chat goal command request ID is invalid".into());
        }
        if let OwnerGoalCommand::Set { markdown } = &command {
            validate_goal_markdown(markdown)?;
        }
        if matches!(command, OwnerGoalCommand::Status) {
            return Ok(OwnerGoalCommandReceipt {
                kind: OwnerGoalCommandKind::Status,
                goal: self.load(task)?.goal,
                replayed: false,
                updated_active_goal: false,
            });
        }
        let request_digest = owner_goal_request_digest(request_id);
        let command_digest = if let Some(revision) = expected_revision {
            format!(
                "{:x}",
                Sha256::digest(format!(
                    "agent-goal-v1\0{revision}\0{}",
                    owner_goal_command_digest(&command)?
                ))
            )
        } else if agent_stop {
            format!("{:x}", Sha256::digest(b"pause_goal_for_agent_stop"))
        } else {
            owner_goal_command_digest(&command)?
        };
        self.mutate(task, move |state| {
            if let Some(record) = state
                .owner_goal_requests
                .iter()
                .find(|record| record.request_digest == request_digest)
            {
                if record.command_digest != command_digest {
                    return Err(
                        "This goal command request ID already belongs to a different command".into(),
                    );
                }
                let mut receipt = record.receipt.clone();
                receipt.replayed = true;
                return Ok(receipt);
            }

            if let Some(expected_revision) = expected_revision {
                let current_revision = state.goal.as_ref().map_or(0, |goal| goal.revision);
                if current_revision != expected_revision {
                    return Err(format!(
                        "Agent-chat goal revision changed (expected {expected_revision}, current {current_revision}); call goal_read before changing it"
                    ));
                }
            }

            let mut updated_active_goal = false;
            let (kind, goal) = match command {
                OwnerGoalCommand::Set { markdown } => {
                    let revision = state.goal.as_ref().map_or(Ok(1), |goal| {
                        goal.revision
                            .checked_add(1)
                            .ok_or("Agent-chat goal revision overflow")
                    })?;
                    let previous = state
                        .goal
                        .as_ref()
                        .filter(|goal| goal.status == GoalStatus::Active);
                    updated_active_goal = previous.is_some();
                    let goal = GoalRecord {
                        revision,
                        markdown,
                        status: GoalStatus::Active,
                        created_at_ms: previous.map_or(now_ms, |goal| goal.created_at_ms),
                        updated_at_ms: now_ms,
                        finished_at_ms: None,
                        summary: None,
                        blocked_reason: None,
                        elapsed_active_ms: previous.map_or(0, |goal| goal.elapsed_active_ms),
                        active_since_ms: previous.and_then(|goal| goal.active_since_ms),
                        timer_until_ms: previous.and_then(|goal| goal.timer_until_ms),
                        turn_count: previous.map_or(1, |goal| goal.turn_count.max(1)),
                        completed_chat_end: None,
                        completed_tool_call_count: None,
                    };
                    state.goal = Some(goal.clone());
                    state.goal_wake = GoalWakeState::default();
                    if state.action.goal_revision.is_some() {
                        state.action = AutomationActionState::default();
                    }
                    (OwnerGoalCommandKind::Set, Some(goal))
                }
                OwnerGoalCommand::Run => {
                    let revision = state
                        .goal
                        .as_ref()
                        .ok_or("No agent-chat goal is configured")?
                        .revision;
                    let goal = start_goal(state, revision, now_ms)?;
                    (OwnerGoalCommandKind::Run, Some(goal))
                }
                OwnerGoalCommand::Stop
                    if agent_stop
                        && state
                            .goal
                            .as_ref()
                            .is_none_or(|goal| goal.status != GoalStatus::Active) =>
                {
                    (OwnerGoalCommandKind::Stop, state.goal.clone())
                }
                OwnerGoalCommand::Stop => {
                    let revision = state
                        .goal
                        .as_ref()
                        .ok_or("No agent-chat goal is configured")?
                        .revision;
                    let goal = exact_goal_mut(state, revision)?;
                    match goal.status {
                        GoalStatus::Active => {
                            goal.pause_timer(now_ms);
                            goal.status = GoalStatus::Configured;
                            goal.updated_at_ms = now_ms;
                            goal.finished_at_ms = None;
                            goal.summary = None;
                            goal.blocked_reason = None;
                        }
                        GoalStatus::Configured => {}
                        GoalStatus::Completed | GoalStatus::Blocked | GoalStatus::Error => {
                            return Err("This agent-chat goal is already terminal".into());
                        }
                    }
                    let goal = goal.clone();
                    state.goal_wake = GoalWakeState::default();
                    if state.action.goal_revision == Some(revision) {
                        state.action = AutomationActionState::default();
                    }
                    (OwnerGoalCommandKind::Stop, Some(goal))
                }
                OwnerGoalCommand::Clear => {
                    if let Some(goal) = state.goal.as_ref()
                        && goal.status == GoalStatus::Active
                    {
                        return Err("Stop the active agent-chat goal before clearing it".into());
                    }
                    let revision = state.goal.as_ref().map(|goal| goal.revision);
                    state.goal = None;
                    state.goal_wake = GoalWakeState::default();
                    if revision.is_some() && state.action.goal_revision == revision {
                        state.action = AutomationActionState::default();
                    }
                    (OwnerGoalCommandKind::Clear, None)
                }
                OwnerGoalCommand::Status => unreachable!("status is handled before mutation"),
            };
            let receipt = OwnerGoalCommandReceipt {
                kind,
                goal,
                replayed: false,
                updated_active_goal,
            };
            state.owner_goal_requests.push(OwnerGoalRequestRecord {
                request_digest,
                command_digest,
                receipt: receipt.clone(),
            });
            if state.owner_goal_requests.len() > MAX_OWNER_GOAL_REQUESTS {
                state.owner_goal_requests.remove(0);
            }
            Ok(receipt)
        })
    }

    pub fn finish_goal(
        &self,
        task: &ConversationIdentity,
        expected_revision: u64,
        finish: GoalFinish,
        now_ms: u64,
    ) -> Result<GoalRecord, String> {
        self.finish_goal_with_tool_count(task, expected_revision, finish, now_ms, None)
    }

    pub fn finish_goal_with_tool_count(
        &self,
        task: &ConversationIdentity,
        expected_revision: u64,
        finish: GoalFinish,
        now_ms: u64,
        tool_call_count: Option<u64>,
    ) -> Result<GoalRecord, String> {
        let normalized = normalize_goal_finish(finish)?;
        let transcript = self.task_directory(task)?.join("CHAT.md");
        let result = self.mutate(task, |state| {
            let result = {
                let goal = exact_goal_mut(state, expected_revision)?;
                if terminal_finish_matches(goal, &normalized) {
                    return Ok(goal.clone());
                }
                if goal.status != GoalStatus::Active {
                    return Err("Only an active agent-chat goal can be finished".into());
                }
                goal.pause_timer(now_ms);
                match &normalized {
                    GoalFinish::Completed { summary } => {
                        goal.status = GoalStatus::Completed;
                        goal.summary = summary.clone();
                        goal.blocked_reason = None;
                    }
                    GoalFinish::Blocked { reason } => {
                        goal.status = GoalStatus::Blocked;
                        goal.summary = None;
                        goal.blocked_reason = Some(reason.clone());
                    }
                }
                goal.completed_tool_call_count = tool_call_count;
                goal.completed_chat_end = std::fs::metadata(&transcript)
                    .ok()
                    .filter(|metadata| metadata.is_file())
                    .map(|metadata| metadata.len());
                goal.updated_at_ms = now_ms;
                goal.finished_at_ms = Some(now_ms);
                goal.clone()
            };
            state.goal_wake = GoalWakeState::default();
            if state.action.goal_revision == Some(expected_revision) {
                state.action.status = AutomationActionStatus::Idle;
                state.action.automatic_retry_in_progress = false;
                state.action.chatgpt_access_retry_in_progress = false;
                state.action.updated_at_ms = Some(now_ms);
            }
            Ok(result)
        });
        if let Ok(goal) = &result {
            crate::usage_analytics::goal(
                &self.usage_options,
                self.root.join(task.stable_key()).join("CHAT.md"),
                goal,
            );
        }
        result
    }

    pub fn issue_pairing(
        &self,
        task: &ConversationIdentity,
        candidate_url: &str,
        owner_generation: u64,
        now_ms: u64,
    ) -> Result<PairingIssue, String> {
        self.issue_pairing_guarded(task, candidate_url, owner_generation, now_ms, None, None)?
            .ok_or_else(|| "Pairing changed before it could be prepared".into())
    }

    pub(crate) fn issue_rollover_pairing(
        &self,
        task: &ConversationIdentity,
        candidate_url: &str,
        owner_generation: u64,
        now_ms: u64,
        expected: &AutomationActionState,
    ) -> Result<Option<PairingIssue>, String> {
        self.issue_pairing_guarded(
            task,
            candidate_url,
            owner_generation,
            now_ms,
            Some(expected),
            None,
        )
    }

    pub(crate) fn issue_schema_rollover_pairing(
        &self,
        task: &ConversationIdentity,
        candidate_url: &str,
        owner_generation: u64,
        now_ms: u64,
        expected: &AutomationTaskState,
    ) -> Result<Option<PairingIssue>, String> {
        self.issue_pairing_guarded(
            task,
            candidate_url,
            owner_generation,
            now_ms,
            Some(&expected.action),
            Some(&expected.pairing),
        )
    }

    pub(crate) fn hold_messages_for_schema_rollover(
        &self,
        task: &ConversationIdentity,
        pairing: &PairingState,
    ) -> Result<bool, String> {
        self.mutate(task, |state| {
            if state.pairing != *pairing || state.pairing.verified_owner_generation.is_none() {
                return Ok(false);
            }
            state.pairing.schema_rollover_pending = true;
            Ok(true)
        })
    }

    pub(crate) fn clear_schema_rollover_hold(
        &self,
        task: &ConversationIdentity,
        pairing: &PairingState,
    ) -> Result<(), String> {
        self.mutate(task, |state| {
            if state.pairing == *pairing {
                state.pairing.schema_rollover_pending = false;
            }
            Ok(())
        })
    }

    pub(crate) fn record_creation_schema(
        &self,
        task: &ConversationIdentity,
        action_id: &str,
        generation: u64,
        version: &str,
    ) -> Result<bool, String> {
        self.mutate(task, |state| {
            if !state.pairing.creating_chat
                || state.pairing.generation != generation
                || state.action.id.as_deref() != Some(action_id)
            {
                return Ok(false);
            }
            state.pairing.connector_schema_version = Some(version.into());
            Ok(true)
        })
    }

    fn issue_pairing_guarded(
        &self,
        task: &ConversationIdentity,
        candidate_url: &str,
        owner_generation: u64,
        now_ms: u64,
        expected: Option<&AutomationActionState>,
        expected_pairing: Option<&PairingState>,
    ) -> Result<Option<PairingIssue>, String> {
        let candidate_url = normalize_chatgpt_url(candidate_url)?;
        let reference = generate_reference()?;
        let digest = reference_digest(&reference);
        let task_key = task.stable_key().to_string();
        self.with_locked_task(task, |path| {
            let mut state = load_state(path)?;
            if let Some(expected) = expected
                && (state.action != *expected
                    || expected_pairing.map_or(
                        state.action.status != AutomationActionStatus::ConversationLimit,
                        |pairing| state.pairing != *pairing,
                    )
                    // Schema rollover has a full pairing snapshot; an idle or
                    // cleared action need not carry its pairing generation.
                    || state.pairing.generation
                        != expected_pairing
                            .map(|pairing| pairing.generation)
                            .or(expected.pairing_generation)
                            .unwrap_or(0)
                    || state.pairing.verified_owner_generation != owner_generation.checked_sub(1))
            {
                return Ok(None);
            }
            let previous_digest = state
                .pairing
                .pending
                .as_ref()
                .map(|pending| pending.reference_digest.clone());
            let generation = state
                .pairing
                .generation
                .checked_add(1)
                .ok_or("Agent-chat pairing generation overflow")?;
            let expires_at_ms = now_ms
                .checked_add(PAIRING_REFERENCE_TTL_MS)
                .ok_or("Agent-chat pairing expiry overflow")?;
            state.pairing = PairingState {
                creating_chat: expected.is_some(),
                last_attempted_url: conversation_url_for_retry(&candidate_url).map(str::to_owned),
                candidate_url: Some(candidate_url),
                canonical_url: None,
                generation,
                verified_owner_generation: None,
                verified_at_ms: None,
                verification_sent_at_ms: None,
                account_fingerprint: state
                    .selection
                    .account_fingerprint
                    .clone()
                    .or(state.pairing.account_fingerprint.clone()),
                connector_schema_version: None,
                schema_rollover_pending: state.pairing.schema_rollover_pending
                    && expected.is_some(),
                resume_task: expected.is_some()
                    && (state.pairing.resume_task
                        || state.action.kind == Some(AutomationActionKind::TurnRenewal)),
                pending: Some(PendingPairing {
                    generation,
                    owner_generation,
                    reference_digest: digest.clone(),
                    expires_at_ms,
                }),
            };
            state.action = AutomationActionState {
                kind: Some(AutomationActionKind::PairVerify),
                status: AutomationActionStatus::Unverified,
                pairing_generation: Some(generation),
                updated_at_ms: Some(now_ms),
                send_accepted: expected.is_some().then_some(false),
                ..Default::default()
            };
            if expected.is_some() {
                state.turn_renewal = Default::default();
                // A nudge accepted by the exhausted source cannot suppress the
                // active goal's first wake in its replacement conversation.
                state.goal_wake = Default::default();
            }
            validate_state(&state)?;
            persist_state(path, &state)?;
            self.publish_pairing_index(&digest, &task_key)?;
            if let Some(previous_digest) = previous_digest
                && previous_digest != digest
            {
                let _ = self.remove_pairing_index(&previous_digest);
            }
            Ok(Some(PairingIssue {
                reference,
                generation,
                expires_at_ms,
            }))
        })
    }

    pub fn unlink_pairing(
        &self,
        task: &ConversationIdentity,
        now_ms: u64,
    ) -> Result<PairingState, String> {
        self.with_locked_task(task, |path| {
            let mut state = load_state(path)?;
            let previous_digest = state
                .pairing
                .pending
                .as_ref()
                .map(|pending| pending.reference_digest.clone());
            let generation = state
                .pairing
                .generation
                .checked_add(1)
                .ok_or("Agent-chat pairing generation overflow")?;
            state.pairing = PairingState {
                generation,
                last_attempted_url: state.pairing.last_attempted_url.take(),
                ..Default::default()
            };
            state.action = AutomationActionState {
                status: AutomationActionStatus::Unpaired,
                pairing_generation: Some(generation),
                updated_at_ms: Some(now_ms),
                ..Default::default()
            };
            validate_state(&state)?;
            persist_state(path, &state)?;
            if let Some(previous_digest) = previous_digest {
                let _ = self.remove_pairing_index(&previous_digest);
            }
            Ok(state.pairing)
        })
    }

    pub(crate) fn activate_queued_pairing(
        &self,
        task: &ConversationIdentity,
        action_id: &str,
        generation: u64,
        owner_generation: u64,
        reference: &str,
        now_ms: u64,
    ) -> Result<PairingState, String> {
        validate_reference(reference)?;
        let digest = reference_digest(reference);
        self.mutate(task, |state| {
            if state.action.id.as_deref() != Some(action_id)
                || state.action.kind != Some(AutomationActionKind::PairVerify)
                || state.action.status != AutomationActionStatus::Queued
                || state.pairing.generation != generation
            {
                return Err(invalid_pairing_reference());
            }
            let pending = state
                .pairing
                .pending
                .as_mut()
                .ok_or_else(invalid_pairing_reference)?;
            if pending.generation != generation
                || pending.owner_generation != owner_generation
                || pending.reference_digest != digest
            {
                return Err(invalid_pairing_reference());
            }
            pending.expires_at_ms = now_ms
                .checked_add(PAIRING_REFERENCE_TTL_MS)
                .ok_or("Pairing expiry overflow")?;
            Ok(state.pairing.clone())
        })
    }

    pub fn record_creation_request(
        &self,
        task: &ConversationIdentity,
        digest: &str,
    ) -> Result<(), String> {
        self.mutate(task, |state| {
            if state
                .creation_request_digest
                .as_deref()
                .is_some_and(|previous| previous != digest)
            {
                return Err("New chat request ID belongs to different options".into());
            }
            state.creation_request_digest = Some(digest.to_owned());
            Ok(())
        })
    }

    pub fn record_creation_send(
        &self,
        task: &ConversationIdentity,
        action_id: &str,
        accepted: bool,
    ) -> Result<(), String> {
        self.mutate(task, |state| {
            if state.action.id.as_deref() == Some(action_id) {
                state.action.send_accepted =
                    Some(state.action.send_accepted == Some(true) || accepted);
            }
            Ok(())
        })
    }

    pub fn record_pairing_submission(
        &self,
        task: &ConversationIdentity,
        action_id: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.mutate(task, |state| {
            if state.action.id.as_deref() == Some(action_id)
                && state.action.kind == Some(AutomationActionKind::PairVerify)
            {
                state.action.send_accepted = Some(true);
                state.pairing.verification_sent_at_ms.get_or_insert(now_ms);
            }
            Ok(())
        })
    }

    pub fn mark_creating_chat(&self, task: &ConversationIdentity) -> Result<(), String> {
        self.mutate(task, |state| {
            state.pairing.creating_chat = true;
            state.action.send_accepted = Some(false);
            Ok(())
        })
    }

    pub(crate) fn fail_creation_before_send(
        &self,
        task: &ConversationIdentity,
        generation: u64,
        message: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.mutate(task, |state| {
            if state.pairing.generation == generation
                && state.pairing.creating_chat
                && state.action.send_accepted != Some(true)
            {
                state.action.status = AutomationActionStatus::Unverified;
                state.action.send_accepted = Some(false);
                state.action.last_error = Some(message.to_owned());
                state.action.updated_at_ms = Some(now_ms);
            }
            Ok(())
        })
    }

    pub fn record_pairing_canonical_url(
        &self,
        task: &ConversationIdentity,
        expected_generation: u64,
        canonical_url: &str,
        now_ms: u64,
    ) -> Result<PairingState, String> {
        let canonical_url = normalize_chatgpt_url(canonical_url)?;
        if conversation_url_for_retry(&canonical_url).is_none() {
            return Err("Pairing requires a ChatGPT conversation URL, not a project URL".into());
        }
        self.mutate_persisting_errors(task, |state| {
            if state.pairing.generation != expected_generation {
                return Err("The agent-chat pairing request is stale".into());
            }
            // A failed or expired verification does not erase the known chat identity.
            state.pairing.last_attempted_url = Some(canonical_url.clone());
            state.pairing.canonical_url = Some(canonical_url);
            if let Some(pending) = state.pairing.pending.as_ref() {
                if pending.generation != expected_generation {
                    return Err("The agent-chat pairing request is stale".into());
                }
                if now_ms > pending.expires_at_ms {
                    state.pairing.pending = None;
                    return Err("The agent-chat pairing reference expired".into());
                }
            }
            Ok(state.pairing.clone())
        })
    }

    pub fn validate_pending_pairing(
        &self,
        task: &ConversationIdentity,
        expected_generation: u64,
        owner_generation: u64,
        reference: &str,
        now_ms: u64,
    ) -> Result<PairingState, String> {
        validate_reference(reference)?;
        let digest = reference_digest(reference);
        self.mutate_persisting_errors(task, |state| {
            let Some(pending) = state.pairing.pending.as_ref() else {
                return Err(invalid_pairing_reference());
            };
            if now_ms > pending.expires_at_ms {
                state.pairing.pending = None;
                return Err(invalid_pairing_reference());
            }
            if state.pairing.generation != expected_generation
                || pending.generation != expected_generation
                || pending.owner_generation != owner_generation
                || pending.reference_digest != digest
                || state.pairing.candidate_url.is_none()
            {
                return Err(invalid_pairing_reference());
            }
            Ok(state.pairing.clone())
        })
    }

    pub fn verify_pairing(
        &self,
        task: &ConversationIdentity,
        expected_generation: u64,
        owner_generation: u64,
        reference: &str,
        now_ms: u64,
    ) -> Result<PairingState, String> {
        validate_reference(reference)?;
        let supplied_digest = reference_digest(reference);
        let result = self.mutate_persisting_errors(task, |state| {
            let Some(pending) = state.pairing.pending.clone() else {
                return Err(
                    "The agent-chat pairing reference is invalid, expired, or already used".into(),
                );
            };
            if now_ms > pending.expires_at_ms {
                state.pairing.pending = None;
                return Err(
                    "The agent-chat pairing reference is invalid, expired, or already used".into(),
                );
            }
            let valid = state.pairing.generation == expected_generation
                && pending.generation == expected_generation
                && pending.owner_generation == owner_generation
                && pending.reference_digest == supplied_digest
                && state.pairing.canonical_url.is_some();
            if !valid {
                state.pairing.pending = None;
                return Err(
                    "The agent-chat pairing reference is invalid, expired, or already used".into(),
                );
            }
            state.pairing.pending = None;
            state.pairing.creating_chat = false;
            state.pairing.candidate_url = None;
            state.pairing.verified_owner_generation = Some(owner_generation);
            state.pairing.verified_at_ms = Some(now_ms);
            state.pairing.schema_rollover_pending = false;
            state.action.status = AutomationActionStatus::Idle;
            state.action.last_error = None;
            state.action.automatic_retry_attempts = 0;
            state.action.chatgpt_access_retry_attempts = 0;
            state.action.chatgpt_access_retry_in_progress = false;
            state.action.automatic_retry_in_progress = false;
            state.action.send_accepted = Some(true);
            state.action.updated_at_ms = Some(now_ms);
            Ok(state.pairing.clone())
        });
        let _ = self.remove_pairing_index(&supplied_digest);
        result
    }

    pub fn verify_pairing_reference(
        &self,
        actual_task: &ConversationIdentity,
        owner_generation: u64,
        reference: &str,
        now_ms: u64,
    ) -> Result<PairingState, String> {
        validate_reference(reference)?;
        let supplied_digest = reference_digest(reference);
        let index = self.read_pairing_index(&supplied_digest)?;
        let intended_root = PathBuf::from(&index.task_root);
        let intended =
            Self::new_with_pairing_index(intended_root, self.pairing_index_root.clone())?;
        let actual_matches = actual_task.stable_key() == index.task_key;
        let outcome = intended.with_locked_task_key(&index.task_key, |path| {
            let mut state = load_state(path)?;
            let Some(pending) = state.pairing.pending.clone() else {
                return Ok(PairingReferenceOutcome::Rejected);
            };
            if pending.reference_digest != supplied_digest {
                return Ok(PairingReferenceOutcome::Rejected);
            }
            if state.pairing.creating_chat
                && now_ms <= pending.expires_at_ms
                && actual_matches
                && pending.owner_generation == owner_generation
                && state.pairing.canonical_url.is_none()
            {
                return Err(
                    "ChatGPT conversation creation is not ready; retry pairing verification shortly"
                        .into(),
                );
            }
            let valid = now_ms <= pending.expires_at_ms
                && actual_matches
                && state.pairing.generation == pending.generation
                && pending.owner_generation == owner_generation
                && state.pairing.canonical_url.is_some();
            if !valid {
                state.pairing.pending = None;
                validate_state(&state)?;
                persist_state(path, &state)?;
                return Ok(PairingReferenceOutcome::Rejected);
            }
            state.pairing.pending = None;
            state.pairing.creating_chat = false;
            state.pairing.candidate_url = None;
            state.pairing.verified_owner_generation = Some(owner_generation);
            state.pairing.verified_at_ms = Some(now_ms);
            state.pairing.schema_rollover_pending = false;
            state.action.status = AutomationActionStatus::Idle;
            state.action.last_error = None;
            state.action.automatic_retry_attempts = 0;
            state.action.chatgpt_access_retry_attempts = 0;
            state.action.chatgpt_access_retry_in_progress = false;
            state.action.automatic_retry_in_progress = false;
            state.action.send_accepted = Some(true);
            state.action.updated_at_ms = Some(now_ms);
            validate_state(&state)?;
            persist_state(path, &state)?;
            Ok(PairingReferenceOutcome::Verified(Box::new(
                state.pairing.clone(),
            )))
        })?;
        let _ = self.remove_pairing_index(&supplied_digest);
        match outcome {
            PairingReferenceOutcome::Verified(pairing) => Ok(*pairing),
            PairingReferenceOutcome::Rejected => Err(invalid_pairing_reference()),
        }
    }

    pub fn invalidate_pairing(
        &self,
        task: &ConversationIdentity,
        expected_generation: u64,
        now_ms: u64,
    ) -> Result<PairingState, String> {
        self.with_locked_task(task, |path| {
            let mut state = load_state(path)?;
            // Verification can win the race with browser failure cleanup.
            // A stale failure must not revoke that proof or a newer attempt.
            if state.pairing.generation != expected_generation
                || state.pairing.verified_owner_generation.is_some()
            {
                return Ok(state.pairing);
            }
            let previous_digest = state
                .pairing
                .pending
                .as_ref()
                .map(|pending| pending.reference_digest.clone());
            state.pairing.pending = None;
            state.pairing.verified_owner_generation = None;
            state.pairing.verified_at_ms = None;
            state.action.status = AutomationActionStatus::Unverified;
            state.pairing.connector_schema_version = None;
            state.pairing.schema_rollover_pending = false;
            state.action.updated_at_ms = Some(now_ms);
            validate_state(&state)?;
            persist_state(path, &state)?;
            if let Some(previous_digest) = previous_digest {
                let _ = self.remove_pairing_index(&previous_digest);
            }
            Ok(state.pairing)
        })
    }

    pub(crate) fn invalidate_pairing_and_supersede_action(
        &self,
        task: &ConversationIdentity,
        now_ms: u64,
    ) -> Result<PairingState, String> {
        self.with_locked_task(task, |path| {
            let mut state = load_state(path)?;
            let previous_digest = state
                .pairing
                .pending
                .as_ref()
                .map(|pending| pending.reference_digest.clone());
            state.pairing.pending = None;
            state.pairing.verified_owner_generation = None;
            state.pairing.verified_at_ms = None;
            state.action = AutomationActionState {
                status: AutomationActionStatus::Unverified,
                pairing_generation: (state.pairing.generation > 0)
                    .then_some(state.pairing.generation),
                updated_at_ms: Some(now_ms),
                ..Default::default()
            };
            state.pairing.connector_schema_version = None;
            state.pairing.schema_rollover_pending = false;
            validate_state(&state)?;
            persist_state(path, &state)?;
            if let Some(previous_digest) = previous_digest {
                let _ = self.remove_pairing_index(&previous_digest);
            }
            Ok(state.pairing)
        })
    }

    pub(crate) fn record_interrupted_pairing(
        &self,
        task: &ConversationIdentity,
    ) -> Result<(), String> {
        self.mutate(task, |state| {
            state.action.last_error =
                Some("Pairing verification was interrupted; pair this conversation again".into());
            Ok(())
        })
    }

    pub fn queue_action(
        &self,
        task: &ConversationIdentity,
        action_id: &str,
        kind: AutomationActionKind,
        pairing_generation: u64,
        goal_revision: Option<u64>,
        now_ms: u64,
    ) -> Result<AutomationActionState, String> {
        if !valid_opaque(action_id) || pairing_generation == 0 {
            return Err("Agent automation queued action is invalid".into());
        }
        self.mutate(task, |state| {
            if state.pairing.generation != pairing_generation {
                return Err("Agent automation pairing changed before the action was queued".into());
            }
            if kind == AutomationActionKind::PairVerify {
                if state
                    .pairing
                    .pending
                    .as_ref()
                    .is_none_or(|pending| pending.generation != pairing_generation)
                    || state.pairing.candidate_url.is_none()
                {
                    return Err("Agent automation pairing request is no longer pending".into());
                }
            } else if state.pairing.canonical_url.is_none()
                || state.pairing.verified_owner_generation.is_none()
            {
                return Err("Agent automation pairing changed before the action was queued".into());
            }
            match (kind, goal_revision) {
                (
                    AutomationActionKind::OfflineGoal | AutomationActionKind::OfflineCombined,
                    Some(revision),
                ) => {
                    let goal = exact_goal(state, revision)?;
                    if goal.status != GoalStatus::Active {
                        return Err("Agent automation goal is no longer active".into());
                    }
                }
                (
                    AutomationActionKind::OfflineGoal | AutomationActionKind::OfflineCombined,
                    None,
                )
                | (AutomationActionKind::PairVerify | AutomationActionKind::OfflineChat, Some(_)) =>
                {
                    return Err("Agent automation queued action has inconsistent goal state".into());
                }
                (AutomationActionKind::PairVerify | AutomationActionKind::OfflineChat, None) => {}
                (AutomationActionKind::TurnRenewal, _) => {}
            }
            if !state.action.automatic_retry_in_progress {
                state.suspended_retry = None;
            }
            state.action = AutomationActionState {
                id: Some(action_id.to_string()),
                kind: Some(kind),
                status: AutomationActionStatus::Queued,
                pairing_generation: Some(pairing_generation),
                goal_revision,
                updated_at_ms: Some(now_ms),
                send_accepted: Some(false),
                automatic_retry_attempts: state.action.automatic_retry_attempts,
                chatgpt_access_retry_attempts: state.action.chatgpt_access_retry_attempts,
                chatgpt_access_retry_in_progress: state.action.chatgpt_access_retry_in_progress,
                automatic_retry_in_progress: state.action.automatic_retry_in_progress,
                bypass_conversation_cooldown: kind != AutomationActionKind::PairVerify
                    && state.action.bypass_conversation_cooldown
                    && (state.action.id.is_none() || state.action.id.as_deref() == Some(action_id)),
                cooldown_until_ms: (state.action.id.as_deref() == Some(action_id))
                    .then_some(state.action.cooldown_until_ms)
                    .flatten(),
                ..Default::default()
            };
            Ok(state.action.clone())
        })
    }

    pub(crate) fn defer_action_for_cooldown(
        &self,
        task: &ConversationIdentity,
        action_id: &str,
        next_action_id: &str,
        until_ms: u64,
    ) -> Result<bool, String> {
        if !valid_opaque(next_action_id) || until_ms == 0 || until_ms > 9_007_199_254_740_991 {
            return Err("Browser cooldown deferral is invalid".into());
        }
        self.mutate(task, |state| {
            if state.action.id.as_deref() != Some(action_id)
                || state.action.send_accepted != Some(false)
                || state.action.messages_fenced
                || state.action.goal_turn_counted
                || !matches!(
                    state.action.status,
                    AutomationActionStatus::Queued
                        | AutomationActionStatus::Navigating
                        | AutomationActionStatus::Sending
                        | AutomationActionStatus::Generating
                )
            {
                return Ok(false);
            }
            // Queueing reserved a retry, but no browser request was attempted.
            // Keep the original failure for pause/disable and restore its budget.
            if let Some(previous) = &state.suspended_retry {
                state.action.automatic_retry_attempts = previous.action.automatic_retry_attempts;
                state.action.chatgpt_access_retry_attempts =
                    previous.action.chatgpt_access_retry_attempts;
                state.goal_wake.recovery_attempts = previous.goal_wake.recovery_attempts;
            }
            state.action.id = Some(next_action_id.into());
            state.action.status = AutomationActionStatus::Queued;
            state.action.last_error = None;
            state.action.cooldown_until_ms = Some(until_ms);
            state.action.bypass_conversation_cooldown = false;
            Ok(true)
        })
    }

    pub(crate) fn resume_cooldown_action(
        &self,
        task: &ConversationIdentity,
        action_id: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.mutate(task, |state| {
            if state.action.id.as_deref() != Some(action_id)
                || state
                    .browser_cooldown_until_ms()
                    .is_none_or(|until| now_ms < until)
            {
                return Ok(());
            }
            if let Some(previous) = &state.suspended_retry {
                if chatgpt_access_failure(previous.action.status) {
                    state.action.chatgpt_access_retry_attempts =
                        previous.action.chatgpt_access_retry_attempts + 1;
                } else if confirmed_unsent_failure(&previous.action) {
                    state.action.automatic_retry_attempts =
                        previous.action.automatic_retry_attempts + 1;
                } else {
                    state.goal_wake.recovery_attempts = previous.goal_wake.recovery_attempts + 1;
                }
            }
            state.action.cooldown_until_ms = None;
            Ok(())
        })
    }

    pub(crate) fn restore_missing_cooldown_action(
        &self,
        task: &ConversationIdentity,
        expected: &AutomationActionState,
    ) -> Result<bool, String> {
        self.mutate(task, |state| {
            if state.action != *expected || state.browser_cooldown_until_ms().is_none() {
                return Ok(false);
            }
            if let Some(previous) = state.suspended_retry.take() {
                state.action = previous.action;
                state.goal_wake = previous.goal_wake;
            } else {
                state.action = AutomationActionState {
                    automatic_retry_attempts: state.action.automatic_retry_attempts,
                    chatgpt_access_retry_attempts: state.action.chatgpt_access_retry_attempts,
                    ..Default::default()
                };
            }
            Ok(true)
        })
    }

    pub(crate) fn record_queue_failure(
        &self,
        task: &ConversationIdentity,
        failure: QueueFailure<'_>,
    ) -> Result<AutomationActionState, String> {
        if !valid_opaque(failure.action_id)
            || failure.pairing_generation == 0
            || !bounded_private(failure.message)
            || failure.message.trim().is_empty()
        {
            return Err("Agent automation queue failure is invalid".into());
        }
        self.mutate(task, |state| {
            if state.pairing.generation != failure.pairing_generation {
                return Err(
                    "Agent automation pairing changed before queue failure reporting".into(),
                );
            }
            state.action = AutomationActionState {
                id: Some(failure.action_id.to_string()),
                kind: Some(failure.kind),
                status: AutomationActionStatus::Failed,
                pairing_generation: Some(failure.pairing_generation),
                goal_revision: failure.goal_revision,
                last_error: Some(failure.message.to_string()),
                updated_at_ms: Some(failure.now_ms),
                send_accepted: Some(false),
                automatic_retry_attempts: state.action.automatic_retry_attempts,
                chatgpt_access_retry_attempts: state.action.chatgpt_access_retry_attempts,
                chatgpt_access_retry_in_progress: state.action.chatgpt_access_retry_in_progress,
                automatic_retry_in_progress: state.action.automatic_retry_in_progress,
                ..Default::default()
            };
            Ok(state.action.clone())
        })
    }

    pub fn update_action_status(
        &self,
        task: &ConversationIdentity,
        action_id: &str,
        status: AutomationActionStatus,
        error: Option<&str>,
        stable_answer: Option<&str>,
        now_ms: u64,
    ) -> Result<AutomationActionState, String> {
        if !valid_opaque(action_id)
            || error.is_some_and(|value| !bounded_private(value))
            || stable_answer.is_some_and(|value| value.len() > MAX_PRIVATE_TEXT_BYTES)
        {
            return Err("Agent automation action update is invalid".into());
        }
        self.mutate(task, |state| {
            if state.action.id.as_deref() != Some(action_id) {
                return Err("Agent automation action changed before its status update".into());
            }
            if !matches!(
                status,
                AutomationActionStatus::Queued
                    | AutomationActionStatus::Navigating
                    | AutomationActionStatus::Sending
                    | AutomationActionStatus::Generating
            ) {
                state.suspended_retry = None;
                state.action.cooldown_until_ms = None;
            }
            let verified_pairing = state.action.kind == Some(AutomationActionKind::PairVerify)
                && state.action.pairing_generation == Some(state.pairing.generation)
                && state.pairing.verified_owner_generation.is_some();
            let completed = state.action.status == AutomationActionStatus::Idle
                && state.action.send_accepted == Some(true);
            state.action.status = if verified_pairing || completed {
                AutomationActionStatus::Idle
            } else {
                status
            };
            if status != AutomationActionStatus::Queued {
                state.action.bypass_conversation_cooldown = false;
            }
            if status == AutomationActionStatus::Ambiguous {
                state.action.send_accepted = Some(true);
            }
            state.action.last_error = if verified_pairing || completed {
                None
            } else {
                error.map(str::to_string)
            };
            if let Some(answer) = stable_answer {
                state.action.last_stable_answer = Some(answer.to_string());
            }
            state.action.updated_at_ms = Some(now_ms);
            if state.action.status == AutomationActionStatus::Idle {
                state.action.automatic_retry_attempts = 0;
                state.action.chatgpt_access_retry_attempts = 0;
                state.action.chatgpt_access_retry_in_progress = false;
                state.action.automatic_retry_in_progress = false;
            }
            Ok(state.action.clone())
        })
    }

    pub(crate) fn pause_action_retries(
        &self,
        task: &ConversationIdentity,
        expected_action_id: &str,
    ) -> Result<AutomationActionState, String> {
        self.mutate(task, |state| {
            if state.action.id.as_deref() != Some(expected_action_id) {
                return Err("The browser action changed before retries could be paused".into());
            }
            if state.action.status == AutomationActionStatus::Queued
                && state.action.automatic_retry_in_progress
            {
                if let Some(previous) = state.suspended_retry.take() {
                    state.action = previous.action;
                    state.goal_wake = previous.goal_wake;
                } else {
                    // Older queued retries have no saved failure. No Send has started.
                    state.action.status = AutomationActionStatus::Failed;
                    state.action.last_error = Some("Automatic retry stopped before Send".into());
                }
            } else if !state.raw_action_needs_attention() {
                return Err(
                    "Only retries for a failed or queued retry action can be paused".into(),
                );
            }
            state.action.automatic_retry_paused = true;
            state.action.automatic_retry_in_progress = false;
            state.action.chatgpt_access_retry_in_progress = false;
            Ok(state.action.clone())
        })
    }

    pub(crate) fn restore_disabled_queued_retry(
        &self,
        task: &ConversationIdentity,
        expected_action_id: &str,
    ) -> Result<bool, String> {
        self.mutate(task, |state| {
            if state.automatic_retries_enabled
                || state.action.id.as_deref() != Some(expected_action_id)
                || state.action.status != AutomationActionStatus::Queued
                || !state.action.automatic_retry_in_progress
            {
                return Ok(false);
            }
            if let Some(previous) = state.suspended_retry.take() {
                state.action = previous.action;
                state.goal_wake = previous.goal_wake;
            } else {
                state.action.status = AutomationActionStatus::Failed;
                state.action.last_error = Some("Automatic retry stopped before Send".into());
                state.action.automatic_retry_in_progress = false;
                state.action.chatgpt_access_retry_in_progress = false;
            }
            Ok(true)
        })
    }

    pub(crate) fn run_queued_action_now(
        &self,
        task: &ConversationIdentity,
        expected_action_id: &str,
        now_ms: u64,
    ) -> Result<AutomationActionState, String> {
        if !valid_opaque(expected_action_id) {
            return Err("Agent automation action ID is invalid".into());
        }
        self.mutate(task, |state| {
            if state.action.id.as_deref() != Some(expected_action_id) || !state.can_run_queued_now()
            {
                return Err("Only an unsent queued browser action can run now".into());
            }
            state.action.cooldown_until_ms = None;
            state.action.bypass_conversation_cooldown = true;
            state.action.automatic_retry_in_progress = false;
            state.action.chatgpt_access_retry_in_progress = false;
            state.suspended_retry = None;
            state.action.updated_at_ms = Some(now_ms);
            Ok(state.action.clone())
        })
    }

    pub fn cancel_action(
        &self,
        task: &ConversationIdentity,
        expected_action_id: &str,
        now_ms: u64,
    ) -> Result<AutomationActionState, String> {
        if !valid_opaque(expected_action_id) {
            return Err("Agent automation cancel action ID is invalid".into());
        }
        self.mutate(task, |state| {
            if state.action.id.as_deref() != Some(expected_action_id) {
                return Err("The browser action changed before it could be cancelled".into());
            }
            if state.action.status == AutomationActionStatus::Cancelled
                && state.action.last_error.is_none()
            {
                return Ok(state.action.clone());
            }
            if state.action.status != AutomationActionStatus::Cancelled
                && state.action.status != AutomationActionStatus::Queued
                && !state.raw_action_needs_attention()
                && !(state.action.kind == Some(AutomationActionKind::PairVerify)
                    && state.pairing.pending.is_some())
            {
                return Err("Only a queued or failed browser action can be cancelled here".into());
            }
            // Dismissing a failure or cancelling its queued retry ends that
            // error episode. Cancelling an initial queued goal wake keeps the
            // existing goal scheduling behavior.
            state.action.automatic_retry_paused |= state.raw_action_needs_attention()
                || state.action.automatic_retry_in_progress
                || state.action.last_error.is_some();
            state.action.status = AutomationActionStatus::Cancelled;
            state.action.cooldown_until_ms = None;
            state.suspended_retry = None;
            state.action.last_error = None;
            state.action.automatic_retry_attempts = 0;
            state.action.chatgpt_access_retry_attempts = 0;
            state.action.chatgpt_access_retry_in_progress = false;
            state.action.automatic_retry_in_progress = false;
            state.action.updated_at_ms = Some(now_ms);
            Ok(state.action.clone())
        })
    }

    pub fn resume_cancelled_action_for_new_message(
        &self,
        task: &ConversationIdentity,
        created_at_ms: u64,
    ) -> Result<(), String> {
        self.mutate(task, |state| {
            if state.can_resume_cancelled_action()
                && state
                    .action
                    .updated_at_ms
                    .is_some_and(|cancelled| created_at_ms > cancelled)
            {
                // New intent may resume unsent work or follow an accepted, fenced batch.
                // Retain the mirror ledger so an accepted message is never sent again.
                state.action = AutomationActionState::default();
            }
            Ok(())
        })
    }

    pub fn reset_failed_action_for_retry(
        &self,
        task: &ConversationIdentity,
        expected_action_id: &str,
        now_ms: u64,
    ) -> Result<AutomationActionState, String> {
        if !valid_opaque(expected_action_id) {
            return Err("Agent automation retry action ID is invalid".into());
        }
        self.mutate(task, |state| {
            if state.action.id.as_deref() != Some(expected_action_id) {
                return Err("The browser action changed before it could be retried".into());
            }
            if action_retry_mode(state).is_none() {
                return Err("Only a browser action that failed before Send can be retried".into());
            }
            if state.action.kind == Some(AutomationActionKind::PairVerify) {
                return Err(
                    "Pairing verification cannot be replayed; start a new pairing request".into(),
                );
            }
            state.suspended_retry = None;
            if action_retry_mode(state) == Some("continue_goal") {
                state.goal_wake = GoalWakeState::default();
            }
            state.action = AutomationActionState {
                updated_at_ms: Some(now_ms),
                bypass_conversation_cooldown: true,
                ..Default::default()
            };
            Ok(state.action.clone())
        })
    }

    pub(crate) fn clear_unused_manual_retry(
        &self,
        task: &ConversationIdentity,
    ) -> Result<(), String> {
        self.mutate(task, |state| {
            if state.action.id.is_none() {
                state.action.bypass_conversation_cooldown = false;
            }
            Ok(())
        })
    }

    pub(crate) fn reset_offline_goal_for_retry(
        &self,
        task: &ConversationIdentity,
        expected: &AutomationTaskState,
        now_ms: u64,
    ) -> Result<bool, String> {
        self.mutate(task, |state| {
            if state.action != expected.action
                || state.pairing != expected.pairing
                || state.goal != expected.goal
                || state.goal_wake != expected.goal_wake
                || state
                    .goal_retry_at_ms()
                    .is_none_or(|deadline| now_ms < deadline)
            {
                return Ok(false);
            }
            state.suspended_retry = Some(SuspendedRetryState {
                action: state.action.clone(),
                goal_wake: state.goal_wake.clone(),
            });
            // The active goal authorizes another turn. Keep the accepted-message
            // ledger so continuing a goal never replays an earlier user batch.
            let unsent = confirmed_unsent_failure(&state.action);
            if !unsent {
                state.goal_wake.recovery_attempts += 1;
            }
            state.action = AutomationActionState {
                updated_at_ms: Some(now_ms),
                automatic_retry_in_progress: true,
                chatgpt_access_retry_attempts: state.action.chatgpt_access_retry_attempts,
                automatic_retry_attempts: if unsent {
                    state.action.automatic_retry_attempts + 1
                } else {
                    0
                },
                ..Default::default()
            };
            // Keep the activity baseline and recovery count across the next Send.
            // Send acceptance alone does not prove that the agent recovered.
            state.goal_wake.nudged_epoch = None;
            state.goal_wake.nudged_sequence = None;
            state.goal_wake.last_nudged_at_ms = None;
            Ok(true)
        })
    }

    pub(crate) fn reset_goal_recovery_after_activity(
        &self,
        task: &ConversationIdentity,
        expected: &AutomationTaskState,
        activity_epoch: &str,
        activity_sequence: u64,
    ) -> Result<bool, String> {
        if expected.goal_wake.recovery_attempts == 0
            || expected.goal_wake.activity_epoch.is_none()
            || (expected.goal_wake.activity_epoch.as_deref() == Some(activity_epoch)
                && expected.goal_wake.activity_sequence == Some(activity_sequence))
        {
            return Ok(false);
        }
        self.mutate(task, |state| {
            if state.action != expected.action
                || state.goal_wake != expected.goal_wake
                || state.goal != expected.goal
            {
                return Ok(false);
            }
            state.goal_wake = GoalWakeState::default();
            if state.action.status == AutomationActionStatus::Stalled
                && state.action.send_accepted == Some(true)
                && matches!(
                    state.action.kind,
                    Some(AutomationActionKind::OfflineGoal | AutomationActionKind::OfflineCombined)
                )
            {
                state.action.status = AutomationActionStatus::Idle;
                state.action.last_error = None;
            }
            Ok(true)
        })
    }

    pub(crate) fn mark_goal_recovery_exhausted(
        &self,
        task: &ConversationIdentity,
        expected: &AutomationTaskState,
    ) -> Result<(), String> {
        self.mutate(task, |state| {
            if state.action == expected.action
                && state.goal_wake == expected.goal_wake
                && state.goal == expected.goal
                && state.goal_recovery_exhausted()
            {
                state.action.status = AutomationActionStatus::Stalled;
                let retries = state.retry_timings.goal_recovery_delays_seconds.len();
                state.action.last_error.get_or_insert_with(|| {
                    format!("Automatic goal recovery stopped after {retries} retries without agent activity. Use Continue to try again.")
                });
                state.action.automatic_retry_in_progress = false;
                state.action.chatgpt_access_retry_in_progress = false;
            }
            Ok(())
        })
    }

    pub(crate) fn reset_automatic_failure_for_retry(
        &self,
        task: &ConversationIdentity,
        expected: &AutomationActionState,
        now_ms: u64,
    ) -> Result<bool, String> {
        self.mutate(task, |state| {
            if state.action != *expected
                || state
                    .automatic_retry_at_ms()
                    .is_none_or(|deadline| now_ms < deadline)
            {
                return Ok(false);
            }
            state.suspended_retry = Some(SuspendedRetryState {
                action: state.action.clone(),
                goal_wake: state.goal_wake.clone(),
            });
            let access_retry = chatgpt_access_failure(state.action.status);
            state.action = AutomationActionState {
                updated_at_ms: Some(now_ms),
                automatic_retry_attempts: state.action.automatic_retry_attempts
                    + u32::from(!access_retry),
                chatgpt_access_retry_attempts: state.action.chatgpt_access_retry_attempts
                    + u32::from(access_retry),
                chatgpt_access_retry_in_progress: access_retry,
                automatic_retry_in_progress: true,
                ..Default::default()
            };
            Ok(true)
        })
    }

    pub(crate) fn expire_unobserved_action(
        &self,
        task: &ConversationIdentity,
        expected: &AutomationActionState,
    ) -> Result<bool, String> {
        self.mutate(task, |state| {
            if state.action != *expected || !matches!(state.action.status, AutomationActionStatus::Navigating | AutomationActionStatus::Sending | AutomationActionStatus::Generating) {
                return Ok(false);
            }
            state.action.status = AutomationActionStatus::Ambiguous;
            state.action.last_error = Some("Saved browser action exceeded its time limit; completion could not be confirmed. Check ChatGPT before restarting it.".into());
            // Preserve the original start time for a later retained-receipt reconciliation.
            Ok(true)
        })
    }

    pub(crate) fn record_approval_resume(
        &self,
        task: &ConversationIdentity,
        expected: &AutomationTaskState,
        owner_generation: u64,
        activity_epoch: &str,
        activity_sequence: u64,
        now_ms: u64,
    ) -> Result<bool, String> {
        if !valid_opaque(activity_epoch) {
            return Err("Approval activity baseline is invalid".into());
        }
        self.mutate(task, |state| {
            if state.action != expected.action
                || state.pairing != expected.pairing
                || state.goal.as_ref().map(|g| (g.revision, g.status))
                    != expected.goal.as_ref().map(|g| (g.revision, g.status))
                || state.pairing.verified_owner_generation != Some(owner_generation)
            {
                return Ok(false);
            }
            // Approval resumes the existing turn. Preserve actual Send evidence,
            // goal counts and every pending/accepted message delivery record.
            if state.action.kind != Some(AutomationActionKind::PairVerify) {
                state.action.status = AutomationActionStatus::Idle;
                state.action.last_error = None;
                state.action.automatic_retry_attempts = 0;
                state.action.chatgpt_access_retry_attempts = 0;
                state.action.chatgpt_access_retry_in_progress = false;
                state.action.automatic_retry_in_progress = false;
                state.action.automatic_retry_paused = false;
                state.suspended_retry = None;
            }
            state.approval_resume = Some(ApprovalResumeState {
                resumed_at_ms: now_ms,
                owner_generation,
                pairing_generation: state.pairing.generation,
                goal_revision: state.goal.as_ref().map(|g| g.revision),
            });
            if state
                .goal
                .as_ref()
                .is_some_and(|g| g.status == GoalStatus::Active)
            {
                state.goal_wake = GoalWakeState {
                    activity_epoch: Some(activity_epoch.into()),
                    activity_sequence: Some(activity_sequence),
                    nudged_epoch: Some(activity_epoch.into()),
                    nudged_sequence: Some(activity_sequence),
                    last_nudged_at_ms: Some(now_ms),
                    recovery_attempts: 0,
                };
            }
            Ok(true)
        })
    }

    pub(crate) fn clear_approval_resume(
        &self,
        task: &ConversationIdentity,
        expected: &AutomationTaskState,
        made_progress: bool,
    ) -> Result<bool, String> {
        self.mutate(task, |state| {
            if state.approval_resume != expected.approval_resume
                || state.pairing != expected.pairing
                || state.goal.as_ref().map(|g| (g.revision, g.status))
                    != expected.goal.as_ref().map(|g| (g.revision, g.status))
            {
                return Ok(false);
            }
            if made_progress {
                state.goal_wake = GoalWakeState::default();
            }
            state.approval_resume = None;
            Ok(true)
        })
    }

    pub(crate) fn reconcile_resumed_goal_retry(
        &self,
        task: &ConversationIdentity,
        expected: &AutomationTaskState,
        owner_generation: u64,
        last_agent_call_at_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<bool, String> {
        if expected.action.kind != Some(AutomationActionKind::OfflineGoal)
            || !(expected.raw_action_needs_attention()
                || expected.action.status == AutomationActionStatus::Cancelled
                    && expected.action.automatic_retry_paused
                || expected.action.status == AutomationActionStatus::Queued
                    && expected.action.automatic_retry_in_progress)
        {
            return Ok(false);
        }
        self.mutate(task, |state| {
            let queued_retry = state.action.status == AutomationActionStatus::Queued
                && state.action.automatic_retry_in_progress;
            if state.action != expected.action
                || state.pairing != expected.pairing
                || state.goal != expected.goal
                || state.goal_wake != expected.goal_wake
                || state.action.kind != Some(AutomationActionKind::OfflineGoal)
                || state.action.pairing_generation != Some(state.pairing.generation)
                || state.pairing.verified_owner_generation != Some(owner_generation)
                || state.goal.as_ref().is_none_or(|goal| {
                    goal.status != GoalStatus::Active
                        || Some(goal.revision) != state.action.goal_revision
                })
                || (!queued_retry
                    && state.action.updated_at_ms.is_none_or(|failed_at| {
                        last_agent_call_at_ms.is_none_or(|last| last <= failed_at)
                    }))
            {
                return Ok(false);
            }
            state.action.automatic_retry_paused = false;
            state.suspended_retry = None;
            // The caller observed the agent online again. Retire the failed
            // goal-only wake without claiming our Send succeeded or touching
            // user-message delivery, which might have resumed on another device.
            state.action.status = AutomationActionStatus::Idle;
            state.action.last_error = None;
            state.action.updated_at_ms = Some(now_ms);
            state.action.automatic_retry_attempts = 0;
            state.action.chatgpt_access_retry_attempts = 0;
            state.action.chatgpt_access_retry_in_progress = false;
            state.action.automatic_retry_in_progress = false;
            state.goal_wake = GoalWakeState::default();
            Ok(true)
        })
    }

    pub(crate) fn reconcile_completed_goal_action(
        &self,
        task: &ConversationIdentity,
        expected: &AutomationTaskState,
        activity_epoch: &str,
        activity_sequence: u64,
        made_progress: bool,
        now_ms: u64,
    ) -> Result<bool, String> {
        self.mutate(task, |state| {
            if state.action != expected.action
                || state.pairing != expected.pairing
                || state.goal != expected.goal
                || state.action.kind != Some(AutomationActionKind::OfflineGoal)
                || !matches!(
                    state.action.status,
                    AutomationActionStatus::Navigating
                        | AutomationActionStatus::Sending
                        | AutomationActionStatus::Generating
                        | AutomationActionStatus::Ambiguous
                )
                || state.goal.as_ref().is_none_or(|goal| {
                    goal.status != GoalStatus::Active
                        || Some(goal.revision) != state.action.goal_revision
                })
            {
                return Ok(false);
            }
            count_accepted_goal_turn(state);
            state.action.send_accepted = Some(true);
            state.action.automatic_retry_attempts = 0;
            state.action.chatgpt_access_retry_attempts = 0;
            state.action.chatgpt_access_retry_in_progress = false;
            state.action.automatic_retry_in_progress = false;
            state.action.status = if made_progress {
                AutomationActionStatus::Idle
            } else {
                AutomationActionStatus::Stalled
            };
            state.action.last_error = None;
            state.action.updated_at_ms = Some(now_ms);
            state.goal_wake = if made_progress {
                GoalWakeState::default()
            } else {
                GoalWakeState {
                    activity_epoch: Some(activity_epoch.to_owned()),
                    activity_sequence: Some(activity_sequence),
                    nudged_epoch: Some(activity_epoch.to_owned()),
                    nudged_sequence: Some(activity_sequence),
                    last_nudged_at_ms: Some(now_ms),
                    recovery_attempts: state.goal_wake.recovery_attempts,
                }
            };
            Ok(true)
        })
    }

    pub(crate) fn record_chat_submission(
        &self,
        task: &ConversationIdentity,
        action_id: &str,
        pairing_generation: u64,
        batch: AcceptedChatBatch<'_>,
    ) -> Result<bool, String> {
        validate_accepted_batch(
            action_id,
            batch.batch_id,
            batch.message_ids,
            batch.transcript_revision,
        )?;
        self.with_locked_task(task, |path| {
            let state = load_state(path)?;
            if state.action.id.as_deref() != Some(action_id)
                || state.action.status != AutomationActionStatus::Queued
                || state.pairing.generation != pairing_generation
                || !state.can_reconcile_chat_delivery(batch.owner_generation)
            {
                return Ok(false);
            }
            let submission = ChatSubmission {
                version: 1,
                action_id: action_id.to_owned(),
                owner_generation: batch.owner_generation,
                pairing_generation,
                batch_id: batch.batch_id.to_owned(),
                message_ids: batch.message_ids.to_vec(),
                transcript_revision: batch.transcript_revision.to_owned(),
            };
            // Publish the intent before any browser interaction. This is not a
            // send receipt and must not fence an instruction that is still unread.
            persist_private_record(&path.with_file_name(CHAT_SUBMISSION_FILE), &submission)?;
            Ok(true)
        })
    }

    pub(crate) fn chat_delivery_candidate(
        &self,
        task: &ConversationIdentity,
        expected: &AutomationTaskState,
        owner_generation: u64,
    ) -> Result<Option<ChatSubmission>, String> {
        self.with_locked_task(task, |path| {
            let state = load_state(path)?;
            if state.action != expected.action || state.pairing != expected.pairing {
                return Ok(None);
            }
            chat_delivery_candidate(path, &state, owner_generation)
        })
    }

    pub(crate) fn reconcile_read_chat_action(
        &self,
        task: &ConversationIdentity,
        expected: &AutomationTaskState,
        submission: &ChatSubmission,
        now_ms: u64,
    ) -> Result<bool, String> {
        self.with_locked_task(task, |path| {
            let mut state = load_state(path)?;
            if state.action != expected.action
                || state.pairing != expected.pairing
                || state.chat_mirror != expected.chat_mirror
                || state.goal != expected.goal
                || chat_delivery_candidate(path, &state, submission.owner_generation)?.as_ref()
                    != Some(submission)
            {
                return Ok(false);
            }
            // Reading the exact batch proves delivery through the agent's native
            // mailbox even if browser preparation failed before pressing Send.
            merge_accepted_batch(
                &mut state.chat_mirror,
                submission.owner_generation,
                &submission.batch_id,
                &submission.message_ids,
                &submission.transcript_revision,
            )?;
            let browser_may_have_sent = state.action.send_accepted == Some(true);
            state.action.send_accepted = Some(true);
            state.action.messages_fenced = true;
            if browser_may_have_sent {
                count_accepted_goal_turn(&mut state);
            }
            state.action.status = AutomationActionStatus::Idle;
            state.action.last_error = None;
            state.action.automatic_retry_attempts = 0;
            state.action.chatgpt_access_retry_attempts = 0;
            state.action.chatgpt_access_retry_in_progress = false;
            state.action.automatic_retry_in_progress = false;
            state.action.updated_at_ms = Some(now_ms);
            state.goal_wake = GoalWakeState::default();
            persist_state(path, &state)?;
            Ok(true)
        })
    }

    pub(crate) fn clear_persisted_queued_action(
        &self,
        task: &ConversationIdentity,
        expected_action_id: &str,
        now_ms: u64,
    ) -> Result<AutomationActionState, String> {
        if !valid_opaque(expected_action_id) {
            return Err("Persisted automation action ID is invalid".into());
        }
        self.mutate(task, |state| {
            if state.action.id.as_deref() != Some(expected_action_id)
                || state.action.status != AutomationActionStatus::Queued
                || state.action.kind == Some(AutomationActionKind::PairVerify)
            {
                return Err("Persisted automation action state is inconsistent".into());
            }
            state.action = AutomationActionState {
                updated_at_ms: Some(now_ms),
                ..Default::default()
            };
            Ok(state.action.clone())
        })
    }

    pub fn record_goal_nudge(
        &self,
        task: &ConversationIdentity,
        action_id: &str,
        activity_epoch: &str,
        activity_sequence: u64,
        now_ms: u64,
    ) -> Result<GoalWakeState, String> {
        if !valid_opaque(action_id) || !valid_opaque(activity_epoch) {
            return Err("Agent automation goal nudge is invalid".into());
        }
        self.mutate(task, |state| {
            if state.action.id.as_deref() != Some(action_id)
                || !matches!(
                    state.action.kind,
                    Some(AutomationActionKind::OfflineGoal | AutomationActionKind::OfflineCombined)
                )
            {
                return Err(
                    "Agent automation goal nudge no longer matches the current action".into(),
                );
            }
            state.goal_wake.activity_epoch = Some(activity_epoch.to_string());
            state.goal_wake.activity_sequence = Some(activity_sequence);
            state.goal_wake.nudged_epoch = Some(activity_epoch.to_string());
            state.goal_wake.nudged_sequence = Some(activity_sequence);
            state.goal_wake.last_nudged_at_ms = Some(now_ms);
            Ok(state.goal_wake.clone())
        })
    }

    pub fn record_goal_turn(
        &self,
        task: &ConversationIdentity,
        action_id: &str,
    ) -> Result<(), String> {
        self.mutate(task, |state| {
            if state.action.id.as_deref() != Some(action_id) {
                return Ok(());
            }
            state.action.send_accepted = Some(true);
            state.action.automatic_retry_attempts = 0;
            state.action.chatgpt_access_retry_attempts = 0;
            state.action.chatgpt_access_retry_in_progress = false;
            state.action.automatic_retry_in_progress = false;
            count_accepted_goal_turn(state);
            Ok(())
        })
    }

    pub fn record_accepted_batch(
        &self,
        task: &ConversationIdentity,
        action_id: &str,
        batch: AcceptedChatBatch<'_>,
        now_ms: u64,
    ) -> Result<ChatMirrorState, String> {
        validate_accepted_batch(
            action_id,
            batch.batch_id,
            batch.message_ids,
            batch.transcript_revision,
        )?;
        self.mutate(task, |state| {
            if state.action.id.as_deref() != Some(action_id)
                || !matches!(
                    state.action.kind,
                    Some(AutomationActionKind::OfflineChat | AutomationActionKind::OfflineCombined)
                )
            {
                return Err(
                    "Agent automation chat batch no longer matches the current action".into(),
                );
            }
            merge_accepted_batch(
                &mut state.chat_mirror,
                batch.owner_generation,
                batch.batch_id,
                batch.message_ids,
                batch.transcript_revision,
            )?;
            state.action.messages_fenced = true;
            state.action.updated_at_ms = Some(now_ms);
            Ok(state.chat_mirror.clone())
        })
    }

    pub(crate) fn record_superseded_accepted_batch(
        &self,
        task: &ConversationIdentity,
        action_id: &str,
        batch: AcceptedChatBatch<'_>,
    ) -> Result<ChatMirrorState, String> {
        validate_accepted_batch(
            action_id,
            batch.batch_id,
            batch.message_ids,
            batch.transcript_revision,
        )?;
        self.mutate(task, |state| {
            merge_accepted_batch(
                &mut state.chat_mirror,
                batch.owner_generation,
                batch.batch_id,
                batch.message_ids,
                batch.transcript_revision,
            )?;
            Ok(state.chat_mirror.clone())
        })
    }

    pub(crate) fn retain_accepted_messages(
        &self,
        task: &ConversationIdentity,
        owner_generation: u64,
        message_ids: &[String],
    ) -> Result<ChatMirrorState, String> {
        if message_ids.len() > MAX_CHAT_MIRROR_IDS || message_ids.iter().any(|id| !valid_opaque(id))
        {
            return Err("Agent automation accepted chat history is invalid".into());
        }
        self.mutate(task, |state| {
            state.chat_mirror.owner_generation = Some(owner_generation);
            state.chat_mirror.message_ids = message_ids.to_vec();
            if message_ids.is_empty() {
                state.chat_mirror.last_accepted_batch_id = None;
                state.chat_mirror.transcript_revision_at_start = None;
            }
            Ok(state.chat_mirror.clone())
        })
    }

    pub(super) fn mutate<R>(
        &self,
        task: &ConversationIdentity,
        mutation: impl FnOnce(&mut AutomationTaskState) -> Result<R, String>,
    ) -> Result<R, String> {
        self.with_locked_task(task, |path| {
            let mut state = load_state(path)?;
            state.automatic_retries_enabled = self.automatic_retries_enabled()?;
            state.retry_timings = self.retry_timings.clone();
            let result = mutation(&mut state)?;
            if state.action.status != AutomationActionStatus::Queued {
                state.action.cooldown_until_ms = None;
            }
            validate_state(&state)?;
            persist_state(path, &state)?;
            Ok(result)
        })
    }

    fn mutate_persisting_errors<R>(
        &self,
        task: &ConversationIdentity,
        mutation: impl FnOnce(&mut AutomationTaskState) -> Result<R, String>,
    ) -> Result<R, String> {
        self.with_locked_task(task, |path| {
            let mut state = load_state(path)?;
            state.automatic_retries_enabled = self.automatic_retries_enabled()?;
            state.retry_timings = self.retry_timings.clone();
            let result = mutation(&mut state);
            if state.action.status != AutomationActionStatus::Queued {
                state.action.cooldown_until_ms = None;
            }
            validate_state(&state)?;
            persist_state(path, &state)?;
            result
        })
    }

    fn with_locked_task<R>(
        &self,
        task: &ConversationIdentity,
        action: impl FnOnce(&Path) -> Result<R, String>,
    ) -> Result<R, String> {
        self.with_locked_task_key(task.stable_key(), action)
    }

    fn with_locked_task_key<R>(
        &self,
        task_key: &str,
        action: impl FnOnce(&Path) -> Result<R, String>,
    ) -> Result<R, String> {
        validate_task_key(task_key)?;
        let directory = self.root.join(task_key);
        ensure_private_directory(&self.root, "Agent automation state root")?;
        ensure_private_directory(&directory, "Agent automation task directory")?;

        let process_lock = process_lock(directory.join(LOCK_FILE))?;
        let _process_guard = process_lock
            .lock()
            .map_err(|_| "Agent automation process lock is unavailable")?;
        let lock_file = open_lock_file(&directory.join(LOCK_FILE))?;
        lock_file
            .lock()
            .map_err(|error| format!("Could not lock agent automation state: {error}"))?;
        action(&directory.join(STATE_FILE))
    }

    fn pairing_index_path(&self, digest: &str) -> Result<PathBuf, String> {
        if !valid_digest(digest) {
            return Err(invalid_pairing_reference());
        }
        Ok(self.pairing_index_root.join(digest))
    }

    fn publish_pairing_index(&self, digest: &str, task_key: &str) -> Result<(), String> {
        validate_task_key(task_key)?;
        let path = self.pairing_index_path(digest)?;
        let parent = path
            .parent()
            .ok_or("Pairing index has no parent directory")?;
        ensure_private_directory(parent, "Agent automation pairing index")?;
        let record = PairingIndexRecord {
            task_root: self.root.to_string_lossy().into_owned(),
            task_key: task_key.to_string(),
        };
        let bytes = serde_json::to_vec(&record)
            .map_err(|error| format!("Could not encode agent automation pairing index: {error}"))?;
        if bytes.len() > 8192 {
            return Err("Agent automation pairing index is too large".into());
        }
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|error| format!("Could not create agent automation pairing index: {error}"))?;
        set_private_file_permissions(temporary.path(), "temporary agent automation pairing index")?;
        temporary
            .write_all(&bytes)
            .and_then(|_| temporary.as_file().sync_all())
            .map_err(|error| format!("Could not write agent automation pairing index: {error}"))?;
        match temporary.persist_noclobber(&path) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = self.read_pairing_index(digest)?;
                if existing != record {
                    return Err("Agent automation pairing reference collision".into());
                }
            }
            Err(error) => {
                return Err(format!(
                    "Could not publish agent automation pairing index: {}",
                    error.error
                ));
            }
        }
        set_private_file_permissions(&path, "agent automation pairing index")?;
        Ok(())
    }

    fn read_pairing_index(&self, digest: &str) -> Result<PairingIndexRecord, String> {
        let path = self.pairing_index_path(digest)?;
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(invalid_pairing_reference());
            }
            Err(error) => {
                return Err(format!(
                    "Could not inspect agent automation pairing index: {error}"
                ));
            }
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 8192 {
            return Err("Agent automation pairing index is invalid".into());
        }
        validate_private_file(&metadata, "Agent automation pairing index")?;
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
            options.custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            );
        }
        let file = options
            .open(&path)
            .map_err(|error| format!("Could not open agent automation pairing index: {error}"))?;
        let metadata = file.metadata().map_err(|error| {
            format!("Could not inspect agent automation pairing index: {error}")
        })?;
        if !metadata.is_file() || metadata.len() > 8192 {
            return Err("Agent automation pairing index is invalid".into());
        }
        validate_private_file(&metadata, "Agent automation pairing index")?;
        let mut bytes = Vec::new();
        file.take(8193)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("Could not read agent automation pairing index: {error}"))?;
        if bytes.len() > 8192 {
            return Err("Agent automation pairing index is too large".into());
        }
        let record: PairingIndexRecord = serde_json::from_slice(&bytes)
            .map_err(|_| "Agent automation pairing index is invalid".to_string())?;
        if !Path::new(&record.task_root).is_absolute() {
            return Err("Agent automation pairing index is invalid".into());
        }
        validate_task_key(&record.task_key)?;
        Ok(record)
    }

    fn remove_pairing_index(&self, digest: &str) -> Result<(), String> {
        let path = self.pairing_index_path(digest)?;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err("Agent automation pairing index is invalid".into());
                }
                std::fs::remove_file(path).map_err(|error| {
                    format!("Could not remove agent automation pairing index: {error}")
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "Could not inspect agent automation pairing index: {error}"
            )),
        }
    }
}

fn pairing_index_root(config: &AppConfig, fallback_root: &Path) -> PathBuf {
    config
        .memory
        .dir
        .as_ref()
        .map(PathBuf::from)
        .map(|base| base.join(PAIRING_INDEX_DIR))
        .or_else(|| crate::util::home_dir().map(|home| home.join(".codexify/browser/pairings")))
        .unwrap_or_else(|| fallback_root.join(PAIRING_INDEX_DIR))
}

enum PairingReferenceOutcome {
    Verified(Box<PairingState>),
    Rejected,
}

fn invalid_pairing_reference() -> String {
    "The agent-chat pairing reference is invalid, expired, or already used".into()
}

fn exact_goal(state: &AutomationTaskState, expected_revision: u64) -> Result<&GoalRecord, String> {
    let goal = state
        .goal
        .as_ref()
        .ok_or("No agent-chat goal is configured")?;
    if goal.revision != expected_revision {
        return Err("The agent-chat goal revision changed".into());
    }
    Ok(goal)
}

fn exact_goal_mut(
    state: &mut AutomationTaskState,
    expected_revision: u64,
) -> Result<&mut GoalRecord, String> {
    let goal = state
        .goal
        .as_mut()
        .ok_or("No agent-chat goal is configured")?;
    if goal.revision != expected_revision {
        return Err("The agent-chat goal revision changed".into());
    }
    Ok(goal)
}

fn count_accepted_goal_turn(state: &mut AutomationTaskState) {
    if state.action.goal_turn_counted || state.action.kind == Some(AutomationActionKind::PairVerify)
    {
        return;
    }
    let Some(goal) = state
        .goal
        .as_mut()
        .filter(|goal| goal.status == GoalStatus::Active)
    else {
        return;
    };
    if state
        .action
        .goal_revision
        .is_some_and(|revision| revision != goal.revision)
    {
        return;
    }
    goal.turn_count = goal.turn_count.saturating_add(1);
    state.action.goal_turn_counted = true;
}

fn start_goal(
    state: &mut AutomationTaskState,
    expected_revision: u64,
    now_ms: u64,
) -> Result<GoalRecord, String> {
    let goal = exact_goal_mut(state, expected_revision)?;
    let was_blocked = goal.status == GoalStatus::Blocked;
    match goal.status {
        GoalStatus::Configured | GoalStatus::Blocked => {
            goal.active_since_ms = None;
            goal.timer_until_ms = None;
            goal.status = GoalStatus::Active;
            goal.updated_at_ms = now_ms;
            goal.finished_at_ms = None;
            goal.summary = None;
            goal.blocked_reason = None;
        }
        GoalStatus::Active => {}
        GoalStatus::Completed | GoalStatus::Error => {
            return Err(
                "Only a paused or blocked agent-chat goal can be started; create a new revision first"
                    .into(),
            );
        }
    }
    let result = goal.clone();
    state.goal_wake = GoalWakeState::default();
    if state.can_resume_cancelled_action()
        || (was_blocked
            && state.action.goal_revision == Some(expected_revision)
            && state.action.kind == Some(AutomationActionKind::OfflineGoal)
            && !matches!(
                state.action.status,
                AutomationActionStatus::Queued
                    | AutomationActionStatus::Navigating
                    | AutomationActionStatus::Sending
                    | AutomationActionStatus::Generating
            ))
    {
        // Play requests new work. Keep the accepted-message ledger when releasing
        // cancellation, and retain user-message sends without a confirmed fence.
        state.action = AutomationActionState::default();
    }
    Ok(result)
}

fn normalize_goal_finish(finish: GoalFinish) -> Result<GoalFinish, String> {
    match finish {
        GoalFinish::Completed { summary } => {
            let summary = summary
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            if summary
                .as_ref()
                .is_some_and(|value| value.len() > MAX_GOAL_RESULT_BYTES)
            {
                return Err("Agent-chat goal summary is too large".into());
            }
            Ok(GoalFinish::Completed { summary })
        }
        GoalFinish::Blocked { reason } => {
            let reason = reason.trim().to_string();
            if reason.is_empty() {
                return Err("A blocked agent-chat goal requires a concrete reason".into());
            }
            if reason.len() > MAX_GOAL_RESULT_BYTES {
                return Err("Agent-chat goal blocked reason is too large".into());
            }
            Ok(GoalFinish::Blocked { reason })
        }
    }
}

fn terminal_finish_matches(goal: &GoalRecord, finish: &GoalFinish) -> bool {
    match (goal.status, finish) {
        (GoalStatus::Completed, GoalFinish::Completed { summary }) => {
            goal.summary == *summary && goal.blocked_reason.is_none()
        }
        (GoalStatus::Blocked, GoalFinish::Blocked { reason }) => {
            goal.blocked_reason.as_deref() == Some(reason.as_str()) && goal.summary.is_none()
        }
        _ => false,
    }
}

fn validate_task_key(value: &str) -> Result<(), String> {
    if valid_digest(value) {
        Ok(())
    } else {
        Err("Agent automation task identity is invalid".into())
    }
}

fn process_lock(path: PathBuf) -> Result<Arc<Mutex<()>>, String> {
    let locks = PROCESS_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .map_err(|_| "Agent automation process lock registry is unavailable")?;
    if let Some(lock) = locks.get(&path).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = Arc::new(Mutex::new(()));
    locks.insert(path, Arc::downgrade(&lock));
    Ok(lock)
}

fn open_lock_file(path: &Path) -> Result<File, String> {
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err("Agent automation lock must be a regular non-symlink file".into());
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("Could not open agent automation lock: {error}"))?;
    set_private_file_permissions(path, "agent automation lock")?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("Could not inspect agent automation lock: {error}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("Agent automation lock must remain a regular file".into());
    }
    validate_private_file(&metadata, "Agent automation lock")?;
    Ok(file)
}

fn read_private_record(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(format!("Could not inspect agent automation state: {error}")),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("Agent automation state must be a regular non-symlink file".into());
    }
    if metadata.len() > MAX_STATE_BYTES {
        return Err("Agent automation state is too large".into());
    }
    validate_private_file(&metadata, "Agent automation state")?;

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
        .map_err(|error| format!("Could not open agent automation state: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("Could not inspect open agent automation state: {error}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("Agent automation state must remain a regular file".into());
    }
    validate_private_file(&metadata, "Agent automation state")?;
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read agent automation state: {error}"))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("Agent automation state is too large".into());
    }
    Ok(Some(bytes))
}

fn chat_delivery_candidate(
    path: &Path,
    state: &AutomationTaskState,
    owner_generation: u64,
) -> Result<Option<ChatSubmission>, String> {
    if !state.can_reconcile_chat_delivery(owner_generation) {
        return Ok(None);
    }
    if let Some(bytes) = read_private_record(&path.with_file_name(CHAT_SUBMISSION_FILE))? {
        let submission: ChatSubmission = serde_json::from_slice(&bytes)
            .map_err(|_| "Chat submission intent is invalid JSON".to_string())?;
        validate_accepted_batch(
            &submission.action_id,
            &submission.batch_id,
            &submission.message_ids,
            &submission.transcript_revision,
        )?;
        if submission.version != 1 {
            return Err("Chat submission intent version is unsupported".into());
        }
        if state.action.id.as_deref() == Some(submission.action_id.as_str())
            && submission.owner_generation == owner_generation
            && submission.pairing_generation == state.pairing.generation
        {
            return Ok(Some(submission));
        }
    }
    // Preserve recovery of already-fenced actions from older installations.
    if state.action.messages_fenced
        && state.chat_mirror.owner_generation == Some(owner_generation)
        && !state.chat_mirror.message_ids.is_empty()
        && let (Some(action_id), Some(batch_id), Some(revision)) = (
            &state.action.id,
            &state.chat_mirror.last_accepted_batch_id,
            &state.chat_mirror.transcript_revision_at_start,
        )
    {
        return Ok(Some(ChatSubmission {
            version: 1,
            action_id: action_id.clone(),
            owner_generation,
            pairing_generation: state.pairing.generation,
            batch_id: batch_id.clone(),
            message_ids: state.chat_mirror.message_ids.clone(),
            transcript_revision: revision.clone(),
        }));
    }
    Ok(None)
}

fn migrate_legacy_cooldown(state: &mut AutomationTaskState) -> bool {
    let action = &mut state.action;
    if !matches!(
        action.status,
        AutomationActionStatus::RateLimited | AutomationActionStatus::ConversationUnavailable
    ) || action.kind == Some(AutomationActionKind::PairVerify)
        || action.automatic_retry_paused
        || action.send_accepted != Some(false)
        || action.messages_fenced
        || action.goal_turn_counted
    {
        return false;
    }
    let Some(text) = action.last_error.as_deref() else {
        return false;
    };
    let seconds = [
        "ChatGPT conversation loading is cooling down. Another conversation can load in ",
        "ChatGPT conversation temporarily not accessible. Shared browser cooldown: conversation loading can retry in ",
    ].into_iter().find_map(|prefix| text.strip_prefix(prefix))
        .and_then(|text| text.strip_suffix('s')).and_then(|seconds| seconds.parse::<u64>().ok());
    let Some((seconds, at)) = seconds.zip(action.updated_at_ms).filter(|(seconds, at)| {
        *seconds <= 86_400
            && (1..=9_007_199_254_740_991).contains(&at.saturating_add(seconds * 1000))
    }) else {
        return false;
    };
    action.status = AutomationActionStatus::Queued;
    action.cooldown_until_ms = Some(at.saturating_add(seconds * 1000));
    action.last_error = None;
    action.automatic_retry_in_progress = false;
    action.chatgpt_access_retry_in_progress = false;
    true
}

fn load_state(path: &Path) -> Result<AutomationTaskState, String> {
    let Some(bytes) = read_private_record(path)? else {
        return Ok(AutomationTaskState::default());
    };
    let raw: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
        "Agent automation state contains malformed JSON; saved data was preserved".to_string()
    })?;
    let legacy_goal = raw
        .get("goal")
        .is_some_and(|goal| goal.is_object() && goal.get("elapsedActiveMs").is_none());
    let legacy_receipts = raw
        .get("ownerGoalRequests")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|requests| {
            requests.iter().any(|request| {
                request
                    .pointer("/receipt/goal")
                    .is_some_and(|goal| goal.is_object() && goal.get("elapsedActiveMs").is_none())
            })
        });
    let legacy_presence_timer = raw
        .get("goal")
        .is_some_and(|goal| goal.is_object() && goal.get("timerUntilMs").is_none());
    let legacy_receipt_timers = raw
        .get("ownerGoalRequests")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|requests| {
            requests.iter().any(|request| {
                request
                    .pointer("/receipt/goal")
                    .is_some_and(|goal| goal.is_object() && goal.get("timerUntilMs").is_none())
            })
        });
    let mut state: AutomationTaskState = serde_json::from_value(raw).map_err(|_| {
        "Agent automation state does not match this build's schema; saved data was preserved"
            .to_string()
    })?;
    if (legacy_goal || legacy_presence_timer)
        && let Some(goal) = state.goal.as_mut()
    {
        let now = crate::markdown_chat::now_ms();
        // Preserve the previously reported total. Earlier offline gaps cannot be
        // reconstructed from the old aggregate; presence accounting starts here.
        goal.pause_timer(now);
        if goal.status == GoalStatus::Active {
            goal.updated_at_ms = now;
        }
    }
    if legacy_receipts || legacy_receipt_timers {
        for record in &mut state.owner_goal_requests {
            if let Some(goal) = record.receipt.goal.as_mut()
                && goal.status == GoalStatus::Active
            {
                goal.pause_timer(goal.updated_at_ms);
            }
        }
    }
    let legacy_cooldown = migrate_legacy_cooldown(&mut state);
    validate_state(&state)?;
    if legacy_goal
        || legacy_receipts
        || legacy_presence_timer
        || legacy_receipt_timers
        || legacy_cooldown
    {
        persist_state(path, &state)?;
    }
    Ok(state)
}

fn persist_state(path: &Path, state: &AutomationTaskState) -> Result<(), String> {
    validate_state(state)?;
    persist_private_record(path, state)
}

fn persist_private_record(path: &Path, record: &impl Serialize) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or("Agent automation state has no parent directory")?;
    ensure_private_directory(parent, "Agent automation task directory")?;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err("Agent automation state must be a regular non-symlink file".into());
        }
        validate_private_file(&metadata, "Agent automation state")?;
    }
    let bytes = serde_json::to_vec(record)
        .map_err(|error| format!("Could not encode agent automation state: {error}"))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("Agent automation state is too large".into());
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("Could not create agent automation state: {error}"))?;
    set_private_file_permissions(temporary.path(), "temporary agent automation state")?;
    temporary
        .write_all(&bytes)
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|error| format!("Could not write agent automation state: {error}"))?;
    temporary
        .persist(path)
        .map_err(|error| format!("Could not publish agent automation state: {}", error.error))?;
    set_private_file_permissions(path, "agent automation state")?;
    #[cfg(unix)]
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn ensure_private_directory(path: &Path, label: &str) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|error| format!("Could not create {label}: {error}"))?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect {label}: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!("{label} must be a real directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(format!("{label} must belong to the current user"));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not protect {label}: {error}"))?;
    }
    Ok(())
}

fn validate_private_file(metadata: &std::fs::Metadata, label: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(format!(
                "{label} must belong to the current user and have mode 0600"
            ));
        }
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path, label: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Could not protect {label}: {error}"))?;
    }
    Ok(())
}

fn validate_state(state: &AutomationTaskState) -> Result<(), String> {
    if state.version != STATE_VERSION {
        return Err(format!(
            "Unsupported agent automation state version {}",
            state.version
        ));
    }
    validate_pairing(&state.pairing)?;
    validate_selection(&state.selection)?;
    if let Some(goal) = &state.goal {
        validate_goal(goal)?;
    }
    let unique_mirror_ids = state
        .chat_mirror
        .message_ids
        .iter()
        .collect::<std::collections::HashSet<_>>();
    let mirror_metadata_complete = state.chat_mirror.message_ids.is_empty()
        || (state.chat_mirror.owner_generation.is_some()
            && state.chat_mirror.last_accepted_batch_id.is_some()
            && state.chat_mirror.transcript_revision_at_start.is_some());
    let empty_mirror_metadata_clean = !state.chat_mirror.message_ids.is_empty()
        || (state.chat_mirror.last_accepted_batch_id.is_none()
            && state.chat_mirror.transcript_revision_at_start.is_none());
    if state.chat_mirror.message_ids.len() > MAX_CHAT_MIRROR_IDS
        || unique_mirror_ids.len() != state.chat_mirror.message_ids.len()
        || !mirror_metadata_complete
        || !empty_mirror_metadata_clean
        || state
            .chat_mirror
            .message_ids
            .iter()
            .any(|value| !valid_opaque(value))
        || state
            .chat_mirror
            .last_accepted_batch_id
            .as_deref()
            .is_some_and(|value| !valid_opaque(value))
        || state
            .chat_mirror
            .transcript_revision_at_start
            .as_deref()
            .is_some_and(|value| !bounded_private(value))
    {
        return Err("Agent automation chat mirror state is invalid".into());
    }
    validate_action(&state.action)?;
    validate_goal_wake(&state.goal_wake)?;
    if let Some(previous) = &state.suspended_retry {
        validate_action(&previous.action)?;
        validate_goal_wake(&previous.goal_wake)?;
    }
    if state.owner_goal_requests.len() > MAX_OWNER_GOAL_REQUESTS {
        return Err("Agent automation owner goal request history is too large".into());
    }
    let mut request_digests = std::collections::HashSet::new();
    for record in &state.owner_goal_requests {
        if !valid_digest(&record.request_digest)
            || !request_digests.insert(record.request_digest.as_str())
            || !valid_digest(&record.command_digest)
            || record.receipt.replayed
        {
            return Err("Agent automation owner goal request history is invalid".into());
        }
        match (&record.receipt.kind, &record.receipt.goal) {
            (
                OwnerGoalCommandKind::Set | OwnerGoalCommandKind::Run | OwnerGoalCommandKind::Stop,
                Some(goal),
            ) => validate_goal(goal)?,
            (OwnerGoalCommandKind::Clear | OwnerGoalCommandKind::Stop, None) => {}
            (OwnerGoalCommandKind::Status, _)
            | (_, None)
            | (OwnerGoalCommandKind::Clear, Some(_)) => {
                return Err("Agent automation owner goal request receipt is invalid".into());
            }
        }
    }
    Ok(())
}

fn validate_action(action: &AutomationActionState) -> Result<(), String> {
    if action
        .cooldown_until_ms
        .is_some_and(|at| at == 0 || at > 9_007_199_254_740_991)
    {
        return Err("Browser cooldown deadline is invalid".into());
    }
    if action.automatic_retry_attempts as usize > MAX_AUTOMATIC_RETRY_ATTEMPTS
        || action.chatgpt_access_retry_attempts as usize > MAX_AUTOMATIC_RETRY_ATTEMPTS
        || action.chatgpt_access_retry_in_progress && !action.automatic_retry_in_progress
        || action
            .id
            .as_deref()
            .is_some_and(|value| !valid_opaque(value))
        || action
            .last_error
            .as_deref()
            .is_some_and(|value| !bounded_private(value))
        || action
            .last_stable_answer
            .as_deref()
            .is_some_and(|value| value.len() > MAX_PRIVATE_TEXT_BYTES)
    {
        return Err("Agent automation action state is invalid".into());
    }
    let requires_id = matches!(
        action.status,
        AutomationActionStatus::Queued
            | AutomationActionStatus::Navigating
            | AutomationActionStatus::Sending
            | AutomationActionStatus::Generating
            | AutomationActionStatus::ApprovalRequired
            | AutomationActionStatus::AuthenticationRequired
            | AutomationActionStatus::ConversationUnavailable
            | AutomationActionStatus::ConversationLimit
            | AutomationActionStatus::RateLimited
            | AutomationActionStatus::Interference
            | AutomationActionStatus::Ambiguous
            | AutomationActionStatus::Stalled
            | AutomationActionStatus::Failed
    );
    if requires_id && action.id.is_none() {
        return Err("Agent automation in-flight action has no identity".into());
    }
    if action.id.is_some() && action.kind.is_none() {
        return Err("Agent automation action identity has no kind".into());
    }
    if action.kind.is_some() && action.pairing_generation.is_none() {
        return Err("Agent automation action has no pairing generation".into());
    }
    if action.pairing_generation == Some(0) {
        return Err("Agent automation action pairing generation is invalid".into());
    }
    match (action.kind, action.goal_revision) {
        (
            Some(AutomationActionKind::OfflineGoal | AutomationActionKind::OfflineCombined),
            Some(revision),
        ) if revision > 0 => {}
        (Some(AutomationActionKind::OfflineGoal | AutomationActionKind::OfflineCombined), _)
        | (Some(AutomationActionKind::PairVerify | AutomationActionKind::OfflineChat), Some(_)) => {
            return Err("Agent automation action has inconsistent goal state".into());
        }
        _ => {}
    }
    Ok(())
}

fn validate_goal_wake(wake: &GoalWakeState) -> Result<(), String> {
    if wake.recovery_attempts as usize > MAX_AUTOMATIC_RETRY_ATTEMPTS {
        return Err("Agent automation goal recovery count is invalid".into());
    }
    for value in [wake.activity_epoch.as_deref(), wake.nudged_epoch.as_deref()]
        .into_iter()
        .flatten()
    {
        if !valid_opaque(value) {
            return Err("Agent automation goal wake state is invalid".into());
        }
    }
    if wake.activity_epoch.is_some() != wake.activity_sequence.is_some()
        || wake.nudged_epoch.is_some() != wake.nudged_sequence.is_some()
        || wake.nudged_epoch.is_some() != wake.last_nudged_at_ms.is_some()
    {
        return Err("Agent automation goal wake state is incomplete".into());
    }
    if wake.nudged_epoch.is_some()
        && (wake.nudged_epoch != wake.activity_epoch
            || wake.nudged_sequence != wake.activity_sequence)
    {
        return Err("Agent automation goal wake state is inconsistent".into());
    }
    Ok(())
}

fn validate_pairing(pairing: &PairingState) -> Result<(), String> {
    if pairing
        .connector_schema_version
        .as_deref()
        .is_some_and(|version| !version.strip_prefix("schema-v1:").is_some_and(valid_digest))
    {
        return Err("Agent automation connector schema version is invalid".into());
    }
    if pairing
        .account_fingerprint
        .as_deref()
        .is_some_and(|value| !valid_opaque(value))
    {
        return Err("Agent automation pairing account fingerprint is invalid".into());
    }
    for url in [
        pairing.candidate_url.as_deref(),
        pairing.canonical_url.as_deref(),
        pairing.last_attempted_url.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        normalize_chatgpt_url(url)?;
    }
    if pairing.verified_owner_generation.is_some() != pairing.verified_at_ms.is_some() {
        return Err("Agent automation pairing verification state is incomplete".into());
    }
    if pairing.verified_owner_generation.is_some() && pairing.canonical_url.is_none() {
        return Err("Agent automation pairing is verified without a canonical URL".into());
    }
    if pairing.verified_owner_generation.is_some()
        && (pairing.pending.is_some() || pairing.candidate_url.is_some())
    {
        return Err("Agent automation pairing cannot be pending and verified together".into());
    }
    if let Some(pending) = &pairing.pending
        && (pending.generation == 0
            || pending.generation != pairing.generation
            || !valid_digest(&pending.reference_digest)
            || pairing.candidate_url.is_none()
            || pending.expires_at_ms == 0)
    {
        return Err("Agent automation pending pairing state is invalid".into());
    }
    Ok(())
}

fn validate_selection(selection: &SelectionState) -> Result<(), String> {
    let values = [
        selection.model.as_deref(),
        selection.effort.as_deref(),
        selection.catalog_revision.as_deref(),
        selection.account_fingerprint.as_deref(),
    ];
    let populated = values.iter().filter(|value| value.is_some()).count();
    if populated != 0 && populated != values.len() {
        return Err("Agent automation selection state is incomplete".into());
    }
    for value in values.into_iter().flatten() {
        if !valid_opaque(value) {
            return Err("Agent automation selection state is invalid".into());
        }
    }
    Ok(())
}

fn validate_goal(goal: &GoalRecord) -> Result<(), String> {
    if goal.revision == 0
        || (goal.status != GoalStatus::Active && goal.active_since_ms.is_some())
        || goal.active_since_ms.is_some() != goal.timer_until_ms.is_some()
        || goal
            .active_since_ms
            .zip(goal.timer_until_ms)
            .is_some_and(|(since, until)| since > until)
        || goal.markdown.trim().is_empty()
        || goal.markdown.len() > MAX_GOAL_BYTES
        || goal
            .summary
            .as_deref()
            .is_some_and(|value| value.len() > MAX_GOAL_RESULT_BYTES)
        || goal
            .blocked_reason
            .as_deref()
            .is_some_and(|value| value.trim().is_empty() || value.len() > MAX_GOAL_RESULT_BYTES)
    {
        return Err("Agent automation goal state is invalid".into());
    }
    match goal.status {
        GoalStatus::Configured | GoalStatus::Active => {
            if goal.finished_at_ms.is_some()
                || goal.summary.is_some()
                || goal.blocked_reason.is_some()
            {
                return Err("Nonterminal agent automation goal contains terminal data".into());
            }
        }
        GoalStatus::Completed => {
            if goal.finished_at_ms.is_none() || goal.blocked_reason.is_some() {
                return Err("Completed agent automation goal state is invalid".into());
            }
        }
        GoalStatus::Blocked => {
            if goal.finished_at_ms.is_none()
                || goal.summary.is_some()
                || goal.blocked_reason.is_none()
            {
                return Err("Blocked agent automation goal state is invalid".into());
            }
        }
        GoalStatus::Error => {}
    }
    Ok(())
}

fn validate_goal_markdown(markdown: &str) -> Result<(), String> {
    if markdown.trim().is_empty() {
        return Err("Agent-chat goal Markdown must not be empty".into());
    }
    if markdown.len() > MAX_GOAL_BYTES {
        return Err(format!(
            "Agent-chat goal Markdown must not exceed {MAX_GOAL_BYTES} UTF-8 bytes"
        ));
    }
    Ok(())
}

fn validate_accepted_batch(
    action_id: &str,
    batch_id: &str,
    message_ids: &[String],
    transcript_revision: &str,
) -> Result<(), String> {
    if !valid_opaque(action_id)
        || !valid_opaque(batch_id)
        || message_ids.is_empty()
        || message_ids.len() > MAX_CHAT_MIRROR_IDS
        || message_ids.iter().any(|id| !valid_opaque(id))
        || !bounded_private(transcript_revision)
    {
        return Err("Agent automation accepted chat batch is invalid".into());
    }
    Ok(())
}

fn merge_accepted_batch(
    mirror: &mut ChatMirrorState,
    owner_generation: u64,
    batch_id: &str,
    message_ids: &[String],
    transcript_revision: &str,
) -> Result<(), String> {
    let mut accepted = if mirror.owner_generation == Some(owner_generation) {
        mirror.message_ids.clone()
    } else {
        Vec::new()
    };
    for id in message_ids {
        if !accepted.contains(id) {
            accepted.push(id.clone());
        }
    }
    if accepted.len() > MAX_CHAT_MIRROR_IDS {
        return Err("Agent automation accepted chat history is full".into());
    }
    mirror.owner_generation = Some(owner_generation);
    mirror.last_accepted_batch_id = Some(batch_id.to_string());
    mirror.message_ids = accepted;
    mirror.transcript_revision_at_start = Some(transcript_revision.to_string());
    Ok(())
}

pub(crate) fn conversation_url_for_retry(value: &str) -> Option<&str> {
    super::companion::canonical_chatgpt_conversation_url(value)
        .ok()
        .filter(|canonical| canonical.starts_with("https://chatgpt.com/c/"))
        .map(|_| value)
}

fn normalize_chatgpt_url(value: &str) -> Result<String, String> {
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        return Err("ChatGPT conversation URL is invalid".into());
    }
    let mut url = reqwest::Url::parse(value).map_err(|_| "ChatGPT conversation URL is invalid")?;
    if url.scheme() != "https"
        || url.host_str() != Some("chatgpt.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
        || url.path() == "/"
    {
        return Err("ChatGPT conversation URL must use the https://chatgpt.com origin".into());
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

fn generate_reference() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| format!("Could not generate an agent-chat pairing reference: {error}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn validate_reference(reference: &str) -> Result<(), String> {
    if reference.len() != 43
        || URL_SAFE_NO_PAD
            .decode(reference)
            .ok()
            .is_none_or(|bytes| bytes.len() != 32)
    {
        return Err("The agent-chat pairing reference is invalid, expired, or already used".into());
    }
    Ok(())
}

fn reference_digest(reference: &str) -> String {
    format!("{:x}", Sha256::digest(reference.as_bytes()))
}

fn owner_goal_request_digest(request_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"codexify-owner-goal-request-v1\0");
    hash.update(request_id.as_bytes());
    format!("{:x}", hash.finalize())
}

fn owner_goal_command_digest(command: &OwnerGoalCommand) -> Result<String, String> {
    let encoded = serde_json::to_vec(command)
        .map_err(|_| "Could not encode the agent-chat goal command".to_string())?;
    let mut hash = Sha256::new();
    hash.update(b"codexify-owner-goal-command-v1\0");
    hash.update(encoded);
    Ok(format!("{:x}", hash.finalize()))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_opaque(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.contains('\0')
        && !value.chars().any(char::is_control)
}

fn bounded_private(value: &str) -> bool {
    value.len() <= MAX_PRIVATE_TEXT_BYTES && !value.contains('\0')
}

#[cfg(test)]
mod schema_rollover_tests {
    use super::*;

    fn verified_idle_pairing() -> (
        tempfile::TempDir,
        AgentAutomationStore,
        ConversationIdentity,
    ) {
        let root = tempfile::tempdir().unwrap();
        let store = AgentAutomationStore::new(root.path().join("state")).unwrap();
        let task = ConversationIdentity::from_openai_session("idle-schema-rollover").unwrap();
        let url = "https://chatgpt.com/c/idle-source";
        let issue = store.issue_pairing(&task, url, 1, 100).unwrap();
        store
            .record_pairing_canonical_url(&task, issue.generation, url, 101)
            .unwrap();
        store
            .verify_pairing(&task, issue.generation, 1, &issue.reference, 102)
            .unwrap();
        store
            .mutate(&task, |state| {
                state.action = AutomationActionState::default();
                state.pairing.schema_rollover_pending = true;
                Ok(())
            })
            .unwrap();
        (root, store, task)
    }

    #[test]
    fn schema_rollover_uses_the_pairing_snapshot_when_the_action_is_idle() {
        let (_root, store, task) = verified_idle_pairing();
        let expected = store.load(&task).unwrap();
        assert_eq!(expected.action.pairing_generation, None);
        let issue = store
            .issue_schema_rollover_pairing(&task, "https://chatgpt.com/new", 2, 200, &expected)
            .unwrap()
            .expect("a verified idle pairing must roll over");
        let replacing = store.load(&task).unwrap();
        assert_eq!(issue.generation, 2);
        assert_eq!(
            replacing.pairing.pending.as_ref().unwrap().owner_generation,
            2
        );
        assert!(replacing.pairing.creating_chat);
        assert!(replacing.pairing.schema_rollover_pending);
        assert_eq!(replacing.selection, expected.selection);
        assert!(
            store
                .issue_schema_rollover_pairing(&task, "https://chatgpt.com/new", 2, 201, &expected)
                .unwrap()
                .is_none(),
            "replaying the old snapshot must not create another replacement"
        );
    }

    #[test]
    fn idle_schema_rollover_still_rejects_stale_actions_pairings_and_owners() {
        for changed in ["action", "pairing", "owner"] {
            let (_root, store, task) = verified_idle_pairing();
            let expected = store.load(&task).unwrap();
            if changed != "owner" {
                store
                    .mutate(&task, |state| {
                        if changed == "action" {
                            state.action.updated_at_ms = Some(150);
                        } else {
                            state.pairing.schema_rollover_pending = false;
                        }
                        Ok(())
                    })
                    .unwrap();
            }
            let path = store.task_directory(&task).unwrap().join(STATE_FILE);
            let before = std::fs::read(&path).unwrap();
            assert!(
                store
                    .issue_schema_rollover_pairing(
                        &task,
                        "https://chatgpt.com/new",
                        if changed == "owner" { 3 } else { 2 },
                        200,
                        &expected
                    )
                    .unwrap()
                    .is_none(),
                "{changed}"
            );
            assert_eq!(std::fs::read(path).unwrap(), before, "{changed}");
        }
    }
}
