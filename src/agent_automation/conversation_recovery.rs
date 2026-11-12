//! Opt-in replacement of repeatedly inaccessible ChatGPT conversations.
use super::{AgentAutomationStore, AutomationActionStatus, AutomationTaskState, GoalStatus};
use crate::project_bindings::ConversationIdentity;
use serde::{Deserialize, Serialize};

pub const UNAVAILABLE_CONVERSATION_MESSAGE: &str =
    "ChatGPT conversation temporarily not accessible";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ConversationRecoveryState {
    pub enabled: bool,
    pub failure_count: u32,
    pub last_failure_action_id: Option<String>,
    pub first_failure_at_ms: Option<u64>,
    pub failed_url: Option<String>,
    pub last_recovery_at_ms: Option<u64>,
    pub previous_urls: Vec<String>,
    pub failed_chat_count: u32,
}

impl ConversationRecoveryState {
    pub(crate) fn automatic_replacement_due(&self, now_ms: u64) -> bool {
        self.enabled
            && self.failure_count >= 3
            && self.first_failure_at_ms.is_some_and(|first| now_ms.saturating_sub(first) >= 60_000)
            // Initial setup retries use the same timestamp but have not replaced
            // a broken conversation. Only an actual replacement starts the
            // anti-churn interval; account/activity/send guards still apply.
            && ((self.failed_chat_count == 0 && self.previous_urls.is_empty())
                || self.last_recovery_at_ms.is_none_or(|last| now_ms.saturating_sub(last) >= 30 * 60_000))
    }
}

pub fn unavailable_conversation(state: &AutomationTaskState) -> bool {
    state.action.status == AutomationActionStatus::ConversationUnavailable
        && state.action.last_error.as_deref() == Some(UNAVAILABLE_CONVERSATION_MESSAGE)
        && state.action.send_accepted == Some(false)
        && !state.pairing.creating_chat
        && state.pairing.pending.is_none()
        && state.pairing.canonical_url.is_some()
        && state
            .goal
            .as_ref()
            .is_some_and(|goal| goal.status == GoalStatus::Active)
}

pub fn interrupted_replacement_creation(state: &AutomationTaskState) -> bool {
    state.pairing.creating_chat
        && !state.conversation_recovery.previous_urls.is_empty()
        && state.pairing.pending.is_none()
        && state.pairing.canonical_url.is_none()
        && state.action.id.is_none()
        && state.action.send_accepted.is_none()
        && state.action.last_error.is_none()
        && state.action.status == AutomationActionStatus::Unverified
        && state
            .goal
            .as_ref()
            .is_some_and(|goal| goal.status == GoalStatus::Active)
}

impl AgentAutomationStore {
    pub fn record_interrupted_replacement_retry(
        &self,
        task: &ConversationIdentity,
        now_ms: u64,
    ) -> Result<(), String> {
        self.mutate(task, |state| {
            if !interrupted_replacement_creation(state) {
                return Err(
                    "Replacement creation changed or may have sent; it was not replayed".into(),
                );
            }
            state.conversation_recovery.last_recovery_at_ms = Some(now_ms);
            Ok(())
        })
    }
    pub fn record_conversation_creation_retry(
        &self,
        task: &ConversationIdentity,
        action_id: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.mutate(task, |state| {
            if state.action.id.as_deref() != Some(action_id)
                || !state.pairing.creating_chat
                || state.action.send_accepted != Some(false)
            {
                return Err("Creation changed or may have sent; it was not replayed".into());
            }
            state.conversation_recovery.last_recovery_at_ms = Some(now_ms);
            Ok(())
        })
    }
    pub fn set_conversation_recovery_enabled(
        &self,
        task: &ConversationIdentity,
        enabled: bool,
    ) -> Result<(), String> {
        self.mutate(task, |state| {
            state.conversation_recovery.enabled = enabled;
            Ok(())
        })
    }

