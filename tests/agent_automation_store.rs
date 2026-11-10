use std::sync::{Arc, Barrier};

use codexify::agent_automation::{
    AgentAutomationStore, AutomationActionKind, AutomationActionStatus, GoalFinish, GoalStatus,
    PAIRING_REFERENCE_TTL_MS,
};
use codexify::project_bindings::ConversationIdentity;

fn task(name: &str) -> ConversationIdentity {
    ConversationIdentity::from_openai_session(name).unwrap()
}

#[test]
fn run_now_capability_is_only_for_a_current_confirmed_unsent_queued_action() {
    use codexify::agent_automation::{AutomationActionState, AutomationTaskState};
    let mut state = AutomationTaskState::default();
    state.pairing.generation = 1;
    state.pairing.verified_owner_generation = Some(0);
    state.action = AutomationActionState {
        id: Some("queued-chat".into()),
        kind: Some(AutomationActionKind::OfflineChat),
        status: AutomationActionStatus::Queued,
        pairing_generation: Some(1),
        send_accepted: Some(false),
        ..Default::default()
    };
    assert!(state.can_run_queued_now());
    for accepted in [None, Some(true)] {
        let mut changed = state.clone();
        changed.action.send_accepted = accepted;
        assert!(!changed.can_run_queued_now());
    }
    for phase in [
        AutomationActionStatus::Navigating,
        AutomationActionStatus::Sending,
        AutomationActionStatus::Idle,
        AutomationActionStatus::Cancelled,
    ] {
        let mut changed = state.clone();
        changed.action.status = phase;
        assert!(!changed.can_run_queued_now());
    }
    let mut changed = state.clone();
    changed.action.messages_fenced = true;
    assert!(!changed.can_run_queued_now());
    let mut changed = state.clone();
    changed.action.goal_turn_counted = true;
    assert!(!changed.can_run_queued_now());
    let mut changed = state.clone();
    changed.action.kind = Some(AutomationActionKind::PairVerify);
    assert!(!changed.can_run_queued_now());
    state.pairing.generation = 2;
    assert!(!state.can_run_queued_now());
}

#[test]
fn automatic_retry_requires_confirmed_unsent_work_and_an_active_matching_goal() {
    use codexify::agent_automation::{AutomationActionState, AutomationTaskState};
    let mut state = AutomationTaskState::default();
    state.action = AutomationActionState {
        id: Some("stop-failure".into()),
        kind: Some(AutomationActionKind::OfflineChat),
        status: AutomationActionStatus::Failed,
        send_accepted: Some(false),
        updated_at_ms: Some(100),
        last_error: Some(
            "ChatGPT did not stop the previous response; the new message was not sent".into(),
        ),
        ..Default::default()
    };
    assert!((30_100..=33_100).contains(&state.automatic_retry_at_ms().unwrap()));
    assert!(state.is_automatically_retrying());
    let mut legacy = serde_json::to_value(&state).unwrap();
    legacy["action"]
        .as_object_mut()
        .unwrap()
        .remove("automaticRetryInProgress");
    let legacy: AutomationTaskState = serde_json::from_value(legacy).unwrap();
    assert!(!legacy.action.automatic_retry_in_progress);
    assert!(legacy.is_automatically_retrying());
    for accepted in [None, Some(true)] {
        let mut uncertain = state.clone();
        uncertain.action.send_accepted = accepted;
        assert!(uncertain.automatic_retry_at_ms().is_none());
    }
    for kind in [
        AutomationActionKind::PairVerify,
        AutomationActionKind::OfflineGoal,
        AutomationActionKind::OfflineCombined,
    ] {
        let mut other = state.clone();
        other.action.kind = Some(kind);
        assert!(other.automatic_retry_at_ms().is_none());
    }
    let mut fenced = state.clone();
    fenced.action.messages_fenced = true;
    assert!(fenced.automatic_retry_at_ms().is_none());
    let mut different = state.clone();
    different.action.last_error = Some("Selector verification failed before Send".into());
    assert!((30_100..=33_100).contains(&different.automatic_retry_at_ms().unwrap()));
    for (attempts, delay) in [(1, 60_000), (2, 120_000), (3, 240_000), (4, 480_000)] {
        different.action.automatic_retry_attempts = attempts;
        assert!(
            (100 + delay..=100 + delay + delay / 10)
                .contains(&different.automatic_retry_at_ms().unwrap())
        );
    }
    different.action.automatic_retry_attempts = 5;
    assert!(different.automatic_retry_at_ms().is_none());
    different.action.automatic_retry_in_progress = true;
    assert!(
        !different.is_automatically_retrying(),
        "exhausted failures stay terminal"
    );
    let mut cancelled = state.clone();
    cancelled.action.last_error = Some("Browser action cancelled before Send".into());
    assert!(cancelled.automatic_retry_at_ms().is_none());
    let mut ambiguous = state.clone();
    ambiguous.action.status = AutomationActionStatus::Ambiguous;
    assert!(ambiguous.automatic_retry_at_ms().is_none());
    let root = tempfile::tempdir().unwrap();
    let store = AgentAutomationStore::new(root.path().join("state")).unwrap();
    let task = task("automatic-stop-retry");
    let goal = store.set_goal(&task, "Continue the goal.", 1).unwrap();
    let mut active = store.run_goal(&task, goal.revision, 2).unwrap();
    state.goal = Some(active.clone());
    state.action.kind = Some(AutomationActionKind::OfflineGoal);
    state.action.goal_revision = Some(active.revision);
    assert!((30_100..=33_100).contains(&state.automatic_retry_at_ms().unwrap()));
    active.status = GoalStatus::Configured;
    state.goal = Some(active.clone());
    assert!(state.automatic_retry_at_ms().is_none());
    active.status = GoalStatus::Blocked;
    state.goal = Some(active.clone());
    assert!(state.automatic_retry_at_ms().is_none());
    active.status = GoalStatus::Active;
    active.revision += 1;
    state.goal = Some(active);
    assert!(state.automatic_retry_at_ms().is_none());
}

