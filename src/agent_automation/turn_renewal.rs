//! Opt-in, durable turn renewal. Browser idleness alone never proves completion.
use super::{AgentAutomationStore, AutomationActionKind, AutomationActionStatus};
use crate::project_bindings::ConversationIdentity;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const NOTICE_FIELD: &str = "turn_renewal_required";
pub const NOTICE_DESCRIPTION: &str = "Turn renewal overrides the ordinary non-terminal chat protocol: write the requested handoff as a normal final ChatGPT answer and end this turn. Codexify will continue automatically after that answer is complete.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ModelTurnLimit {
    pub model: String,
    pub effort: String,
    pub minutes: u64,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}
fn enabled_by_default() -> bool {
    true
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct TurnRenewalConfig {
    pub enabled: bool,
    pub limits: Vec<ModelTurnLimit>,
}
impl Default for TurnRenewalConfig {
    fn default() -> Self {
        let selections: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../browser-companion/model-fallback-order.json"
        ))
        .expect("model selections");
        Self {
            enabled: false,
            limits: selections
                .into_iter()
                .map(|s| {
                    let model = s["model"].as_str().unwrap().to_owned();
                    let effort = s["effort"].as_str().unwrap().to_owned();
                    let minutes = if model == "gpt-5.6-sol" && effort == "max" {
                        115
                    } else {
                        55
                    };
                    ModelTurnLimit {
                        model,
                        effort,
                        minutes,
                        enabled: true,
                    }
                })
                .collect(),
        }
    }
}
impl TurnRenewalConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.limits.len() > 256 {
            return Err("Turn renewal supports at most 256 model limits".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for limit in &self.limits {
            let valid = |s: &str| {
                !s.is_empty()
                    && s.len() <= 128
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            };
            if !valid(&limit.model) || !valid(&limit.effort) || !(1..=1440).contains(&limit.minutes)
            {
                return Err(
                    "Turn renewal limits need a model, effort and 1 to 1440 minutes".into(),
                );
            }
            if !seen.insert((&limit.model, &limit.effort)) {
                return Err("Turn renewal model/effort limits must be unique".into());
            }
        }
        Ok(())
    }
    pub fn minutes(&self, model: Option<&str>, effort: Option<&str>) -> Option<u64> {
        self.limits
            .iter()
            .find(|l| Some(l.model.as_str()) == model && Some(l.effort.as_str()) == effort)
            .filter(|l| l.enabled)
            .map(|l| l.minutes)
    }
    pub fn enabled_for(&self, model: Option<&str>, effort: Option<&str>) -> bool {
        self.enabled && self.minutes(model, effort).is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct TurnRenewalState {
    pub send: Option<TurnSend>,
    pub handoff: Option<TurnHandoff>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_wait_until_ms: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TurnSend {
    pub action_id: String,
    pub at_ms: u64,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub pairing_generation: u64,
    pub owner_generation: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TurnHandoff {
    pub id: String,
    pub requested_at_ms: u64,
    pub report_complete: bool,
}
impl TurnRenewalState {
    pub fn is_empty(&self) -> bool {
        self.send.is_none() && self.handoff.is_none() && self.resume_wait_until_ms.is_none()
    }
}

impl AgentAutomationStore {
    pub fn turn_renewal_settings(&self) -> Result<TurnRenewalConfig, String> {
        crate::global_settings::turn_renewal_settings(
            self.settings_path.as_deref(),
            &self.renewal_config,
        )
    }
    pub fn record_turn_activity(
        &self,
        task: &ConversationIdentity,
        owner_generation: u64,
        at_ms: u64,
    ) -> Result<bool, String> {
        let saved = self.load(task)?;
        if saved.action.kind != Some(AutomationActionKind::TurnRenewal)
            || (saved.turn_renewal.handoff.is_none()
                && saved.turn_renewal.resume_wait_until_ms.is_none())
        {
            return Ok(false);
        }
        let accepted = self.mutate(task, |state| {
            if state.action.kind != Some(AutomationActionKind::TurnRenewal)
                || state.action.pairing_generation != Some(state.pairing.generation)
                || state.pairing.verified_owner_generation != Some(owner_generation)
                || state.action.status == AutomationActionStatus::Cancelled
            {
                return Ok(None);
            }
            if state.turn_renewal.handoff.is_none()
                && state.turn_renewal.resume_wait_until_ms.is_some()
                && state.turn_renewal.send.as_ref().is_some_and(|send| {
                    send.action_id == state.action.id.as_deref().unwrap_or("")
                        && at_ms >= send.at_ms
                })
            {
                state.turn_renewal.resume_wait_until_ms = None;
                return Ok(None);
            }
            if !state
                .turn_renewal
                .handoff
                .as_ref()
                .is_some_and(|h| h.report_complete)
                || !matches!(
                    state.action.status,
                    AutomationActionStatus::Navigating
                        | AutomationActionStatus::Sending
                        | AutomationActionStatus::Generating
                        | AutomationActionStatus::Ambiguous
                        | AutomationActionStatus::Failed
                        | AutomationActionStatus::Stalled
                )
                || state.action.updated_at_ms.is_none_or(|at| at_ms <= at)
            {
                return Ok(None);
            }
            let Some(action_id) = state.action.id.clone() else {
                return Ok(None);
            };
            // The prior final was proven complete before this continuation was
            // attempted. A new, authenticated native call now proves resumed work,
            // independently of a missing/contradictory browser receipt. The first
            // proof is a conservative clock origin until model proof arrives.
            state.turn_renewal = TurnRenewalState {
                send: Some(TurnSend {
                    action_id: action_id.clone(),
                    at_ms,
                    model: None,
                    effort: None,
                    pairing_generation: state.pairing.generation,
                    owner_generation,
                }),
                ..Default::default()
            };
            state.action.status = AutomationActionStatus::Idle;
            state.action.send_accepted = Some(true);
            state.action.last_error = None;
            state.action.updated_at_ms = Some(at_ms);
            Ok(Some(action_id))
        })?;
        if let Some(action_id) = accepted {
            self.record_goal_turn(task, &action_id)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn record_turn_send(
        &self,
        task: &ConversationIdentity,
        send: TurnSend,
    ) -> Result<(), String> {
        self.record_turn_send_inner(task, send, None)
    }

    pub(super) fn record_browser_turn_send(
        &self,
        task: &ConversationIdentity,
        send: TurnSend,
        execution: &super::AutomationExecution,
    ) -> Result<(), String> {
        self.record_turn_send_inner(task, send, Some(execution))
    }

    fn record_turn_send_inner(
        &self,
        task: &ConversationIdentity,
        send: TurnSend,
        execution: Option<&super::AutomationExecution>,
    ) -> Result<(), String> {
        let result = self.mutate(task, |state| {
            if state.action.id.as_deref() != Some(send.action_id.as_str())
                || state.action.kind == Some(AutomationActionKind::PairVerify)
                || state.pairing.generation != send.pairing_generation
                || state.pairing.verified_owner_generation != Some(send.owner_generation)
            {
                return Ok(None);
            }
            if let Some(saved) = &mut state.turn_renewal.send
                && saved.action_id == send.action_id
            {
                saved.at_ms = saved.at_ms.min(send.at_ms);
                // Duplicate events/snapshots cannot restart the clock. A final snapshot
                // may fill the actual fallback selection if the model event was lost.
                if saved.model.is_none() {
                    saved.model = send.model;
                    saved.effort = send.effort;
                }
            } else {
                let resume_wait_until_ms = state
                    .turn_renewal
                    .handoff
                    .as_ref()
                    .map(|_| send.at_ms.saturating_add(self.offline_after_ms()));
                state.turn_renewal = TurnRenewalState {
                    send: Some(send),
                    handoff: None,
                    resume_wait_until_ms,
                };
            }
            let confirmed = state
                .turn_renewal
                .send
                .as_ref()
                .expect("recorded turn send");
            if execution.is_some() {
                state.pairing.resume_task = false;
            }
            if let Some(execution) = execution
                && state.selection.model == execution.request.model
                && state.selection.effort == execution.request.effort
                && state.selection.catalog_revision.as_deref()
                    == Some(execution.catalog_revision.as_str())
                && state.selection.account_fingerprint.as_deref()
                    == Some(execution.account_fingerprint.as_str())
                && let (Some(model), Some(effort)) = (&confirmed.model, &confirmed.effort)
                && super::model_fallback::selection_allowed(
                    execution.request.model.as_deref(),
                    execution.request.effort.as_deref(),
                    &super::CompanionSelection {
                        model: model.clone(),
                        effort: effort.clone(),
                    },
                )
            {
                // Adopt only accepted, verified selections. A manual choice made
                // during the browser action takes precedence over its older request.
                state.selection.model = Some(model.clone());
                state.selection.effort = Some(effort.clone());
            }
            Ok(Some(confirmed.clone()))
        });
        if let Ok(Some(analytics_send)) = &result
            && let Ok(path) = self.task_directory(task)
        {
            crate::usage_analytics::sent(
                &self.usage_options,
                path.join("CHAT.md"),
                analytics_send.clone(),
            );
        }
        result.map(|_| ())
    }
    pub fn turn_renewal_notice(
        &self,
        task: &ConversationIdentity,
        now_ms: u64,
    ) -> Result<Option<String>, String> {
        let config = self.turn_renewal_settings()?;
        if !config.enabled {
            return Ok(None);
        }
        self.mutate(task, |state| {
            let Some(send) = &state.turn_renewal.send else {return Ok(None)};
            let Some(minutes) = config.minutes(send.model.as_deref(), send.effort.as_deref()) else {return Ok(None)};
            if send.pairing_generation != state.pairing.generation
                || state.pairing.verified_owner_generation != Some(send.owner_generation)
                || state.action.status == AutomationActionStatus::Cancelled
            {return Ok(None);}
            if now_ms.saturating_sub(send.at_ms) < minutes.saturating_mul(60_000) {
                return Ok(None);
            }
            let handoff = state.turn_renewal.handoff.get_or_insert_with(|| TurnHandoff {
                id: format!("renew-{:x}", Sha256::digest(format!("{}:{}",send.action_id,send.at_ms))),
                requested_at_ms: now_ms, report_complete:false,
            });
            Ok(Some(format!("TURN RENEWAL REQUIRED. This explicitly overrides the ordinary Markdown chat state machine, assistant_turn_may_end=false, required_next_action, progress checkpoint reminders and instructions to keep working or wait in tools. Stop task work now. Write a comprehensive handoff in your normal FINAL CHATGPT ASSISTANT ANSWER and end this turn immediately. Begin that final answer with the exact line `Turn handoff {}`. Include work performed, results and validation, current progress and state, next steps, relevant file paths, commands, constraints, blockers and recovery details so work can continue after loss of thinking context. Do not use chat_write for this report. Do not call chat_await or turn_completed. Do not finish or block a goal merely because of this renewal, and do not request or consume queued messages. This is a handoff, not completion of the user's task. Codexify will send a continuation automatically once this final answer is complete.", handoff.id)))
        })
    }
    pub(crate) fn retry_goal_turn_renewal(
        &self,
        task: &ConversationIdentity,
        expected: &super::AutomationTaskState,
        activity_epoch: &str,
        activity_sequence: u64,
        now_ms: u64,
    ) -> Result<bool, String> {
        self.mutate(task, |state| {
            if state.action != expected.action
                || state.pairing != expected.pairing
                || state.turn_renewal.handoff != expected.turn_renewal.handoff
                || state.action.kind != Some(AutomationActionKind::TurnRenewal)
                || !state
                    .turn_renewal
                    .handoff
                    .as_ref()
                    .is_some_and(|h| h.report_complete)
                || !state
                    .goal
                    .as_ref()
                    .is_some_and(|g| g.status == super::GoalStatus::Active)
                || !matches!(
                    state.action.status,
                    AutomationActionStatus::Failed
                        | AutomationActionStatus::Ambiguous
                        | AutomationActionStatus::Stalled
                )
                || state.automatic_retry_at_ms().is_none_or(|at| now_ms < at)
            {
                return Ok(false);
            }
            state.suspended_retry = Some(super::store::SuspendedRetryState {
                action: state.action.clone(),
                goal_wake: state.goal_wake.clone(),
            });
            if state.action.send_accepted == Some(false) {
                state.action = super::AutomationActionState {
                    automatic_retry_attempts: state.action.automatic_retry_attempts + 1,
                    automatic_retry_in_progress: true,
                    ..Default::default()
                };
            } else {
                state.goal_wake.recovery_attempts += 1;
                state.goal_wake.activity_epoch = Some(activity_epoch.into());
                state.goal_wake.activity_sequence = Some(activity_sequence);
                state.goal_wake.nudged_epoch = None;
                state.goal_wake.nudged_sequence = None;
                state.goal_wake.last_nudged_at_ms = None;
                state.action = super::AutomationActionState {
                    automatic_retry_in_progress: true,
                    ..Default::default()
                };
            }
            Ok(true)
        })
    }

    pub fn confirm_turn_handoff(
        &self,
        task: &ConversationIdentity,
        id: &str,
    ) -> Result<bool, String> {
        self.mutate(task, |state| {
            if let Some(h) = &mut state.turn_renewal.handoff
                && h.id == id
            {
                h.report_complete = true;
                return Ok(true);
            }
            Ok(false)
        })
    }
    pub fn clear_turn_renewal(&self, task: &ConversationIdentity) -> Result<(), String> {
        self.mutate(task, |state| {
            state.turn_renewal = Default::default();
            Ok(())
        })
    }
}