    pub fn observe_unavailable_conversation(
        &self,
        task: &ConversationIdentity,
        now_ms: u64,
    ) -> Result<ConversationRecoveryState, String> {
        let saved = self.load(task)?;
        let recovered = saved.conversation_recovery.failure_count > 0
            && saved.action.status == AutomationActionStatus::Idle
            && saved.action.send_accepted == Some(true)
            && saved.pairing.canonical_url == saved.conversation_recovery.failed_url;
        if recovered {
            return self.mutate(task, |state| {
                if state.action.status == AutomationActionStatus::Idle
                    && state.action.send_accepted == Some(true)
                    && state.pairing.canonical_url == state.conversation_recovery.failed_url
                {
                    state.conversation_recovery.failure_count = 0;
                    state.conversation_recovery.first_failure_at_ms = None;
                    state.conversation_recovery.failed_url = None;
                    state.conversation_recovery.last_failure_action_id = None;
                }
                Ok(state.conversation_recovery.clone())
            });
        }
        if !unavailable_conversation(&saved)
            || saved.conversation_recovery.last_failure_action_id == saved.action.id
        {
            return Ok(saved.conversation_recovery);
        }
        self.mutate(task, |state| {
            if !unavailable_conversation(state)
                || state.conversation_recovery.last_failure_action_id == state.action.id
            {
                return Ok(state.conversation_recovery.clone());
            }
            let recovery = &mut state.conversation_recovery;
            if recovery.failed_url != state.pairing.canonical_url {
                recovery.failure_count = 0;
                recovery.first_failure_at_ms = None;
            }
            recovery.failed_url = state.pairing.canonical_url.clone();
            recovery.last_failure_action_id = state.action.id.clone();
            recovery.failure_count = recovery.failure_count.saturating_add(1);
            recovery
                .first_failure_at_ms
                .get_or_insert(state.action.updated_at_ms.unwrap_or(now_ms));
            Ok(recovery.clone())
        })
    }