#[test]
fn pairing_reference_remains_valid_until_sixty_minutes() {
    assert_eq!(PAIRING_REFERENCE_TTL_MS, 60 * 60 * 1000);
    for (elapsed, valid) in [
        (59 * 60_000, true),
        (60 * 60_000, true),
        (60 * 60_000 + 1, false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let store = AgentAutomationStore::new(root.path().join("automation")).unwrap();
        let task = task("hour-long-pairing");
        let pairing = store
            .issue_pairing(&task, "https://chatgpt.com/c/hour", 0, 1000)
            .unwrap();
        store
            .record_pairing_canonical_url(
                &task,
                pairing.generation,
                "https://chatgpt.com/c/hour",
                1001,
            )
            .unwrap();
        assert_eq!(
            store
                .verify_pairing(
                    &task,
                    pairing.generation,
                    0,
                    &pairing.reference,
                    1000 + elapsed
                )
                .is_ok(),
            valid
        );
    }
}

#[test]
fn goal_state_round_trips_with_private_storage() {
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("automation");
    let store = AgentAutomationStore::new(state_root.clone()).unwrap();
    let task = task("goal-round-trip");

    let configured = store
        .set_goal(&task, "Ship the browser fallback", 10)
        .unwrap();
    assert_eq!(configured.revision, 1);
    assert_eq!(configured.status, GoalStatus::Configured);
    let active = store.run_goal(&task, configured.revision, 20).unwrap();
    assert_eq!(active.status, GoalStatus::Active);

    let reloaded = AgentAutomationStore::new(state_root).unwrap();
    let state = reloaded.load(&task).unwrap();
    let goal = state.goal.unwrap();
    assert_eq!(goal.markdown, "Ship the browser fallback");
    assert_eq!(goal.revision, 1);
    assert_eq!(goal.status, GoalStatus::Active);

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let directory = reloaded.task_directory(&task).unwrap();
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let path = directory.join("automation.json");
        let metadata = std::fs::metadata(path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    }
}

#[test]
fn unset_pairing_submission_time_keeps_persisted_state_compatible() {
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("automation");
    let store = AgentAutomationStore::new(state_root.clone()).unwrap();
    let task = task("pairing-state-compatibility");
    let issue = store
        .issue_pairing(&task, "https://chatgpt.com/c/compatibility", 1, 1000)
        .unwrap();

    let path = store.task_directory(&task).unwrap().join("automation.json");
    let mut persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(
        persisted["pairing"].get("verificationSentAtMs").is_none(),
        "older readers reject this new field even when it is null"
    );
    let reloaded = AgentAutomationStore::new(state_root).unwrap();
    assert_eq!(
        reloaded
            .load(&task)
            .unwrap()
            .pairing
            .verification_sent_at_ms,
        None
    );

    // A populated timestamp from a newer writer must survive a normal mutation.
    persisted["pairing"]["verificationSentAtMs"] = serde_json::json!(1100);
    std::fs::write(&path, serde_json::to_vec(&persisted).unwrap()).unwrap();
    reloaded
        .queue_action(
            &task,
            "compatibility-action",
            AutomationActionKind::PairVerify,
            issue.generation,
            None,
            1101,
        )
        .unwrap();
    let persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(persisted["pairing"]["verificationSentAtMs"], 1100);
    assert_eq!(
        reloaded
            .load(&task)
            .unwrap()
            .pairing
            .verification_sent_at_ms,
        Some(1100)
    );
}

#[test]
fn goals_are_bounded_revisioned_and_have_no_pause_state() {
    let root = tempfile::tempdir().unwrap();
    let store = AgentAutomationStore::new(root.path().join("automation")).unwrap();
    let task = task("goal-lifecycle");

    assert!(store.set_goal(&task, &"x".repeat(10 * 1024), 1).is_ok());
    assert!(
        store
            .set_goal(&task, &"x".repeat(10 * 1024 + 1), 2)
            .is_err()
    );
    let replacement = store.set_goal(&task, "replacement", 3).unwrap();
    assert_eq!(replacement.revision, 2);
    assert_eq!(replacement.status, GoalStatus::Configured);
    assert!(store.run_goal(&task, 1, 4).is_err());
    assert_eq!(
        store
            .run_goal(&task, replacement.revision, 4)
            .unwrap()
            .status,
        GoalStatus::Active
    );
    assert!(store.clear_goal(&task, replacement.revision).is_err());
    assert_eq!(
        store
            .stop_goal(&task, replacement.revision, 5)
            .unwrap()
            .status,
        GoalStatus::Configured
    );
    store.clear_goal(&task, replacement.revision).unwrap();
    assert!(store.load(&task).unwrap().goal.is_none());

    let serialized = serde_json::to_string(&store.load(&task).unwrap()).unwrap();
    assert!(!serialized.contains("pause"));
    assert!(!serialized.contains("resume"));
}

#[test]
fn finishing_requires_the_exact_active_revision_and_a_blocked_reason() {
    let root = tempfile::tempdir().unwrap();
    let store = AgentAutomationStore::new(root.path().join("automation")).unwrap();
    let task = task("goal-finish");
    let goal = store.set_goal(&task, "Complete me", 1).unwrap();
    store.run_goal(&task, goal.revision, 2).unwrap();

    assert!(
        store
            .finish_goal(
                &task,
                goal.revision + 1,
                GoalFinish::Completed { summary: None },
                3
            )
            .is_err()
    );
    assert!(
        store
            .finish_goal(
                &task,
                goal.revision,
                GoalFinish::Blocked {
                    reason: "  ".into()
                },
                3
            )
            .is_err()
    );
    let blocked = store
        .finish_goal(
            &task,
            goal.revision,
            GoalFinish::Blocked {
                reason: "Waiting for the user to approve a tool action".into(),
            },
            4,
        )
        .unwrap();
    assert_eq!(blocked.status, GoalStatus::Blocked);
    assert_eq!(
        blocked.blocked_reason.as_deref(),
        Some("Waiting for the user to approve a tool action")
    );
    assert!(
        store
            .finish_goal(
                &task,
                goal.revision,
                GoalFinish::Completed { summary: None },
                5
            )
            .is_err()
    );
}

#[test]
fn pairing_retry_url_survives_failures_and_reload_without_prefilling_creation_destinations() {
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("automation");
    let store = AgentAutomationStore::new(state_root.clone()).unwrap();
    let task = task("retry-pairing-url");
    let url = "https://chatgpt.com/g/project-alias/c/attempted-chat";
    let issue = store
        .issue_pairing(&task, &format!("{url}?source=ignored"), 0, 1000)
        .unwrap();
    store
        .invalidate_pairing(&task, issue.generation, 1001)
        .unwrap();
    assert_eq!(
        store
            .load(&task)
            .unwrap()
            .pairing
            .last_attempted_url
            .as_deref(),
        Some(url)
    );
    store.unlink_pairing(&task, 1002).unwrap();
    let reloaded = AgentAutomationStore::new(state_root).unwrap();
    let pairing = reloaded.load(&task).unwrap().pairing;
    assert!(pairing.pending.is_none() && pairing.candidate_url.is_none());
    assert_eq!(pairing.last_attempted_url.as_deref(), Some(url));

    let issue = reloaded
        .issue_pairing(&task, "https://chatgpt.com/g/g-p-project/project", 1, 2000)
        .unwrap();
    reloaded.mark_creating_chat(&task).unwrap();
    assert!(
        reloaded
            .load(&task)
            .unwrap()
            .pairing
            .last_attempted_url
            .is_none(),
        "a destination project is not a created conversation URL"
    );
    reloaded
        .record_pairing_canonical_url(
            &task,
            issue.generation,
            "https://chatgpt.com/c/created-chat",
            2001,
        )
        .unwrap();
    assert_eq!(
        reloaded
            .load(&task)
            .unwrap()
            .pairing
            .last_attempted_url
            .as_deref(),
        Some("https://chatgpt.com/c/created-chat")
    );
    let file = reloaded
        .task_directory(&task)
        .unwrap()
        .join("automation.json");
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    legacy["pairing"]
        .as_object_mut()
        .unwrap()
        .remove("lastAttemptedUrl");
    std::fs::write(&file, serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert!(
        reloaded
            .load(&task)
            .unwrap()
            .pairing
            .last_attempted_url
            .is_none(),
        "older state remains readable"
    );
}

#[test]
fn pairing_references_are_random_digest_only_expiring_and_single_use() {
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("automation");
    let store = AgentAutomationStore::new(state_root.clone()).unwrap();
    let task = task("pairing");

    let first = store
        .issue_pairing(&task, "https://chatgpt.com/c/first", 7, 1_000)
        .unwrap();
    let second = store
        .issue_pairing(&task, "https://chatgpt.com/c/second", 7, 2_000)
        .unwrap();
    assert_ne!(first.reference, second.reference);
    assert_eq!(second.generation, first.generation + 1);

    let bytes =
        std::fs::read_to_string(store.task_directory(&task).unwrap().join("automation.json"))
            .unwrap();
    assert!(!bytes.contains(&first.reference));
    assert!(!bytes.contains(&second.reference));
    assert!(bytes.contains("referenceDigest"));

    store
        .record_pairing_canonical_url(
            &task,
            second.generation,
            "https://chatgpt.com/c/canonical",
            2_100,
        )
        .unwrap();
    let verified = store
        .verify_pairing(&task, second.generation, 7, &second.reference, 2_200)
        .unwrap();
    assert_eq!(
        verified.canonical_url.as_deref(),
        Some("https://chatgpt.com/c/canonical")
    );
    assert_eq!(verified.verified_owner_generation, Some(7));
    assert!(
        store
            .verify_pairing(&task, second.generation, 7, &second.reference, 2_300)
            .is_err()
    );

    let expired = store
        .issue_pairing(&task, "https://chatgpt.com/c/expired", 8, 10_000)
        .unwrap();
    assert!(
        store
            .verify_pairing(
                &task,
                expired.generation,
                8,
                &expired.reference,
                10_000 + PAIRING_REFERENCE_TTL_MS + 1,
            )
            .is_err()
    );
    assert!(store.load(&task).unwrap().pairing.pending.is_none());
}

#[test]
fn pairing_urls_discard_noncanonical_query_data_before_persistence() {
    let root = tempfile::tempdir().unwrap();
    let store = AgentAutomationStore::new(root.path().join("automation")).unwrap();
    let task = task("pairing-query");

    let pairing = store
        .issue_pairing(
            &task,
            "https://chatgpt.com/c/query-example?temporary-chat=false&source=sidebar",
            0,
            1_000,
        )
        .unwrap();
    assert_eq!(
        store.load(&task).unwrap().pairing.candidate_url.as_deref(),
        Some("https://chatgpt.com/c/query-example")
    );

    let recorded = store
        .record_pairing_canonical_url(
            &task,
            pairing.generation,
            "https://chatgpt.com/c/query-example?source=browser",
            1_001,
        )
        .unwrap();
    assert_eq!(
        recorded.canonical_url.as_deref(),
        Some("https://chatgpt.com/c/query-example")
    );
}

#[test]
fn pairing_rejects_the_chatgpt_homepage_without_a_conversation_path() {
    let root = tempfile::tempdir().unwrap();
    let store = AgentAutomationStore::new(root.path().join("automation")).unwrap();
    let task = task("pairing-root");

    let error = store
        .issue_pairing(&task, "https://chatgpt.com/", 0, 1_000)
        .unwrap_err();

    assert!(error.contains("conversation URL"), "{error}");
    assert!(store.load(&task).unwrap().pairing.candidate_url.is_none());
}

#[test]
fn pairing_preserves_project_conversation_urls() {
    let root = tempfile::tempdir().unwrap();
    let store = AgentAutomationStore::new(root.path().join("automation")).unwrap();
    let task = task("project-pairing");
    let url = "https://chatgpt.com/g/g-p-0123456789abcdef0123456789abcdef-example-project/c/01234567-89ab-cdef-0123-456789abcdef";
    let pairing = store.issue_pairing(&task, url, 0, 1_000).unwrap();
    assert_eq!(
        store.load(&task).unwrap().pairing.candidate_url.as_deref(),
        Some(url)
    );
    let recorded = store
        .record_pairing_canonical_url(&task, pairing.generation, url, 1_001)
        .unwrap();
    assert_eq!(recorded.canonical_url.as_deref(), Some(url));
}

#[test]
fn pairing_reference_index_is_private_and_digest_only() {
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("automation");
    let store = AgentAutomationStore::new(state_root.clone()).unwrap();
    let task = task("pairing-index");

    let issued = store
        .issue_pairing(&task, "https://chatgpt.com/c/private", 0, 1_000)
        .unwrap();
    let index_root = state_root.join(".pairings");
    let entries = std::fs::read_dir(&index_root)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(entries.len(), 1);
    let index_path = entries[0].path();
    assert_eq!(index_path.file_name().unwrap().to_string_lossy().len(), 64);
    let stored = std::fs::read_to_string(&index_path).unwrap();
    assert!(!stored.contains(&issued.reference));
    assert!(stored.contains("taskRoot"));
    assert!(stored.contains("taskKey"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            std::fs::metadata(&index_root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(index_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn invalidating_pairing_revokes_its_pending_reference_index() {
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("automation");
    let store = AgentAutomationStore::new(state_root.clone()).unwrap();
    let task = task("pairing-invalidate");

    let issued = store
        .issue_pairing(&task, "https://chatgpt.com/c/pairing-invalidate", 0, 1_000)
        .unwrap();
    assert_eq!(
        std::fs::read_dir(state_root.join(".pairings"))
            .unwrap()
            .count(),
        1
    );

    let pairing = store
        .invalidate_pairing(&task, issued.generation, 1_001)
        .unwrap();

    assert!(pairing.pending.is_none());
    assert_eq!(
        std::fs::read_dir(state_root.join(".pairings"))
            .unwrap()
            .count(),
        0
    );
    assert!(
        store
            .verify_pairing_reference(&task, 0, &issued.reference, 1_002)
            .is_err()
    );
}

#[test]
fn failed_pairing_cleanup_preserves_verification_that_won_the_race() {
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("automation");
    let store = AgentAutomationStore::new(state_root.clone()).unwrap();
    let task = task("verified-before-failure-cleanup");
    let url = "https://chatgpt.com/g/g-p-project/c/verified-chat";
    let issue = store.issue_pairing(&task, url, 1, 1_000).unwrap();
    store
        .record_pairing_canonical_url(&task, issue.generation, url, 1_001)
        .unwrap();

    // The browser has observed an unverified attempt, but the authenticated
    // tool succeeds before that browser failure reaches its cleanup write.
    store
        .verify_pairing_reference(&task, 1, &issue.reference, 1_002)
        .unwrap();
    let path = store.task_directory(&task).unwrap().join("automation.json");
    let verified_bytes = std::fs::read(&path).unwrap();
    store
        .invalidate_pairing(&task, issue.generation, 1_003)
        .unwrap();

    let reloaded = AgentAutomationStore::new(state_root).unwrap();
    let state = reloaded.load(&task).unwrap();
    assert_eq!(state.pairing.verified_owner_generation, Some(1));
    assert_eq!(state.pairing.verified_at_ms, Some(1_002));
    assert_eq!(state.pairing.canonical_url.as_deref(), Some(url));
    assert_eq!(state.action.status, AutomationActionStatus::Idle);
    assert!(
        std::fs::read(&path).unwrap() == verified_bytes,
        "Late failure cleanup changed the verified state"
    );

    // Explicit user unlinking remains authoritative.
    let unlinked = reloaded.unlink_pairing(&task, 1_004).unwrap();
    assert!(unlinked.verified_owner_generation.is_none());
    assert!(unlinked.canonical_url.is_none());
}

#[test]
fn failed_pairing_cleanup_does_not_revoke_a_newer_attempt() {
    let root = tempfile::tempdir().unwrap();
    let store = AgentAutomationStore::new(root.path().join("automation")).unwrap();
    let task = task("replaced-before-failure-cleanup");
    let first = store
        .issue_pairing(&task, "https://chatgpt.com/c/old-attempt", 0, 1_000)
        .unwrap();
    let url = "https://chatgpt.com/c/new-attempt";
    let second = store.issue_pairing(&task, url, 0, 1_001).unwrap();
    let path = store.task_directory(&task).unwrap().join("automation.json");
    let pending_bytes = std::fs::read(&path).unwrap();

    store
        .invalidate_pairing(&task, first.generation, 1_002)
        .unwrap();

    assert!(
        std::fs::read(&path).unwrap() == pending_bytes,
        "Stale failure cleanup changed the newer pending state"
    );
    store
        .record_pairing_canonical_url(&task, second.generation, url, 1_003)
        .unwrap();
    let verified = store
        .verify_pairing_reference(&task, 0, &second.reference, 1_004)
        .unwrap();
    assert_eq!(verified.generation, second.generation);
    assert_eq!(verified.verified_owner_generation, Some(0));
    assert_eq!(verified.canonical_url.as_deref(), Some(url));
}

#[test]
fn account_change_revokes_a_pending_pairing_reference() {
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("automation");
    let store = AgentAutomationStore::new(state_root.clone()).unwrap();
    let task = task("pairing-account-change");
    store
        .set_selection(
            &task,
            "gpt-6-sol",
            "high",
            "catalog-one",
            "account-one",
            1_000,
        )
        .unwrap();
    let issued = store
        .issue_pairing(
            &task,
            "https://chatgpt.com/c/pairing-account-change",
            0,
            1_001,
        )
        .unwrap();
    store
        .record_pairing_canonical_url(
            &task,
            issued.generation,
            "https://chatgpt.com/c/pairing-account-change",
            1_002,
        )
        .unwrap();
    assert_eq!(
        std::fs::read_dir(state_root.join(".pairings"))
            .unwrap()
            .count(),
        1
    );

    store
        .set_selection(
            &task,
            "gpt-6-sol",
            "high",
            "catalog-two",
            "account-two",
            1_003,
        )
        .unwrap();

    let state = store.load(&task).unwrap();
    assert!(state.pairing.pending.is_none());
    assert!(state.pairing.verified_owner_generation.is_none());
    assert_eq!(state.action.status, AutomationActionStatus::Unverified);
    assert_eq!(
        std::fs::read_dir(state_root.join(".pairings"))
            .unwrap()
            .count(),
        0
    );
    assert!(
        store
            .verify_pairing_reference(&task, 0, &issued.reference, 1_004)
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn pairing_reference_index_rejects_a_symlinked_root() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real-index");
    std::fs::create_dir(&real).unwrap();
    let link = root.path().join("index-link");
    symlink(&real, &link).unwrap();

    assert!(
        AgentAutomationStore::new_with_pairing_index(root.path().join("automation"), link,)
            .is_err()
    );
}

#[test]
fn independent_store_instances_serialize_mutations() {
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("automation");
    let first = Arc::new(AgentAutomationStore::new(state_root.clone()).unwrap());
    let second = Arc::new(AgentAutomationStore::new(state_root).unwrap());
    let task = Arc::new(task("concurrent"));
    let barrier = Arc::new(Barrier::new(3));

    let workers = [first, second]
        .into_iter()
        .enumerate()
        .map(|(index, store)| {
            let task = task.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .set_goal(&task, &format!("goal-{index}"), index as u64 + 1)
                    .unwrap()
                    .revision
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let mut revisions = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    revisions.sort_unstable();
    assert_eq!(revisions, vec![1, 2]);
}

#[test]
fn corrupt_or_symlinked_state_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("automation");
    let store = AgentAutomationStore::new(state_root.clone()).unwrap();
    let task = task("unsafe-state");
    store.set_goal(&task, "safe", 1).unwrap();
    let state_path = store.task_directory(&task).unwrap().join("automation.json");
    std::fs::write(&state_path, b"not json").unwrap();
    assert!(
        AgentAutomationStore::new(state_root.clone())
            .unwrap()
            .load(&task)
            .is_err()
    );

    #[cfg(unix)]
    {
        let target = root.path().join("target.json");
        std::fs::write(&target, b"{}").unwrap();
        std::fs::remove_file(&state_path).unwrap();
        std::os::unix::fs::symlink(&target, &state_path).unwrap();
        assert!(
            AgentAutomationStore::new(state_root)
                .unwrap()
                .load(&task)
                .is_err()
        );
    }
}

#[test]
fn logically_inconsistent_persisted_state_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("automation");
    let store = AgentAutomationStore::new(state_root.clone()).unwrap();

    let partial_selection = task("partial-selection-state");
    store
        .set_selection(
            &partial_selection,
            "gpt-6-sol",
            "high",
            "catalog-one",
            "account-one",
            1,
        )
        .unwrap();
    let partial_path = store
        .task_directory(&partial_selection)
        .unwrap()
        .join("automation.json");
    let mut partial: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&partial_path).unwrap()).unwrap();
    partial["selection"]["effort"] = serde_json::Value::Null;
    std::fs::write(&partial_path, serde_json::to_vec(&partial).unwrap()).unwrap();
    assert!(
        AgentAutomationStore::new(state_root.clone())
            .unwrap()
            .load(&partial_selection)
            .is_err()
    );

    let duplicate_mirror = task("duplicate-mirror-state");
    store
        .set_goal(&duplicate_mirror, "Create state", 1)
        .unwrap();
    let mirror_path = store
        .task_directory(&duplicate_mirror)
        .unwrap()
        .join("automation.json");
    let mut mirror: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&mirror_path).unwrap()).unwrap();
    mirror["chatMirror"] = serde_json::json!({
        "ownerGeneration": 0,
        "lastAcceptedBatchId": "duplicate-batch",
        "messageIds": ["duplicate-message", "duplicate-message"],
        "transcriptRevisionAtStart": "duplicate-revision"
    });
    std::fs::write(&mirror_path, serde_json::to_vec(&mirror).unwrap()).unwrap();
    assert!(
        AgentAutomationStore::new(state_root.clone())
            .unwrap()
            .load(&duplicate_mirror)
            .is_err()
    );

    let pending_and_verified = task("pending-and-verified-state");
    store
        .set_selection(
            &pending_and_verified,
            "gpt-6-sol",
            "high",
            "catalog-two",
            "account-two",
            1,
        )
        .unwrap();
    let issue = store
        .issue_pairing(
            &pending_and_verified,
            "https://chatgpt.com/c/pending-and-verified",
            7,
            2,
        )
        .unwrap();
    let pairing_path = store
        .task_directory(&pending_and_verified)
        .unwrap()
        .join("automation.json");
    let mut pairing: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pairing_path).unwrap()).unwrap();
    pairing["pairing"]["canonicalUrl"] =
        serde_json::Value::String("https://chatgpt.com/c/pending-and-verified".into());
    pairing["pairing"]["verifiedOwnerGeneration"] = serde_json::json!(7);
    pairing["pairing"]["verifiedAtMs"] = serde_json::json!(3);
    assert_eq!(pairing["pairing"]["generation"], issue.generation);
    std::fs::write(&pairing_path, serde_json::to_vec(&pairing).unwrap()).unwrap();
    assert!(
        AgentAutomationStore::new(state_root)
            .unwrap()
            .load(&pending_and_verified)
            .is_err()
    );

    let incomplete_action = task("incomplete-action-state");
    store
        .set_goal(&incomplete_action, "Create action state", 1)
        .unwrap();
    let action_path = store
        .task_directory(&incomplete_action)
        .unwrap()
        .join("automation.json");
    let mut action: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&action_path).unwrap()).unwrap();
    action["action"] = serde_json::json!({
        "id": null,
        "kind": "offline_chat",
        "status": "queued",
        "pairingGeneration": 1,
        "goalRevision": null,
        "lastError": null,
        "lastStableAnswer": null,
        "updatedAtMs": 1
    });
    std::fs::write(&action_path, serde_json::to_vec(&action).unwrap()).unwrap();
    assert!(
        AgentAutomationStore::new(root.path().join("automation"))
            .unwrap()
            .load(&incomplete_action)
            .is_err()
    );

    let failed_without_id = task("failed-action-without-id");
    store
        .set_goal(&failed_without_id, "Create failed action state", 1)
        .unwrap();
    let failed_path = store
        .task_directory(&failed_without_id)
        .unwrap()
        .join("automation.json");
    let mut failed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&failed_path).unwrap()).unwrap();
    failed["action"] = serde_json::json!({
        "id": null,
        "kind": "offline_chat",
        "status": "failed",
        "pairingGeneration": 1,
        "goalRevision": null,
        "lastError": "Retryable queue failure",
        "lastStableAnswer": null,
        "updatedAtMs": 1
    });
    std::fs::write(&failed_path, serde_json::to_vec(&failed).unwrap()).unwrap();
    assert!(
        AgentAutomationStore::new(root.path().join("automation"))
            .unwrap()
            .load(&failed_without_id)
            .is_err()
    );

    let incomplete_wake = task("incomplete-goal-wake-state");
    store
        .set_goal(&incomplete_wake, "Create wake state", 1)
        .unwrap();
    let wake_path = store
        .task_directory(&incomplete_wake)
        .unwrap()
        .join("automation.json");
    let mut wake: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&wake_path).unwrap()).unwrap();
    wake["goalWake"]["nudgedEpoch"] = serde_json::json!("epoch-one");
    std::fs::write(&wake_path, serde_json::to_vec(&wake).unwrap()).unwrap();
    assert!(
        AgentAutomationStore::new(root.path().join("automation"))
            .unwrap()
            .load(&incomplete_wake)
            .is_err()
    );
}

