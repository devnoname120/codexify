use super::*;
use std::collections::HashMap;
use std::future::Future;
use tokio::task::JoinSet;

const MAX_PARALLEL_CHAT_READS: usize = 4;
type ListingResult<T> = Result<T, (StatusCode, String)>;
type ProjectGroups = HashMap<PathBuf, ProjectGroup>;

// Filesystem discovery and Git-backed grouping must not block a request/runtime
// worker. Each request resolves its own current bindings and project groups.
pub(super) fn discover(state: &OwnerState) -> (Vec<LocatedChat>, ProjectGroups) {
    let chats = locate(&state.config, &state.project_bindings);
    let mut groups = ProjectGroups::new();
    for located in &chats {
        let path = located.work_dir.as_ref().unwrap_or(&located.state_root);
        groups.entry(path.clone()).or_insert_with(|| {
            project_group(&state.config, path, &located.workspace, located.is_scratch)
        });
    }
    (chats, groups)
}

pub(super) async fn read_chats(
    state: OwnerState,
    chats: Vec<LocatedChat>,
    groups: ProjectGroups,
) -> ListingResult<Vec<ChatListing>> {
    let state = Arc::new(state);
    let groups = Arc::new(groups);
    read_bounded(chats, move |located| {
        let state = state.clone();
        let groups = groups.clone();
        async move {
            let path = located.work_dir.as_ref().unwrap_or(&located.state_root);
            let group = groups
                .get(path)
                .cloned()
                .ok_or_else(|| internal_error("Chat project group is unavailable"))?;
            read_chat(&state, located, group).await
        }
    })
    .await
}

async fn read_bounded<I, T, F, Fut>(items: Vec<I>, read: F) -> ListingResult<Vec<T>>
where
    I: Send + 'static,
    T: Send + 'static,
    F: Fn(I) -> Fut,
    Fut: Future<Output = ListingResult<T>> + Send + 'static,
{
    let mut items = items.into_iter();
    let mut pending = JoinSet::new();
    for _ in 0..MAX_PARALLEL_CHAT_READS {
        if let Some(item) = items.next() {
            pending.spawn(read(item));
        }
    }
    let mut rows = Vec::new();
    while let Some(result) = pending.join_next().await {
        rows.push(result.map_err(internal_error)??);
        if let Some(item) = items.next() {
            pending.spawn(read(item));
        }
    }
    Ok(rows)
}