    pub fn reserve_conversation_recovery(
        &self,
        task: &ConversationIdentity,
        action_id: &str,
        now_ms: u64,
        automatic: bool,
    ) -> Result<String, String> {
        self.mutate(task, |state| {
            if state.action.id.as_deref() != Some(action_id) || !unavailable_conversation(state) {
                return Err(
                    "The failed conversation changed before recovery; nothing was replaced".into(),
                );
            }
            let recovery = &mut state.conversation_recovery;
            if automatic && !recovery.automatic_replacement_due(now_ms) {
                return Err("Waiting for repeated conversation failures before replacement".into());
            }
            let url = state
                .pairing
                .canonical_url
                .clone()
                .ok_or("The broken conversation has no saved URL")?;
            if !recovery.previous_urls.contains(&url) {
                recovery.previous_urls.push(url.clone());
                recovery.failed_chat_count = recovery.failed_chat_count.saturating_add(1);
            }
            if recovery.previous_urls.len() > 8 {
                recovery.previous_urls.remove(0);
            }
            recovery.last_recovery_at_ms = Some(now_ms);
            recovery.failure_count = 0;
            recovery.first_failure_at_ms = None;
            Ok(url)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_automation::{AutomationActionKind, AutomationActionState};
    fn fixture() -> (
        tempfile::TempDir,
        AgentAutomationStore,
        ConversationIdentity,
    ) {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().into());
        config
            .markdown_chat
            .automation
            .retry_timings
            .infinite_retries = true;
        let store = AgentAutomationStore::for_config(&config).unwrap();
        let task = ConversationIdentity::from_openai_session("recovery-test").unwrap();
        store
            .set_goal(&task, "Preserve this goal and its progress", 1)
            .unwrap();
        store.run_goal(&task, 1, 2).unwrap();
        let pair = store
            .issue_pairing(&task, "https://chatgpt.com/c/broken", 0, 3)
            .unwrap();
        store
            .record_pairing_canonical_url(&task, pair.generation, "https://chatgpt.com/c/broken", 4)
            .unwrap();
        store
            .verify_pairing(&task, pair.generation, 0, &pair.reference, 5)
            .unwrap();
        store
            .set_conversation_recovery_enabled(&task, true)
            .unwrap();
        (root, store, task)
    }
    fn fail(store: &AgentAutomationStore, task: &ConversationIdentity, id: &str, now: u64) {
        store
            .mutate(task, |state| {
                state.action = AutomationActionState {
                    id: Some(id.into()),
                    kind: Some(AutomationActionKind::OfflineGoal),
                    pairing_generation: Some(state.pairing.generation),
                    goal_revision: Some(1),
                    status: AutomationActionStatus::ConversationUnavailable,
                    send_accepted: Some(false),
                    last_error: Some(UNAVAILABLE_CONVERSATION_MESSAGE.into()),
                    updated_at_ms: Some(now),
                    ..Default::default()
                };
                Ok(())
            })
            .unwrap();
    }
    #[test]
    fn replacement_requires_three_distinct_failed_loads_and_preserves_the_goal() {
        let (_root, store, task) = fixture();
        fail(&store, &task, "failure-one", 100);
        for now in [100, 300, 90_000] {
            assert_eq!(
                store
                    .observe_unavailable_conversation(&task, now)
                    .unwrap()
                    .failure_count,
                1
            );
        }
        assert!(
            store
                .reserve_conversation_recovery(&task, "failure-one", 90_000, true)
                .is_err()
        );
        fail(&store, &task, "failure-two", 95_000);
        store
            .observe_unavailable_conversation(&task, 95_000)
            .unwrap();
        assert!(
            store
                .reserve_conversation_recovery(&task, "failure-two", 95_000, true)
                .is_err()
        );
        fail(&store, &task, "failure-three", 100_000);
        store
            .observe_unavailable_conversation(&task, 100_000)
            .unwrap();
        assert!(
            store
                .reserve_conversation_recovery(&task, "stale-action", 100_001, true)
                .is_err()
        );
        assert_eq!(
            store
                .reserve_conversation_recovery(&task, "failure-three", 100_001, true)
                .unwrap(),
            "https://chatgpt.com/c/broken"
        );
        let saved = store.load(&task).unwrap();
        assert_eq!(saved.conversation_recovery.failed_chat_count, 1);
        assert_eq!(
            saved.goal.unwrap().markdown,
            "Preserve this goal and its progress"
        );
        assert_eq!(
            saved.pairing.canonical_url.as_deref(),
            Some("https://chatgpt.com/c/broken")
        );
    }
    #[test]
    fn initial_setup_retry_does_not_delay_first_real_replacement_but_replacement_cooldown_remains()
    {
        let (_root, store, task) = fixture();
        store
            .mutate(&task, |s| {
                s.conversation_recovery.last_recovery_at_ms = Some(95_000);
                Ok(())
            })
            .unwrap();
        for (id, now) in [("first", 10_000), ("second", 45_000)] {
            fail(&store, &task, id, now);
            let recovery = store.observe_unavailable_conversation(&task, now).unwrap();
            assert!(!recovery.automatic_replacement_due(100_000));
        }
        fail(&store, &task, "third", 70_000);
        let recovery = store
            .observe_unavailable_conversation(&task, 70_000)
            .unwrap();
        assert!(recovery.automatic_replacement_due(100_000));
        store
            .reserve_conversation_recovery(&task, "third", 100_000, true)
            .unwrap();
        for (id, now) in [("fourth", 110_000), ("fifth", 145_000), ("sixth", 180_000)] {
            fail(&store, &task, id, now);
            store.observe_unavailable_conversation(&task, now).unwrap();
        }
        let mut recovery = store.load(&task).unwrap().conversation_recovery;
        assert_eq!(recovery.failed_chat_count, 1);
        assert!(!recovery.automatic_replacement_due(200_000));
        assert!(
            store
                .reserve_conversation_recovery(&task, "sixth", 200_000, true)
                .is_err()
        );
        assert!(recovery.automatic_replacement_due(1_900_000));
        recovery.enabled = false;
        assert!(!recovery.automatic_replacement_due(1_900_000));
    }
    #[test]
    fn a_success_resets_failure_evidence_and_uncertain_sends_are_never_replaced() {
        let (_root, store, task) = fixture();
        fail(&store, &task, "first", 100);
        store.observe_unavailable_conversation(&task, 100).unwrap();
        store
            .mutate(&task, |s| {
                s.action.status = AutomationActionStatus::Idle;
                s.action.send_accepted = Some(true);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store
                .observe_unavailable_conversation(&task, 200)
                .unwrap()
                .failure_count,
            0
        );
        fail(&store, &task, "second", 300);
        assert_eq!(
            store
                .observe_unavailable_conversation(&task, 300)
                .unwrap()
                .failure_count,
            1
        );
        for accepted in [None, Some(true)] {
            store
                .mutate(&task, |s| {
                    s.action.send_accepted = accepted;
                    Ok(())
                })
                .unwrap();
            assert!(
                store
                    .reserve_conversation_recovery(&task, "second", 1_000_000, false)
                    .is_err()
            );
        }
        store.stop_goal(&task, 1, 1_000_001).unwrap();
        assert!(!unavailable_conversation(&store.load(&task).unwrap()));
    }
    #[test]
    fn infinite_retries_survive_large_counters_but_preserve_pause_and_send_safety() {
        let (_root, store, task) = fixture();
        store
            .mutate(&task, |s| {
                s.action = AutomationActionState {
                    id: Some("unsent".into()),
                    kind: Some(AutomationActionKind::OfflineChat),
                    pairing_generation: Some(1),
                    status: AutomationActionStatus::Failed,
                    send_accepted: Some(false),
                    updated_at_ms: Some(100),
                    automatic_retry_attempts: u32::MAX,
                    last_error: Some("Transient editor failure".into()),
                    ..Default::default()
                };
                Ok(())
            })
            .unwrap();
        let state = store.load(&task).unwrap();
        assert!(state.retry_timings.infinite_retries);
        assert!(state.automatic_retry_at_ms().is_some());
        let mut finite = state.clone();
        finite.retry_timings.infinite_retries = false;
        assert!(finite.automatic_retry_at_ms().is_none());
        let mut paused = state.clone();
        paused.action.automatic_retry_paused = true;
        assert!(paused.automatic_retry_at_ms().is_none());
        for accepted in [None, Some(true)] {
            let mut uncertain = state.clone();
            uncertain.action.send_accepted = accepted;
            assert!(uncertain.automatic_retry_at_ms().is_none());
        }
    }

    #[test]
    fn interrupted_creation_is_retryable_only_before_any_action_was_queued() {
        let (_root, store, task) = fixture();
        fail(&store, &task, "broken-load", 10);
        store
            .reserve_conversation_recovery(&task, "broken-load", 20, false)
            .unwrap();
        let issue = store
            .issue_creation_pairing(&task, "https://chatgpt.com/new", 1, 30)
            .unwrap();
        let preparing = store.load(&task).unwrap();
        assert!(preparing.pairing.creating_chat);
        assert!(preparing.pairing.pending.is_some());
        assert!(!interrupted_replacement_creation(&preparing));
        store
            .invalidate_pairing(&task, issue.generation, 40)
            .unwrap();
        let interrupted = store.load(&task).unwrap();
        assert!(interrupted_replacement_creation(&interrupted));
        store
            .record_interrupted_replacement_retry(&task, 50)
            .unwrap();
        assert_eq!(
            store
                .load(&task)
                .unwrap()
                .conversation_recovery
                .failed_chat_count,
            1
        );
        for accepted in [None, Some(false), Some(true)] {
            let mut queued = interrupted.clone();
            queued.action.id = Some("a-browser-action-existed".into());
            queued.action.send_accepted = accepted;
            assert!(!interrupted_replacement_creation(&queued));
        }
        let mut uncertain = interrupted.clone();
        uncertain.action.last_error =
            Some("Pairing verification was interrupted; pair this conversation again".into());
        assert!(!interrupted_replacement_creation(&uncertain));
        let mut stopped = interrupted;
        stopped.goal.as_mut().unwrap().status = GoalStatus::Configured;
        assert!(!interrupted_replacement_creation(&stopped));
    }
}