#[test]
fn only_pre_send_failed_non_pair_actions_can_be_reset_for_explicit_retry() {
    let root = tempfile::tempdir().unwrap();
    let store = AgentAutomationStore::new(root.path().join("automation")).unwrap();
    let task = task("retry-action");
    store
        .set_selection(&task, "gpt-6-sol", "high", "catalog-1", "account-1", 1)
        .unwrap();
    let pairing = store
        .issue_pairing(&task, "https://chatgpt.com/c/retry", 0, 2)
        .unwrap();
    store
        .record_pairing_canonical_url(&task, pairing.generation, "https://chatgpt.com/c/retry", 3)
        .unwrap();
    store
        .verify_pairing(&task, pairing.generation, 0, &pairing.reference, 4)
        .unwrap();
    store
        .queue_action(
            &task,
            "failed-action",
            AutomationActionKind::OfflineChat,
            pairing.generation,
            None,
            5,
        )
        .unwrap();
    store
        .update_action_status(
            &task,
            "failed-action",
            AutomationActionStatus::Failed,
            Some("Selector verification failed before Send"),
            None,
            6,
        )
        .unwrap();

    let reset = store
        .reset_failed_action_for_retry(&task, "failed-action", 7)
        .unwrap();
    assert_eq!(reset.status, AutomationActionStatus::Idle);
    assert!(reset.id.is_none());
    assert!(reset.kind.is_none());
    assert!(reset.last_error.is_none());

    store
        .queue_action(
            &task,
            "ambiguous-action",
            AutomationActionKind::OfflineChat,
            pairing.generation,
            None,
            8,
        )
        .unwrap();
    store
        .update_action_status(
            &task,
            "ambiguous-action",
            AutomationActionStatus::Ambiguous,
            Some("Send may have been accepted"),
            None,
            9,
        )
        .unwrap();
    assert!(
        store
            .reset_failed_action_for_retry(&task, "ambiguous-action", 10)
            .unwrap_err()
            .contains("failed before Send")
    );
}

