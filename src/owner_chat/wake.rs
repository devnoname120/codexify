use crate::agent_automation::{AutomationActionKind, AutomationActionStatus, AutomationTaskState};
use crate::markdown_chat::{OFFLINE_AFTER_MS, OwnerChatSummary};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub(super) struct WakeStatus {
    phase: &'static str,
    label: &'static str,
    description: &'static str,
}

#[derive(Default)]
pub(super) struct Activity {
    offline_after_ms: Option<u64>,
    last_call: Option<u64>,
    last_user: Option<u64>,
    stopped: bool,
    waiting_until: Option<u64>,
    idle_until: Option<u64>,
    presence_until: Option<u64>,
}

impl From<&OwnerChatSummary> for Activity {
    fn from(summary: &OwnerChatSummary) -> Self {
        Self {
            offline_after_ms: None,
            last_call: summary.last_agent_call_at_ms,
            last_user: summary.last_user_entry_at_ms,
            stopped: summary.agent_stop_requested,
            waiting_until: summary.agent_waiting_until_ms,
            idle_until: summary.agent_idle_until_ms,
            presence_until: summary.agent_presence_until_ms,
        }
    }
}

impl Activity {
    pub fn with_offline_timeout(mut self, timeout: u64) -> Self {
        self.offline_after_ms = Some(timeout);
        self
    }

    pub fn offline(&self, now: u64) -> bool {
        !self.stopped
            && ![self.waiting_until, self.idle_until]
                .into_iter()
                .flatten()
                .any(|until| now < until)
            && self.presence_until.map_or_else(
                || {
                    self.last_call.is_none_or(|last| {
                        now.saturating_sub(last)
                            >= self.offline_after_ms.unwrap_or(OFFLINE_AFTER_MS)
                    })
                },
                |until| now >= until,
            )
    }
}

pub(super) fn status(
    state: &AutomationTaskState,
    activity: &Activity,
    queued_messages: bool,
    enabled: bool,
    now: u64,
) -> Option<WakeStatus> {
    if !enabled
        || !activity.offline(now)
        || state.pairing.verified_owner_generation.is_none()
        || state.action_needs_attention_at(now)
        || state.action.status == AutomationActionStatus::Cancelled
    {
        return None;
    }
    // This receipt is immutable across later action-status updates. An ambiguous
    // action's sendAccepted flag alone is not proof that ChatGPT accepted Send.
    let sent = state.turn_renewal.send.as_ref().filter(|send| {
        state.action.id.as_deref() == Some(send.action_id.as_str())
            && send.pairing_generation == state.pairing.generation
            && Some(send.owner_generation) == state.pairing.verified_owner_generation
    });
    if sent.is_some_and(|send| activity.last_call.is_none_or(|last| last < send.at_ms)) {
        return Some(WakeStatus {
            phase: "waiting",
            label: "Sent · waiting for agent",
            description: "Message sent to ChatGPT; waiting for the agent's first tool call.",
        });
    }
    if state.approval_resume.as_ref().is_some_and(|resume| {
        resume.pairing_generation == state.pairing.generation
            && Some(resume.owner_generation) == state.pairing.verified_owner_generation
            && activity
                .last_call
                .is_none_or(|last| last < resume.resumed_at_ms)
    }) {
        return Some(WakeStatus {
            phase: "waiting",
            label: "Waiting for agent",
            description: "Conversation resumed after approval; waiting for the agent's next tool call.",
        });
    }
    if state.action.kind != Some(AutomationActionKind::PairVerify) {
        match state.action.status {
            AutomationActionStatus::Queued => return Some(queued()),
            AutomationActionStatus::Navigating | AutomationActionStatus::Sending => {
                return Some(WakeStatus {
                    phase: "sending",
                    label: "Sending to ChatGPT",
                    description: "Opening the conversation and submitting the wake-up message to ChatGPT.",
                });
            }
            _ => {}
        }
    }
    if queued_messages
        || activity
            .last_user
            .is_some_and(|user| activity.last_call.is_none_or(|last| user > last))
    {
        return Some(queued());
    }
    None
}

fn queued() -> WakeStatus {
    WakeStatus {
        phase: "queued",
        label: "Queued to wake",
        description: "Message queued to wake the agent; submission to ChatGPT has not been confirmed yet.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_automation::turn_renewal::TurnSend;

    #[test]
    fn wake_indicators_follow_submission_proof_and_never_resurrect_after_activity() {
        let mut state = AutomationTaskState::default();
        state.pairing.verified_owner_generation = Some(0);
        state.action.id = Some("wake-action".into());
        state.action.kind = Some(AutomationActionKind::OfflineGoal);
        state.action.status = AutomationActionStatus::Queued;
        let mut activity = Activity::default();
        assert_eq!(
            status(&state, &activity, false, true, 1_000_000)
                .unwrap()
                .phase,
            "queued"
        );
        state.action.status = AutomationActionStatus::Sending;
        assert_eq!(
            status(&state, &activity, false, true, 1_000_000)
                .unwrap()
                .phase,
            "sending"
        );
        state.action.status = AutomationActionStatus::Idle;
        state.action.send_accepted = Some(true);
        assert!(
            status(&state, &activity, false, true, 1_000_000).is_none(),
            "a flag alone is not a Send receipt"
        );
        state.turn_renewal.send = Some(TurnSend {
            action_id: "wake-action".into(),
            at_ms: 1000,
            model: None,
            effort: None,
            pairing_generation: state.pairing.generation,
            owner_generation: 0,
        });
        assert_eq!(
            status(&state, &activity, false, true, 1_000_000)
                .unwrap()
                .phase,
            "waiting"
        );
        state.action.updated_at_ms = Some(999_999);
        activity.last_call = Some(1001);
        assert!(
            status(&state, &activity, false, true, 1_000_000).is_none(),
            "later idle must not resurrect the spinner"
        );
        activity.last_call = None;
        state.action.status = AutomationActionStatus::Failed;
        state.action.last_error = Some("Send failed".into());
        assert!(status(&state, &activity, false, true, 1_000_000).is_none());
    }

    #[test]
    fn queued_messages_show_pending_but_online_waiting_stopped_and_disabled_chats_do_not() {
        let mut state = AutomationTaskState::default();
        state.pairing.verified_owner_generation = Some(0);
        let mut activity = Activity::default();
        assert_eq!(
            status(&state, &activity, true, true, 1_000_000)
                .unwrap()
                .phase,
            "queued"
        );
        assert!(status(&state, &activity, true, false, 1_000_000).is_none());
        activity.stopped = true;
        assert!(status(&state, &activity, true, true, 1_000_000).is_none());
        activity.stopped = false;
        activity.waiting_until = Some(1_000_001);
        assert!(status(&state, &activity, true, true, 1_000_000).is_none());
        activity.waiting_until = None;
        activity.last_call = Some(999_999);
        assert!(status(&state, &activity, true, true, 1_000_000).is_none());
    }
}
