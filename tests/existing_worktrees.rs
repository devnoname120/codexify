use codexify::config::default_config;
use codexify::project_bindings::{ConversationIdentity, ProjectBindingStore};
use codexify::types::WorktreeMode;
use codexify::worktrees::{list_existing_worktrees, touch_workspace};
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn lists_and_reuses_registered_worktrees_without_changing_their_files() {
    let temp = tempfile::tempdir().unwrap();
    let projects = temp.path().join("projects");
    let source = projects.join("repo");
    let previous = temp.path().join("earlier worktree");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(
        &source,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    git(
        &source,
        &[
            "worktree",
            "add",
            "-b",
            "earlier",
            previous.to_str().unwrap(),
        ],
    );
    std::fs::write(previous.join("unfinished.txt"), "keep edits").unwrap();
    let mut config = default_config(projects);
    config.multi_project = true;
    config.worktrees.mode = WorktreeMode::Always;
    config.worktrees.root = temp.path().join("managed");
    config.memory.dir = Some(temp.path().join("metadata").display().to_string());
    let rows = list_existing_worktrees(&config, &source).await.unwrap();
    assert_eq!(rows.len(), 2);
    let previous = std::fs::canonicalize(previous).unwrap();
    let row = rows.iter().find(|row| row.path == previous).unwrap();
    assert_eq!(row.branch.as_deref(), Some("earlier"));
    assert_eq!(row.name, "earlier");
    assert!(row.last_used_at_ms.is_none());
    touch_workspace(&config, &previous).unwrap();
    let rows = list_existing_worktrees(&config, &source).await.unwrap();
    assert_eq!(rows[0].path, previous);
    assert!(rows[0].last_used_at_ms.is_some());
    let store = ProjectBindingStore::new(temp.path().join("bindings"));
    let id = ConversationIdentity::from_openai_session("reuse").unwrap();
    assert!(
        store
            .reuse_worktree(&config, &id, "repo", &temp.path().join("other"))
            .await
            .is_err()
    );
    let selected = store
        .reuse_worktree(&config, &id, "repo", &previous)
        .await
        .unwrap();
    assert_eq!(selected.project_root, previous);
    assert!(!selected.managed_worktree);
    assert_eq!(
        store.effective_config(&config, &id).unwrap().work_dir,
        previous
    );
    let restarted = ProjectBindingStore::new(temp.path().join("bindings"));
    assert_eq!(
        restarted.effective_config(&config, &id).unwrap().work_dir,
        previous
    );
    let other = ConversationIdentity::from_openai_session("another-chat").unwrap();
    restarted
        .resume_workspace(&config, &other, previous.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(previous.join("unfinished.txt")).unwrap(),
        "keep edits"
    );
    assert_eq!(
        list_existing_worktrees(&config, &source)
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(!config.worktrees.root.exists());
}

#[tokio::test]
async fn previous_managed_worktree_remains_reusable_after_a_switch_and_restart() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("projects/repo");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(
        &source,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    let mut config = default_config(temp.path().join("projects"));
    config.multi_project = true;
    config.worktrees.mode = WorktreeMode::Always;
    config.worktrees.root = temp.path().join("managed");
    config.memory.dir = Some(temp.path().join("metadata").display().to_string());
    let store = ProjectBindingStore::new(temp.path().join("bindings"));
    let identity = ConversationIdentity::from_openai_session("original").unwrap();
    let old = store
        .select_project_root(&config, &identity, "repo")
        .await
        .unwrap();
    assert!(old.managed_worktree);
    std::fs::write(old.project_root.join("unfinished.txt"), "do not lose").unwrap();
    store
        .switch_to_picker(&config, &identity, &old.project_root)
        .await
        .unwrap();
    let restarted = ProjectBindingStore::new(temp.path().join("bindings"));
    assert!(
        restarted
            .referenced_managed_project_roots(&config)
            .unwrap()
            .contains(&old.project_root)
    );
    let rows = list_existing_worktrees(&config, &source).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .any(|row| row.path == old.project_root && row.managed_worktree)
    );
    let fresh = ConversationIdentity::from_openai_session("new-conversation").unwrap();
    let reused = restarted
        .reuse_worktree(&config, &fresh, "repo", &old.project_root)
        .await
        .unwrap();
    assert!(reused.managed_worktree);
    assert_eq!(reused.project_root, old.project_root);
    assert_eq!(
        std::fs::read_to_string(reused.project_root.join("unfinished.txt")).unwrap(),
        "do not lose"
    );
    assert_eq!(
        list_existing_worktrees(&config, &source)
            .await
            .unwrap()
            .len(),
        2
    );
}