#[test]
fn goal_timer_accumulates_only_enabled_intervals_and_survives_reload() {
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("automation");
    let store = AgentAutomationStore::new(state_root.clone()).unwrap();
    let task = task("goal-timer");
    let goal = store.set_goal(&task, "Track active time", 10).unwrap();
    assert_eq!(goal.elapsed_active_ms, 0);
    assert_eq!(goal.active_since_ms, None);
    store.run_goal(&task, 1, 20).unwrap();
    store
        .sync_goal_presence(&task, Some(20), None, false, 20)
        .unwrap();
    let paused = store.stop_goal(&task, 1, 5020).unwrap();
    assert_eq!(paused.elapsed_active_ms, 5000);
    assert_eq!(paused.active_since_ms, None);
    assert_eq!(
        store.stop_goal(&task, 1, 8000).unwrap().elapsed_active_ms,
        5000
    );
    let reloaded = AgentAutomationStore::new(state_root).unwrap();
    reloaded.run_goal(&task, 1, 10000).unwrap();
    reloaded
        .sync_goal_presence(&task, Some(10000), None, false, 10000)
        .unwrap();
    assert_eq!(
        reloaded.run_goal(&task, 1, 11000).unwrap().active_since_ms,
        Some(10000)
    );
    let stopped = reloaded
        .pause_goal_for_agent_stop(&task, "stop-timed-turn", 12000)
        .unwrap()
        .goal
        .unwrap();
    assert_eq!(stopped.elapsed_active_ms, 7000);
    assert_eq!(stopped.active_since_ms, None);
    reloaded
        .apply_owner_goal_command(
            &task,
            "play-timed-goal",
            codexify::agent_automation::OwnerGoalCommand::Run,
            15000,
        )
        .unwrap();
    reloaded
        .sync_goal_presence(&task, Some(15000), None, false, 15000)
        .unwrap();
    let finished = reloaded
        .finish_goal(&task, 1, GoalFinish::Completed { summary: None }, 18000)
        .unwrap();
    assert_eq!(finished.elapsed_active_ms, 10000);
    assert_eq!(finished.active_since_ms, None);
    assert_eq!(
        reloaded
            .load(&task)
            .unwrap()
            .goal
            .unwrap()
            .elapsed_active_ms,
        10000
    );
}

