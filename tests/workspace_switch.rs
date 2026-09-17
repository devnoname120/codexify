use codexify::config::default_config;
use codexify::project_bindings::{ConversationIdentity, ProjectBindingState, ProjectBindingStore};
use codexify::types::{AppConfig, WorktreeMode};
use std::path::Path;

fn fixture() -> (
    tempfile::TempDir,
    AppConfig,
    ProjectBindingStore,
    ConversationIdentity,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("projects");
    std::fs::create_dir_all(root.join("first")).unwrap();
    std::fs::create_dir_all(root.join("second")).unwrap();
    std::fs::write(root.join("first/keep.txt"), "keep the user's edits").unwrap();
    let mut config = default_config(root);
    config.multi_project = true;
    config.worktrees.mode = WorktreeMode::Never;
    config.memory.dir = Some(temp.path().join("metadata").display().to_string());
    let store = ProjectBindingStore::new(temp.path().join("bindings"));
    let identity = ConversationIdentity::from_openai_session("switch-test").unwrap();
    (temp, config, store, identity)
}

#[tokio::test]
async fn user_switch_persists_selection_and_requires_a_fresh_brief() {
    let (temp, config, store, identity) = fixture();
    let first = store
        .select_project_root(&config, &identity, "first")
        .await
        .unwrap();
    assert!(
        store
            .pending_workspace_change(&config, &identity)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .switch_to_picker(&config, &identity, Path::new("stale-card"))
            .await
            .is_err()
    );
    store
        .switch_to_picker(&config, &identity, &first.project_root)
        .await
        .unwrap();
    store
        .switch_to_picker(&config, &identity, &first.project_root)
        .await
        .unwrap();
    assert!(matches!(
        store.binding_state(&config, &identity).unwrap(),
        ProjectBindingState::Unselected { .. }
    ));
    assert_eq!(
        std::fs::read_to_string(first.project_root.join("keep.txt")).unwrap(),
        "keep the user's edits"
    );
    let restarted = ProjectBindingStore::new(temp.path().join("bindings"));
    assert!(restarted.effective_config(&config, &identity).is_err());
    assert!(
        restarted
            .select_project_root(&config, &identity, "missing")
            .await
            .is_err()
    );
    let second = restarted
        .select_project_root(&config, &identity, "second")
        .await
        .unwrap();
    let change = restarted
        .pending_workspace_change(&config, &identity)
        .unwrap()
        .unwrap();
    assert!(!change.awaiting_selection);
    assert!(
        change
            .notice(Some(&second.project_root))
            .contains("get_agent_brief")
    );
    assert!(
        change
            .notice(Some(&second.project_root))
            .contains(&second.project_root.display().to_string())
    );
    restarted
        .acknowledge_workspace_change(&config, &identity, "stale-brief", &second.project_root)
        .await
        .unwrap();
    assert!(
        restarted
            .pending_workspace_change(&config, &identity)
            .unwrap()
            .is_some()
    );
    restarted
        .acknowledge_workspace_change(&config, &identity, &change.revision, &first.project_root)
        .await
        .unwrap();
    assert!(
        restarted
            .pending_workspace_change(&config, &identity)
            .unwrap()
            .is_some()
    );
    restarted
        .acknowledge_workspace_change(&config, &identity, &change.revision, &second.project_root)
        .await
        .unwrap();
    assert!(
        restarted
            .pending_workspace_change(&config, &identity)
            .unwrap()
            .is_none()
    );
    let fresh = ConversationIdentity::from_openai_session("reuse-after-switch").unwrap();
    let reused = restarted
        .resume_workspace(&config, &fresh, first.project_root.to_str().unwrap())
        .await
        .unwrap();
    assert!(
        matches!(reused, codexify::project_bindings::WorkspaceSelection::Project(selection) if selection.project_root == first.project_root)
    );
}

#[tokio::test]
async fn scratch_and_project_switches_leave_old_files_and_bindings_reusable() {
    let (_temp, config, store, identity) = fixture();
    let scratch = store
        .select_without_project(&config, &identity)
        .await
        .unwrap();
    std::fs::write(scratch.scratch_root.join("notes.txt"), "keep scratch").unwrap();
    store
        .switch_to_picker(&config, &identity, &scratch.scratch_root)
        .await
        .unwrap();
    let project = store
        .select_project_root(&config, &identity, "first")
        .await
        .unwrap();
    assert!(project.newly_selected);
    store
        .switch_to_picker(&config, &identity, &project.project_root)
        .await
        .unwrap();
    store
        .resume_workspace(&config, &identity, scratch.scratch_root.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(scratch.scratch_root.join("notes.txt")).unwrap(),
        "keep scratch"
    );
    assert_eq!(
        store
            .selected_project_root(&config, &identity)
            .unwrap()
            .unwrap(),
        scratch.scratch_root
    );
}