async fn read_chat(
    state: &OwnerState,
    located: LocatedChat,
    project_group: ProjectGroup,
) -> ListingResult<ChatListing> {
    let chat = state
        .chats
        .chat_at_path(located.path.clone(), true)
        .map_err(internal_error)?;
    let summary = match chat.owner_summary().await {
        Ok(summary) => summary,
        Err(error) => {
            tracing::warn!("owner chat transcript unavailable: {error}");
            let metadata = std::fs::metadata(&located.path).map_err(internal_error)?;
            OwnerChatSummary {
                title: "Conversation unavailable".into(),
                custom_title: None,
                pinned_at_ms: None,
                archived_at_ms: None,
                owner_read_through: 0,
                last_entry_end: metadata.len(),
                last_incoming_entry_end: 0,
                last_entry_at_ms: metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis() as u64)
                    .unwrap_or_default(),
                last_agent_call_at_ms: None,
                last_user_entry_end: 0,
                last_user_entry_at_ms: None,
                agent_stop_requested: false,
                agent_waiting_until_ms: None,
                agent_idle_until_ms: None,
                agent_presence_until_ms: None,
                total_tool_calls: 0,
            }
        }
    };
    let wake_activity = wake::Activity::from(&summary)
        .with_offline_timeout(state.config.markdown_chat.offline_after_ms());
    let OwnerChatSummary {
        title,
        custom_title,
        pinned_at_ms,
        archived_at_ms,
        owner_read_through,
        last_entry_end,
        last_incoming_entry_end,
        last_entry_at_ms,
        last_user_entry_end,
        last_user_entry_at_ms,
        last_agent_call_at_ms,
        agent_waiting_until_ms,
        agent_idle_until_ms,
        agent_presence_until_ms,
        total_tool_calls,
        ..
    } = summary;
    let workspace_path = located.work_dir.as_ref().unwrap_or(&located.state_root);
    let project_path = workspace_path.to_string_lossy().into_owned();
    let project_id = format!("{:x}", Sha256::digest(project_path.as_bytes()));
    // Read persisted goal/action state without registering browser work.
    let goal_task = automation_task(&located)?;
    let team_task_key = goal_task.stable_key().to_owned();
    let goal_config = state.config.clone();
    let goal_root = located.state_root.clone();
    let automation = tokio::task::spawn_blocking(move || {
        AgentAutomationStore::for_state_root(&goal_config, &goal_root)?.load(&goal_task)
    })
    .await;
    let (automation, state_error) = match automation {
        Ok(Ok(automation)) => (automation, None),
        Ok(Err(error)) => {
            tracing::warn!(chat_id=%located.id, %error, "owner chat automation state unavailable");
            (Default::default(), Some(error))
        }
        Err(error) => {
            tracing::warn!(chat_id=%located.id, %error, "owner chat automation state task failed");
            (
                Default::default(),
                Some("Automation state is unavailable; saved data was preserved".into()),
            )
        }
    };
    let automation_retrying = state_error.is_none() && automation.is_automatically_retrying();
    let automation_error = state_error.or_else(|| {
        automation
            .action_needs_attention_at(crate::markdown_chat::now_ms())
            .then(|| {
                automation
                    .action
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "Browser action needs attention".into())
            })
    });
    let automation_waiting_for_browser =
        automation_error.is_none() && automation.browser_cooldown_until_ms().is_some();
    let now = crate::markdown_chat::now_ms();
    let enabled = state.config.markdown_chat.automation.enabled;
    let pending_queue = if enabled && automation_error.is_none() && wake_activity.offline(now) {
        chat.has_wakeable_queued_messages().await.unwrap_or(false)
    } else {
        false
    };
    let automation_wake = wake::status(&automation, &wake_activity, pending_queue, enabled, now);
    let agent_team = (state.config.markdown_chat.features.agent_folders && (automation.agents.parent_task_key.is_some()
        || !automation.agents.children.is_empty())).then(|| json!({
            "taskKey":team_task_key,
            "parentTaskKey":automation.agents.parent_task_key,
            "role":automation.agents.role,
            "scope":automation.agents.scope,
            "actionStatus":automation.action.status,
            "creating":automation.pairing.creating_chat,
            "children":automation.agents.children.iter().filter(|child|child.retired_at_ms.is_none()).map(|child|json!({
                "taskKey":child.task_key,"role":child.role,"scope":child.scope
            })).collect::<Vec<_>>()
        }));
    let goal = automation.goal;
    let goal_status = goal.as_ref().map(|goal| goal.status);
    let goal_completion_dismissed = goal.as_ref().is_some_and(|goal| {
        goal.status == crate::agent_automation::GoalStatus::Completed
            && goal.completed_chat_end.map_or_else(
                || {
                    last_user_entry_at_ms
                        .zip(goal.finished_at_ms)
                        .is_some_and(|(message, completed)| message > completed)
                },
                |completed_end| last_user_entry_end > completed_end,
            )
    });
    let completed_goal = goal
        .as_ref()
        .filter(|goal| goal.status == crate::agent_automation::GoalStatus::Completed);
    Ok(ChatListing {
        id: located.id,
        agent_team,
        title,
        custom_title,
        pinned_at_ms,
        archived_at_ms,
        owner_read_through,
        workspace: located.workspace,
        project_id,
        project_path,
        project_group,
        is_scratch: located.is_scratch,
        last_entry_end,
        last_incoming_entry_end,
        last_entry_at_ms,
        last_agent_call_at_ms,
        agent_waiting_until_ms,
        agent_idle_until_ms,
        agent_presence_until_ms,
        goal_status,
        goal_completion_dismissed,
        goal_completed_at_ms: completed_goal.and_then(|goal| goal.finished_at_ms),
        goal_completed_chat_end: completed_goal.and_then(|goal| goal.completed_chat_end),
        goal,
        automation_error,
        automation_retrying,
        automation_waiting_for_browser,
        automation_wake,
        total_tool_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

    #[tokio::test]
    async fn row_reads_overlap_with_a_four_row_limit() {
        let started = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let reading = tokio::spawn({
            let started = started.clone();
            let release = release.clone();
            let active = active.clone();
            let maximum = maximum.clone();
            async move {
                read_bounded((0..12).collect(), move |id| {
                    let started = started.clone();
                    let release = release.clone();
                    let active = active.clone();
                    let maximum = maximum.clone();
                    async move {
                        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                        maximum.fetch_max(count, Ordering::SeqCst);
                        started.add_permits(1);
                        release.acquire().await.unwrap().forget();
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(id)
                    }
                })
                .await
            }
        });
        // Serial reads cannot reach this barrier, and unbounded reads exceed it.
        tokio::time::timeout(Duration::from_secs(2), started.acquire_many(4))
            .await
            .unwrap()
            .unwrap()
            .forget();
        tokio::task::yield_now().await;
        assert_eq!(active.load(Ordering::SeqCst), 4);
        assert_eq!(started.available_permits(), 0);
        release.add_permits(12);
        let mut rows = reading.await.unwrap().unwrap();
        rows.sort_unstable();
        assert_eq!(rows, (0..12).collect::<Vec<_>>());
        assert_eq!(maximum.load(Ordering::SeqCst), 4);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn row_errors_keep_the_original_status_instead_of_returning_partial_success() {
        let result = read_bounded(vec![0, 1, 2], |id| async move {
            if id == 1 {
                Err((StatusCode::FORBIDDEN, "fixture row rejected".into()))
            } else {
                Ok(id)
            }
        })
        .await;
        assert_eq!(
            result.unwrap_err(),
            (StatusCode::FORBIDDEN, "fixture row rejected".into())
        );
    }

    #[tokio::test]
    async fn parallel_listing_reads_fresh_goals_titles_and_stops_without_crossing_projects() {
        use crate::agent_automation::GoalStatus;

        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().to_owned());
        let metadata = root.path().join("metadata");
        config.memory.dir = Some(metadata.to_string_lossy().into_owned());
        config.markdown_chat.enabled = true;
        config.markdown_chat.automation.enabled = true;
        let state = OwnerState {
            chats: Arc::new(MarkdownChatStore::default()),
            project_bindings: Arc::new(crate::project_bindings::ProjectBindingStore::new(
                root.path().join("bindings"),
            )),
            creation_lock: Arc::default(),
            artifacts: Arc::new(ArtifactEgressStore::new_at(
                config.artifact_egress.clone(),
                root.path().join("artifacts"),
            )),
            continuations: Arc::new(
                ConversationContinuationStore::new(root.path().join("continuations.json")).unwrap(),
            ),
            config: Arc::new(config),
            automation: None,
            open_browser: Arc::new(|_| Ok(())),
            catalog_cache: Arc::default(),
            global: Arc::default(),
            token: "listing-fixture".into(),
        };
        let mut fixtures = Vec::new();
        for index in 0..2 {
            // Same display name must never merge unrelated project roots.
            let project = root.path().join(index.to_string()).join("repo");
            std::fs::create_dir_all(&project).unwrap();
            let state_root = metadata.join(index.to_string());
            std::fs::create_dir_all(&state_root).unwrap();
            std::fs::write(
                state_root.join("memory.json"),
                json!({"workDir":project}).to_string(),
            )
            .unwrap();
            let task =
                ConversationIdentity::from_openai_session(&format!("listing-{index}")).unwrap();
            let chat = state
                .chats
                .chat_at_path(
                    state_root
                        .join("chats")
                        .join(task.stable_key())
                        .join("CHAT.md"),
                    true,
                )
                .unwrap();
            chat.append_user(format!("message-{index}"), format!("Original {index}"))
                .await
                .unwrap();
            let store = AgentAutomationStore::for_state_root(&state.config, &state_root).unwrap();
            let goal = store.set_goal(&task, &format!("Goal {index}"), 1).unwrap();
            store.run_goal(&task, goal.revision, 2).unwrap();
            fixtures.push((chat, store, task));
        }
        let list = || list_chats(State(state.clone()), Query(ListQuery::default()));
        let first = list().await.unwrap().0;
        let first = first["chats"].as_array().unwrap();
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|row| row["goalStatus"] == "active"));
        assert_ne!(first[0]["projectId"], first[1]["projectId"]);
        assert_ne!(
            first[0]["projectGroup"]["path"],
            first[1]["projectGroup"]["path"]
        );

        let (chat, store, task) = &fixtures[0];
        chat.set_owner_title("Fresh owner title".into())
            .await
            .unwrap();
        chat.set_owner_organization(None, Some(true)).await.unwrap();
        chat.request_agent_stop("explicit-stop".into())
            .await
            .unwrap();
        let revision = store.load(task).unwrap().goal.unwrap().revision;
        store.stop_goal(task, revision, 3).unwrap();
        let cursor_path = chat.path().with_file_name("cursor.json");
        let cursor_before = std::fs::read(&cursor_path).unwrap();
        let (other_chat, _, _) = &fixtures[1];
        let broken_state = other_chat.path().with_file_name("automation.json");
        std::fs::write(&broken_state, b"{invalid saved state").unwrap();

        let second = list().await.unwrap().0;
        let rows = second["chats"].as_array().unwrap();
        assert_eq!(
            rows.len(),
            2,
            "a damaged automation state retains its chat row"
        );
        let fresh = rows
            .iter()
            .find(|row| row["title"] == "Fresh owner title")
            .unwrap();
        assert_eq!(fresh["goalStatus"], "configured");
        assert_eq!(
            store.load(task).unwrap().goal.unwrap().status,
            GoalStatus::Configured
        );
        assert!(fresh["archivedAtMs"].as_u64().is_some());
        assert!(
            fresh["automationWake"].is_null(),
            "an explicit stop never becomes a queued wake"
        );
        assert!(chat.agent_stop_requested().await.unwrap());
        assert_eq!(
            std::fs::read(cursor_path).unwrap(),
            cursor_before,
            "listing must not advance agent/owner cursors or clear stops"
        );
        let damaged = rows.iter().find(|row| row["id"] != fresh["id"]).unwrap();
        assert!(damaged["automationError"].as_str().is_some());
        assert_eq!(
            std::fs::read(broken_state).unwrap(),
            b"{invalid saved state"
        );
    }
}