#[test]
fn blocked_goals_resume_with_the_same_revision_time_and_turns() {
    use codexify::agent_automation::OwnerGoalCommand;
    for owner_command in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let store = AgentAutomationStore::new(root.path().join("automation")).unwrap();
        let task = task("resume-blocked-goal");
        store.set_goal(&task, "Continue this goal", 10).unwrap();
        store.run_goal(&task, 1, 20).unwrap();
        store
            .sync_goal_presence(&task, Some(20), None, false, 20)
            .unwrap();
        let url = "https://chatgpt.com/c/resume-blocked-goal";
        let pairing = store.issue_pairing(&task, url, 1, 21).unwrap();
        store
            .record_pairing_canonical_url(&task, pairing.generation, url, 22)
            .unwrap();
        store
            .verify_pairing(&task, pairing.generation, 1, &pairing.reference, 23)
            .unwrap();
        store
            .queue_action(
                &task,
                "timed-out-goal",
                AutomationActionKind::OfflineGoal,
                1,
                Some(1),
                30,
            )
            .unwrap();
        store.record_goal_turn(&task, "timed-out-goal").unwrap();
        store
            .update_action_status(
                &task,
                "timed-out-goal",
                AutomationActionStatus::Ambiguous,
                Some("Previous browser action timed out"),
                None,
                40,
            )
            .unwrap();
        let blocked = store
            .finish_goal(
                &task,
                1,
                GoalFinish::Blocked {
                    reason: "Waiting for input".into(),
                },
                5020,
            )
            .unwrap();
        assert_eq!(blocked.elapsed_active_ms, 5000);
        assert_eq!(blocked.turn_count, 2);

        let resumed = if owner_command {
            store
                .apply_owner_goal_command(&task, "resume-blocked", OwnerGoalCommand::Run, 10000)
                .unwrap()
                .goal
                .unwrap()
        } else {
            store.run_goal(&task, 1, 10000).unwrap()
        };
        assert_eq!(resumed.status, GoalStatus::Active);
        assert_eq!(resumed.revision, blocked.revision);
        assert_eq!(resumed.markdown, blocked.markdown);
        assert_eq!(resumed.created_at_ms, blocked.created_at_ms);
        assert_eq!(resumed.elapsed_active_ms, 5000);
        assert_eq!(resumed.active_since_ms, None);
        store
            .sync_goal_presence(&task, Some(10000), None, false, 10000)
            .unwrap();
        assert_eq!(resumed.turn_count, 2);
        assert!(resumed.finished_at_ms.is_none());
        assert!(resumed.blocked_reason.is_none());
        assert!(store.load(&task).unwrap().action.id.is_none());
        assert_eq!(
            store.stop_goal(&task, 1, 13000).unwrap().elapsed_active_ms,
            8000
        );
    }
}

#[test]
fn goal_timer_excludes_offline_gaps_and_stopped_goals_after_reload() {
    use codexify::markdown_chat::OFFLINE_AFTER_MS;
    let root = tempfile::tempdir().unwrap();
    let store = AgentAutomationStore::new(root.path().join("automation")).unwrap();
    let task = task("presence-timer");
    store.set_goal(&task, "Track actual presence", 10).unwrap();
    store.run_goal(&task, 1, 20).unwrap();
    store
        .sync_goal_presence(&task, None, None, false, 1000)
        .unwrap();
    assert!(
        store
            .load(&task)
            .unwrap()
            .goal
            .unwrap()
            .active_since_ms
            .is_none()
    );
    store
        .sync_goal_presence(&task, Some(2000), None, false, 2000)
        .unwrap();
    store
        .sync_goal_presence(&task, Some(4000), None, false, 4000)
        .unwrap();
    let reloaded = AgentAutomationStore::new(root.path().join("automation")).unwrap();
    reloaded
        .sync_goal_presence(&task, Some(4000), None, false, 900000)
        .unwrap();
    let offline = reloaded.load(&task).unwrap().goal.unwrap();
    assert_eq!(offline.elapsed_active_ms, OFFLINE_AFTER_MS + 2000);
    assert!(offline.active_since_ms.is_none());
    assert_eq!(offline.status, GoalStatus::Active);
    reloaded
        .sync_goal_presence(&task, Some(950000), None, false, 950000)
        .unwrap();
    let paused = reloaded.stop_goal(&task, 1, 955000).unwrap();
    assert_eq!(paused.elapsed_active_ms, OFFLINE_AFTER_MS + 7000);
    reloaded
        .sync_goal_presence(&task, Some(960000), None, false, 960000)
        .unwrap();
    assert_eq!(
        reloaded
            .load(&task)
            .unwrap()
            .goal
            .unwrap()
            .elapsed_active_ms,
        paused.elapsed_active_ms
    );
    reloaded.run_goal(&task, 1, 970000).unwrap();
    reloaded
        .sync_goal_presence(&task, Some(960000), None, false, 970000)
        .unwrap();
    assert_eq!(
        reloaded.load(&task).unwrap().goal.unwrap().active_since_ms,
        Some(970000)
    );
    reloaded
        .sync_goal_presence(&task, Some(960000), None, true, 975000)
        .unwrap();
    let stopped = reloaded.load(&task).unwrap().goal.unwrap();
    assert!(stopped.active_since_ms.is_none());
    assert_eq!(stopped.elapsed_active_ms, OFFLINE_AFTER_MS + 12000);
}

#[test]
fn retrying_pre_send_attention_and_continuing_goals_preserves_message_fences() {
    use codexify::agent_automation::OwnerGoalCommand;
    let root = tempfile::tempdir().unwrap();
    let store = AgentAutomationStore::new(root.path().join("automation")).unwrap();
    let task = task("attention-retry");
    store
        .apply_owner_goal_command(
            &task,
            "set-attention-goal",
            OwnerGoalCommand::Set {
                markdown: "Continue".into(),
            },
            1,
        )
        .unwrap();
    let url = "https://chatgpt.com/c/attention-retry";
    let pairing = store.issue_pairing(&task, url, 1, 2).unwrap();
    store
        .record_pairing_canonical_url(&task, pairing.generation, url, 3)
        .unwrap();
    store
        .verify_pairing(&task, pairing.generation, 1, &pairing.reference, 4)
        .unwrap();
    for status in [
        AutomationActionStatus::Interference,
        AutomationActionStatus::AuthenticationRequired,
        AutomationActionStatus::ApprovalRequired,
        AutomationActionStatus::RateLimited,
    ] {
        store
            .queue_action(
                &task,
                "before-send",
                AutomationActionKind::OfflineCombined,
                pairing.generation,
                Some(1),
                5,
            )
            .unwrap();
        store
            .update_action_status(
                &task,
                "before-send",
                status,
                Some("Resolve this condition, then retry"),
                None,
                6,
            )
            .unwrap();
        assert_eq!(store.load(&task).unwrap().retry_mode(), Some("retry"));
        store
            .reset_failed_action_for_retry(&task, "before-send", 7)
            .unwrap();
    }
    store
        .queue_action(
            &task,
            "accepted-goal",
            AutomationActionKind::OfflineGoal,
            pairing.generation,
            Some(1),
            8,
        )
        .unwrap();
    store.record_goal_turn(&task, "accepted-goal").unwrap();
    store
        .update_action_status(
            &task,
            "accepted-goal",
            AutomationActionStatus::Ambiguous,
            Some("Completion unknown"),
            None,
            9,
        )
        .unwrap();
    assert_eq!(
        store.load(&task).unwrap().retry_mode(),
        Some("continue_goal")
    );
    store
        .reset_failed_action_for_retry(&task, "accepted-goal", 10)
        .unwrap();
    assert_eq!(store.load(&task).unwrap().goal.unwrap().turn_count, 2);
    store
        .queue_action(
            &task,
            "unknown-message",
            AutomationActionKind::OfflineChat,
            pairing.generation,
            None,
            11,
        )
        .unwrap();
    store.record_goal_turn(&task, "unknown-message").unwrap();
    store
        .update_action_status(
            &task,
            "unknown-message",
            AutomationActionStatus::Ambiguous,
            Some("Message may be sent"),
            None,
            12,
        )
        .unwrap();
    assert_eq!(store.load(&task).unwrap().retry_mode(), None);
    assert!(
        store
            .reset_failed_action_for_retry(&task, "unknown-message", 13)
            .is_err()
    );
}

