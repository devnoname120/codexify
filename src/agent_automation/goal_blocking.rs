//! Keep an infinite goal active until the same blocker survives later turns.
use super::{AgentAutomationStore, GoalStatus};
use crate::project_bindings::ConversationIdentity;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BlockerAudit {
    pub revision: u64,
    pub reason: String,
    pub attempts: u32,
    pub last_turn: u64,
    pub first_at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn infinite_goal_rejects_premature_blocking_but_accepts_a_persistent_blocker() {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().into());
        config.markdown_chat.automation.retry_timings.infinite_retries = true;
        let store = AgentAutomationStore::for_config(&config).unwrap();
        let task = ConversationIdentity::from_openai_session("blocker-audit").unwrap();
        store
            .set_goal(&task, "Finish the requested implementation", 1)
            .unwrap();
        store.run_goal(&task, 1, 2).unwrap();
        let reason = "Hardware verification requires the unavailable physical console; local checks completed.";
        assert!(store.audit_goal_blocker(&task, 1, reason, 100).is_err());
        for _ in 0..4 {
            assert!(store.audit_goal_blocker(&task, 1, reason, 100_000).is_err());
        }
        assert_eq!(
            store.load(&task).unwrap().blocker_audit.unwrap().attempts,
            1
        );
        for turn in [2, 3] {
            store
                .mutate(&task, |s| {
                    s.goal.as_mut().unwrap().turn_count = turn;
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                store
                    .audit_goal_blocker(&task, 1, reason, turn * 100_000)
                    .is_ok(),
                turn == 3
            );
        }
        assert_eq!(
            store.load(&task).unwrap().goal.unwrap().status,
            GoalStatus::Active
        );
        store
            .finish_goal(
                &task,
                1,
                super::super::GoalFinish::Blocked {
                    reason: reason.into(),
                },
                400_000,
            )
            .unwrap();
        store.run_goal(&task, 1, 500_000).unwrap();
        assert!(store.load(&task).unwrap().blocker_audit.is_none());
        assert!(store.audit_goal_blocker(&task, 1, reason, 500_001).is_err());
    }
}

impl AgentAutomationStore {
    pub fn audit_goal_blocker(
        &self,
        task: &ConversationIdentity,
        revision: u64,
        reason: &str,
        now: u64,
    ) -> Result<(), String> {
        if reason.trim().is_empty() || reason.len() > 4096 {
            return Err("A blocked goal requires a concrete reason of at most 4096 bytes".into());
        }
        let deferred = self.mutate(task, |state| {
            let goal = state.goal.as_ref().ok_or("No agent-chat goal is configured")?;
            if goal.revision != revision { return Err("The goal revision changed; read goal_read again".into()); }
            if !state.retry_timings.infinite_retries || goal.status != GoalStatus::Active { return Ok(None); }
            let same = state.blocker_audit.as_ref().is_some_and(|audit| audit.revision == revision && audit.reason == reason);
            if !same {
                state.blocker_audit = Some(BlockerAudit {revision, reason:reason.into(), attempts:1, last_turn:goal.turn_count, first_at_ms:now});
            } else if let Some(audit) = state.blocker_audit.as_mut() {
                if audit.last_turn != goal.turn_count {
                    audit.last_turn = goal.turn_count;
                    audit.attempts = audit.attempts.saturating_add(1);
                }
            }
            let audit = state.blocker_audit.as_ref().expect("audit recorded");
            Ok((audit.attempts < 3 || now.saturating_sub(audit.first_at_ms) < 60_000).then(|| format!(
                "Goal remains active. Blocker audit {}/3: {}. Check alternatives, retry temporary failures, and continue independent work. Record evidence and the exact external input needed in rollout_update and chat_write. If the same blocker remains on a later goal turn, submit the same reason again. Repeated calls within this turn do not count. Three distinct turns and at least 60 seconds are required before blocking an infinite goal.", audit.attempts.min(3), audit.reason)))
        })?;
        if let Some(message) = deferred {
            return Err(message);
        }
        Ok(())
    }
}