#[test]
fn concurrent_agent_goal_updates_compare_revisions_under_the_mutation_lock() {
    use codexify::agent_automation::OwnerGoalCommand;
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("automation");
    let task = task("agent-concurrent-goal-edit");
    let store = AgentAutomationStore::new(state_root.clone()).unwrap();
    // Owner and agent identifiers are independent, even with identical raw IDs.
    store
        .apply_owner_goal_command(
            &task,
            "request-a",
            OwnerGoalCommand::Set {
                markdown: "Initial".into(),
            },
            10,
        )
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let attempts: Vec<_> = ["request-a", "request-b"]
        .into_iter()
        .map(|request_id| {
            let state_root = state_root.clone();
            let task = task.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = AgentAutomationStore::new(state_root).unwrap();
                barrier.wait();
                store.apply_agent_goal_command(
                    &task,
                    request_id,
                    1,
                    OwnerGoalCommand::Set {
                        markdown: request_id.into(),
                    },
                    20,
                )
            })
        })
        .collect();
    let results: Vec<_> = attempts
        .into_iter()
        .map(|attempt| attempt.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let failure = results
        .iter()
        .find_map(|result| result.as_ref().err())
        .unwrap();
    assert!(failure.contains("revision changed"));
    let goal = store.load(&task).unwrap().goal.unwrap();
    assert_eq!(goal.revision, 2);
    assert_eq!(goal.status, GoalStatus::Active);
    assert!(["request-a", "request-b"].contains(&goal.markdown.as_str()));
}

#[test]
fn legacy_goal_tracking_starts_once_at_upgrade_without_inventing_history() {
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("automation");
    let store = AgentAutomationStore::new(state_root.clone()).unwrap();
    let task = task("legacy-goal-timer");
    store
        .apply_owner_goal_command(
            &task,
            "legacy-set",
            codexify::agent_automation::OwnerGoalCommand::Set {
                markdown: "Already active".into(),
            },
            10,
        )
        .unwrap();
    let path = store.task_directory(&task).unwrap().join("automation.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for key in ["elapsedActiveMs", "activeSinceMs", "turnCount"] {
        state["goal"].as_object_mut().unwrap().remove(key);
    }
    for record in state["ownerGoalRequests"].as_array_mut().unwrap() {
        for key in ["elapsedActiveMs", "activeSinceMs", "turnCount"] {
            record["receipt"]["goal"]
                .as_object_mut()
                .unwrap()
                .remove(key);
        }
    }
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    let reloaded = AgentAutomationStore::new(state_root).unwrap();
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let goal = reloaded.load(&task).unwrap().goal.unwrap();
    assert!(goal.updated_at_ms >= before);
    assert!(goal.active_since_ms.is_none());
    assert_eq!(goal.elapsed_active_ms, 0);
    assert_eq!(goal.turn_count, 1);
    assert_eq!(
        reloaded.load(&task).unwrap().goal.unwrap().active_since_ms,
        goal.active_since_ms
    );
}

#[test]
fn editing_an_active_goal_preserves_progress_and_invalidates_the_old_revision() {
    use codexify::agent_automation::{OwnerGoalCommand, OwnerGoalCommandReceipt};
    let root = tempfile::tempdir().unwrap();
    let store = AgentAutomationStore::new(root.path().join("automation")).unwrap();
    let task = task("active-goal-edit");
    let first = store
        .apply_owner_goal_command(
            &task,
            "initial-goal",
            OwnerGoalCommand::Set {
                markdown: "Original goal".into(),
            },
            10,
        )
        .unwrap();
    assert!(!first.updated_active_goal);
    store
        .sync_goal_presence(&task, Some(20), None, false, 20)
        .unwrap();
    let pairing = store
        .issue_pairing(&task, "https://chatgpt.com/c/active-goal-edit", 0, 21)
        .unwrap();
    store
        .record_pairing_canonical_url(
            &task,
            pairing.generation,
            "https://chatgpt.com/c/active-goal-edit",
            22,
        )
        .unwrap();
    store
        .verify_pairing(&task, pairing.generation, 0, &pairing.reference, 23)
        .unwrap();
    store
        .queue_action(
            &task,
            "first-turn",
            AutomationActionKind::OfflineGoal,
            pairing.generation,
            Some(1),
            30,
        )
        .unwrap();
    store.record_goal_turn(&task, "first-turn").unwrap();
    let before = store.load(&task).unwrap().goal.unwrap();
    let edit = || {
        store.apply_owner_goal_command(
            &task,
            "edit-goal",
            OwnerGoalCommand::Set {
                markdown: "Updated goal".into(),
            },
            1020,
        )
    };
    let receipt = edit().unwrap();
    assert!(receipt.updated_active_goal);
    let after = receipt.goal.unwrap();
    assert_eq!(after.revision, 2);
    assert_eq!(after.markdown, "Updated goal");
    assert_eq!(after.status, GoalStatus::Active);
    assert_eq!(after.created_at_ms, before.created_at_ms);
    assert_eq!(after.active_since_ms, before.active_since_ms);
    assert_eq!(after.elapsed_active_ms, before.elapsed_active_ms);
    assert_eq!(after.timer_until_ms, before.timer_until_ms);
    assert_eq!(after.turn_count, 2);
    assert!(
        store
            .finish_goal(&task, 1, GoalFinish::Completed { summary: None }, 1030)
            .is_err()
    );
    let replayed = edit().unwrap();
    assert!(replayed.replayed && replayed.updated_active_goal);
    assert_eq!(replayed.goal.unwrap(), after);
    let mut old_receipt = serde_json::to_value(first).unwrap();
    old_receipt
        .as_object_mut()
        .unwrap()
        .remove("updatedActiveGoal");
    let compatible: OwnerGoalCommandReceipt = serde_json::from_value(old_receipt).unwrap();
    assert!(!compatible.updated_active_goal);
}

#[test]
fn new_chat_verification_race_preserves_the_one_time_reference_until_url_is_ready() {
    let root = tempfile::tempdir().unwrap();
    let store = AgentAutomationStore::new(root.path().join("automation")).unwrap();
    let task = task("creation-race");
    let issue = store
        .issue_pairing(&task, "https://chatgpt.com/g/g-p-project/project", 1, 100)
        .unwrap();
    store.mark_creating_chat(&task).unwrap();
    let first = store
        .verify_pairing_reference(&task, 1, &issue.reference, 101)
        .unwrap_err();
    assert!(first.contains("not ready"));
    assert!(store.load(&task).unwrap().pairing.pending.is_some());
    store
        .record_pairing_canonical_url(
            &task,
            issue.generation,
            "https://chatgpt.com/c/created",
            102,
        )
        .unwrap();
    let pairing = store
        .verify_pairing_reference(&task, 1, &issue.reference, 103)
        .unwrap();
    assert_eq!(pairing.verified_owner_generation, Some(1));
    assert!(!pairing.creating_chat);
}

#[test]
fn a_late_valid_pairing_clears_the_error_and_retains_the_created_conversation_url() {
    use codexify::agent_automation::PAIRING_VERIFICATION_GRACE_MS;
    let root = tempfile::tempdir().unwrap();
    let store = AgentAutomationStore::new(root.path().join("automation")).unwrap();
    let task = task("late-created-pairing");
    let issue = store
        .issue_pairing(&task, "https://chatgpt.com/g/g-p-project/project", 1, 1000)
        .unwrap();
    store.mark_creating_chat(&task).unwrap();
    store
        .queue_action(
            &task,
            "creation-action",
            AutomationActionKind::PairVerify,
            issue.generation,
            None,
            1001,
        )
        .unwrap();
    store
        .record_pairing_submission(&task, "creation-action", 1100)
        .unwrap();
    store
        .record_pairing_canonical_url(
            &task,
            issue.generation,
            "https://chatgpt.com/c/created-late",
            1101,
        )
        .unwrap();
    store
        .update_action_status(
            &task,
            "creation-action",
            AutomationActionStatus::Ambiguous,
            Some("Preference restoration timed out"),
            None,
            1200,
        )
        .unwrap();
    let state = store.load(&task).unwrap();
    assert_eq!(
        state.pairing.last_attempted_url.as_deref(),
        Some("https://chatgpt.com/c/created-late")
    );
    assert!(!state.action_needs_attention_at(1100 + PAIRING_VERIFICATION_GRACE_MS - 1));
    assert!(state.action_needs_attention_at(1100 + PAIRING_VERIFICATION_GRACE_MS));
    let verified = store
        .verify_pairing_reference(
            &task,
            1,
            &issue.reference,
            1100 + PAIRING_VERIFICATION_GRACE_MS + 1000,
        )
        .unwrap();
    assert_eq!(verified.verified_owner_generation, Some(1));
    let state = store.load(&task).unwrap();
    assert_eq!(state.action.status, AutomationActionStatus::Idle);
    assert!(state.action.last_error.is_none());
    assert!(!state.action_needs_attention_at(u64::MAX));
    // A later browser failure cannot undo authenticated tool verification.
    store
        .update_action_status(
            &task,
            "creation-action",
            AutomationActionStatus::Failed,
            Some("Late cleanup failure"),
            None,
            100_000,
        )
        .unwrap();
    assert_eq!(
        store.load(&task).unwrap().action.status,
        AutomationActionStatus::Idle
    );
    assert!(store.load(&task).unwrap().action.last_error.is_none());
    assert!(
        store
            .verify_pairing_reference(&task, 1, &issue.reference, 100_001)
            .is_err()
    );
}

#[test]
fn created_urls_survive_expired_verification_but_project_urls_and_superseded_requests_are_rejected()
{
    let root = tempfile::tempdir().unwrap();
    let state_root = root.path().join("automation");
    let store = AgentAutomationStore::new(state_root.clone()).unwrap();
    let task = task("expired-created-url");
    let issue = store
        .issue_pairing(&task, "https://chatgpt.com/g/g-p-project/project", 1, 1000)
        .unwrap();
    store.mark_creating_chat(&task).unwrap();
    assert!(
        store
            .record_pairing_canonical_url(
                &task,
                issue.generation,
                "https://chatgpt.com/g/g-p-project/project",
                1001
            )
            .is_err()
    );
    let url = "https://chatgpt.com/c/created-after-expiry";
    assert!(
        store
            .record_pairing_canonical_url(
                &task,
                issue.generation,
                url,
                1000 + PAIRING_REFERENCE_TTL_MS + 1
            )
            .is_err()
    );
    let reloaded = AgentAutomationStore::new(state_root).unwrap();
    assert_eq!(
        reloaded
            .load(&task)
            .unwrap()
            .pairing
            .last_attempted_url
            .as_deref(),
        Some(url)
    );
    assert_eq!(
        reloaded
            .load(&task)
            .unwrap()
            .pairing
            .canonical_url
            .as_deref(),
        Some(url)
    );
    assert!(
        reloaded
            .verify_pairing_reference(
                &task,
                1,
                &issue.reference,
                1000 + PAIRING_REFERENCE_TTL_MS + 2
            )
            .is_err()
    );
    let current = reloaded
        .issue_pairing(&task, "https://chatgpt.com/c/another", 1, 1_000_000)
        .unwrap();
    assert!(
        reloaded
            .record_pairing_canonical_url(&task, issue.generation, url, 1_000_001)
            .is_err()
    );
    assert_eq!(
        reloaded.load(&task).unwrap().pairing.generation,
        current.generation
    );
    assert!(
        reloaded
            .load(&task)
            .unwrap()
            .pairing
            .canonical_url
            .is_none()
    );
}

#[test]
fn authentication_retries_use_six_separate_intervals_and_never_replay_uncertain_messages() {
    use codexify::agent_automation::{AutomationActionState, AutomationTaskState};
    let mut state = AutomationTaskState::default();
    state.action = AutomationActionState {
        id: Some("authentication-failure".into()),
        kind: Some(AutomationActionKind::OfflineChat),
        status: AutomationActionStatus::AuthenticationRequired,
        send_accepted: Some(false),
        updated_at_ms: Some(100),
        automatic_retry_attempts: 4,
        ..Default::default()
    };
    for (attempt, minutes) in [3, 5, 10, 20, 30, 60].into_iter().enumerate() {
        state.action.chatgpt_access_retry_attempts = attempt as u32;
        let delay = minutes * 60_000;
        let deadline = state.automatic_retry_at_ms().unwrap();
        assert!((100 + delay..=100 + delay + delay / 10).contains(&deadline));
        assert_eq!(state.automatic_retry_limit(), 6);
        assert_eq!(state.automatic_retry_attempts(), attempt as u32);
        let restored: AutomationTaskState =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        assert_eq!(restored.automatic_retry_at_ms(), Some(deadline));
        assert!(restored.is_automatically_retrying());
        for status in [
            AutomationActionStatus::ConversationUnavailable,
            AutomationActionStatus::RateLimited,
        ] {
            let mut access_failure = state.clone();
            access_failure.action.status = status;
            assert_eq!(access_failure.automatic_retry_at_ms(), Some(deadline));
            assert_eq!(access_failure.automatic_retry_limit(), 6);
            assert_eq!(access_failure.automatic_retry_attempts(), attempt as u32);
        }
    }
    state.action.chatgpt_access_retry_attempts = 6;
    for status in [
        AutomationActionStatus::AuthenticationRequired,
        AutomationActionStatus::ConversationUnavailable,
        AutomationActionStatus::RateLimited,
    ] {
        let mut exhausted = state.clone();
        exhausted.action.status = status;
        assert!(exhausted.automatic_retry_at_ms().is_none());
        assert!(!exhausted.is_automatically_retrying());
    }
    assert_eq!(state.retry_mode(), Some("retry"));
    state.action.chatgpt_access_retry_attempts = 0;
    state.action.status = AutomationActionStatus::ConversationUnavailable;
    for status in [
        AutomationActionStatus::AuthenticationRequired,
        AutomationActionStatus::ConversationUnavailable,
        AutomationActionStatus::RateLimited,
    ] {
        let mut disabled = state.clone();
        disabled.action.status = status;
        disabled.automatic_retries_enabled = false;
        assert!(disabled.automatic_retry_at_ms().is_none());
        disabled.automatic_retries_enabled = true;
        disabled.action.automatic_retry_paused = true;
        assert!(disabled.automatic_retry_at_ms().is_none());
        for variant in [
            "accepted",
            "unknown",
            "fenced",
            "counted",
            "pairing",
            "cancelled",
        ] {
            let mut unsafe_state = state.clone();
            unsafe_state.action.status = status;
            match variant {
                "accepted" => unsafe_state.action.send_accepted = Some(true),
                "unknown" => unsafe_state.action.send_accepted = None,
                "fenced" => unsafe_state.action.messages_fenced = true,
                "counted" => unsafe_state.action.goal_turn_counted = true,
                "pairing" => unsafe_state.action.kind = Some(AutomationActionKind::PairVerify),
                "cancelled" => unsafe_state.action.status = AutomationActionStatus::Cancelled,
                _ => unreachable!(),
            }
            assert!(
                unsafe_state.automatic_retry_at_ms().is_none(),
                "{variant} must not schedule an authentication replay"
            );
        }
    }
    let root = tempfile::tempdir().unwrap();
    let store = AgentAutomationStore::new(root.path().join("state")).unwrap();
    let task = task("authentication-goal");
    let goal = store.set_goal(&task, "Continue", 1).unwrap();
    let active = store.run_goal(&task, goal.revision, 2).unwrap();
    for status in [
        AutomationActionStatus::ConversationUnavailable,
        AutomationActionStatus::RateLimited,
    ] {
        state.action.status = status;
        for kind in [
            AutomationActionKind::OfflineGoal,
            AutomationActionKind::OfflineCombined,
        ] {
            state.action.kind = Some(kind);
            state.action.goal_revision = Some(active.revision);
            state.goal = Some(active.clone());
            assert!(state.automatic_retry_at_ms().is_some());
            for status in [
                GoalStatus::Configured,
                GoalStatus::Blocked,
                GoalStatus::Completed,
            ] {
                state.goal.as_mut().unwrap().status = status;
                assert!(state.automatic_retry_at_ms().is_none());
            }
        }
    }
    let mut legacy = serde_json::to_value(AutomationTaskState::default()).unwrap();
    for key in ["chatgptAccessRetryAttempts", "chatgptAccessRetryInProgress"] {
        legacy["action"].as_object_mut().unwrap().remove(key);
    }
    let restored: AutomationTaskState = serde_json::from_value(legacy).unwrap();
    assert_eq!(restored.action.chatgpt_access_retry_attempts, 0);
}

#[test]
fn retry_jitter_is_stable_bounded_and_spreads_chats_on_each_schedule() {
    use codexify::agent_automation::{AutomationActionState, AutomationTaskState};
    let root = tempfile::tempdir().unwrap();
    let store = AgentAutomationStore::new(root.path().join("state")).unwrap();
    let task = task("jitter-goal");
    let goal = store.set_goal(&task, "Continue", 1).unwrap();
    let active = store.run_goal(&task, goal.revision, 2).unwrap();
    for (status, kind, delay, accepted) in [
        (
            AutomationActionStatus::Failed,
            AutomationActionKind::OfflineChat,
            30_000,
            false,
        ),
        (
            AutomationActionStatus::AuthenticationRequired,
            AutomationActionKind::OfflineChat,
            180_000,
            false,
        ),
        (
            AutomationActionStatus::ConversationUnavailable,
            AutomationActionKind::OfflineChat,
            180_000,
            false,
        ),
        (
            AutomationActionStatus::Stalled,
            AutomationActionKind::OfflineGoal,
            60_000,
            true,
        ),
    ] {
        let mut deadlines = std::collections::HashSet::new();
        for index in 0..100 {
            let mut state = AutomationTaskState::default();
            state.goal = Some(active.clone());
            state.action = AutomationActionState {
                id: Some(format!("chat-{index}")),
                kind: Some(kind),
                status,
                goal_revision: (kind == AutomationActionKind::OfflineGoal)
                    .then_some(active.revision),
                send_accepted: Some(accepted),
                updated_at_ms: Some(100),
                ..Default::default()
            };
            let deadline = state.automatic_retry_at_ms().unwrap();
            assert!((100 + delay..=100 + delay + delay / 10).contains(&deadline));
            assert_eq!(state.automatic_retry_at_ms(), Some(deadline));
            let restored: AutomationTaskState =
                serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
            assert_eq!(restored.automatic_retry_at_ms(), Some(deadline));
            deadlines.insert(deadline);
        }
        assert!(
            deadlines.len() > 90,
            "independent chats must not share one fixed retry time"
        );
    }
}

#[test]
fn configured_retry_timings_survive_restart_and_shortening_without_changing_send_safety() {
    use codexify::agent_automation::{AutomationActionState, AutomationTaskState};
    let root = tempfile::tempdir().unwrap();
    let mut config = codexify::config::default_config(root.path().to_owned());
    let timings = &mut config.markdown_chat.automation.retry_timings;
    timings.action_delays_seconds = vec![7, 19];
    timings.chatgpt_access_delays_seconds = vec![11, 23, 37, 41, 43, 47, 53];
    timings.goal_recovery_delays_seconds = vec![13, 29, 31, 59];
    timings.jitter_percent = 0;
    timings.approval_resume_grace_seconds = 17;
    let store = AgentAutomationStore::for_state_root(&config, root.path()).unwrap();
    let task = task("custom-retry-policy");
    store
        .issue_pairing(&task, "https://chatgpt.com/c/custom", 0, 1)
        .unwrap();
    let goal = store.set_goal(&task, "Continue", 1).unwrap();
    store.run_goal(&task, goal.revision, 2).unwrap();
    for (status, kind, delays, accepted) in [
        (
            AutomationActionStatus::Failed,
            AutomationActionKind::OfflineChat,
            vec![7, 19],
            false,
        ),
        (
            AutomationActionStatus::ConversationUnavailable,
            AutomationActionKind::OfflineChat,
            vec![11, 23, 37, 41, 43, 47, 53],
            false,
        ),
        (
            AutomationActionStatus::RateLimited,
            AutomationActionKind::OfflineChat,
            vec![11, 23, 37, 41, 43, 47, 53],
            false,
        ),
        (
            AutomationActionStatus::Stalled,
            AutomationActionKind::OfflineGoal,
            vec![13, 29, 31, 59],
            true,
        ),
    ] {
        let mut state = store.load(&task).unwrap();
        state.action = AutomationActionState {
            id: Some("custom-failure".into()),
            pairing_generation: Some(state.pairing.generation),
            kind: Some(kind),
            status,
            goal_revision: (kind == AutomationActionKind::OfflineGoal).then_some(goal.revision),
            send_accepted: Some(accepted),
            updated_at_ms: Some(100),
            ..Default::default()
        };
        let path = store.task_directory(&task).unwrap().join("automation.json");
        for attempt in 0..=delays.len() {
            state.action.automatic_retry_attempts = if status == AutomationActionStatus::Failed {
                attempt as u32
            } else {
                0
            };
            state.action.chatgpt_access_retry_attempts = if matches!(
                status,
                AutomationActionStatus::ConversationUnavailable
                    | AutomationActionStatus::RateLimited
            ) {
                attempt as u32
            } else {
                0
            };
            state.goal_wake.recovery_attempts = if kind == AutomationActionKind::OfflineGoal {
                attempt as u32
            } else {
                0
            };
            let bytes = serde_json::to_vec(&state).unwrap();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(json.get("retryTimings").is_none());
            assert!(json.get("offlineResponseGraceMs").is_none());
            std::fs::write(&path, &bytes).unwrap();
            let restarted = AgentAutomationStore::for_state_root(&config, root.path())
                .unwrap()
                .load(&task)
                .unwrap();
            assert_eq!(restarted.automatic_retry_limit(), delays.len());
            assert_eq!(
                restarted.automatic_retry_at_ms(),
                delays.get(attempt).map(|seconds| {
                    let delay = seconds * 1000;
                    100 + if accepted {
                        delay.max(config.markdown_chat.offline_timeout_minutes * 60_000)
                    } else {
                        delay
                    }
                })
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            if !accepted {
                for evidence in [None, Some(true)] {
                    let mut uncertain = restarted.clone();
                    uncertain.action.send_accepted = evidence;
                    assert_eq!(uncertain.automatic_retry_at_ms(), None);
                }
            }
        }
        let mut shorter = config.clone();
        shorter
            .markdown_chat
            .automation
            .retry_timings
            .action_delays_seconds = vec![1];
        shorter
            .markdown_chat
            .automation
            .retry_timings
            .chatgpt_access_delays_seconds = vec![1];
        shorter
            .markdown_chat
            .automation
            .retry_timings
            .goal_recovery_delays_seconds = vec![1];
        let shorter = AgentAutomationStore::for_state_root(&shorter, root.path()).unwrap();
        assert_eq!(shorter.load(&task).unwrap().automatic_retry_at_ms(), None);
        // Mutations must remain possible when an old counter exceeds the new schedule length.
        shorter
            .set_selection(&task, "gpt-6-sol", "high", "catalog", "account", 500)
            .unwrap();
    }
    let mut state: AutomationTaskState = store.load(&task).unwrap();
    state.approval_resume=Some(serde_json::from_value(serde_json::json!({"resumedAtMs":100,"ownerGeneration":0,"pairingGeneration":0,"goalRevision":null})).unwrap());
    assert_eq!(state.approval_resume_until_ms(), Some(17_100));
    state.retry_timings.jitter_percent = 25;
    assert!((17_100..=21_350).contains(&state.approval_resume_until_ms().unwrap()));
}

#[test]
fn saved_local_cooldown_errors_migrate_to_queued_deadlines_without_new_backoff() {
    use codexify::agent_automation::AutomationActionState;
    for (index, message, expected) in [
        (
            0,
            "ChatGPT conversation loading is cooling down. Another conversation can load in 70s",
            Some(71_000),
        ),
        (
            1,
            "ChatGPT conversation temporarily not accessible. Shared browser cooldown: conversation loading can retry in 300s",
            Some(301_000),
        ),
        (2, "ChatGPT rejected the request with HTTP 429", None),
        (3, "ChatGPT conversation temporarily not accessible", None),
    ] {
        for (accepted, paused) in [(false, false), (true, false), (false, true)] {
            let root = tempfile::tempdir().unwrap();
            let store = AgentAutomationStore::new(root.path().join("state")).unwrap();
            let task = task(&format!("saved-cooldown-{index}-{accepted}-{paused}"));
            store.set_goal(&task, "Keep the objective", 1).unwrap();
            let mut state = store.load(&task).unwrap();
            state.action = AutomationActionState {
                id: Some("saved-action".into()),
                automatic_retry_paused: paused,
                pairing_generation: Some(1),
                kind: Some(AutomationActionKind::OfflineChat),
                status: AutomationActionStatus::RateLimited,
                send_accepted: Some(accepted),
                updated_at_ms: Some(1_000),
                last_error: Some(message.into()),
                chatgpt_access_retry_attempts: 2,
                ..Default::default()
            };
            let path = store.task_directory(&task).unwrap().join("automation.json");
            std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
            let loaded = store.load(&task).unwrap();
            assert_eq!(loaded.action.chatgpt_access_retry_attempts, 2);
            assert_eq!(loaded.goal, state.goal);
            if let Some(until) = expected.filter(|_| !accepted && !paused) {
                assert_eq!(loaded.action.status, AutomationActionStatus::Queued);
                assert_eq!(loaded.browser_cooldown_until_ms(), Some(until));
                assert!(loaded.action.last_error.is_none());
                assert!(loaded.automatic_retry_at_ms().is_none());
                assert!(!loaded.action_needs_attention());
                assert_eq!(store.load(&task).unwrap().action, loaded.action);
            } else {
                assert_eq!(loaded.action, state.action);
            }
        }
    }
}
