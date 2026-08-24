use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use codexify::config::default_config;
use codexify::project_bindings::{ConversationIdentity, ProjectBindingStore};
use codexify::types::{AppConfig, WorktreeMode};
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("git must be installed to run these tests");
    assert!(
        output.status.success(),
        "git {} failed:\nstdout: {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn repository(root: &TempDir) -> (PathBuf, PathBuf) {
    let access_root = root.path().join("projects");
    let project_root = access_root.join("demo");
    fs::create_dir_all(&project_root).unwrap();
    fs::write(project_root.join("tracked.txt"), "tracked\n").unwrap();
    git(&project_root, &["init", "--quiet"]);
    git(&project_root, &["add", "tracked.txt"]);
    git(
        &project_root,
        &[
            "-c",
            "user.email=codexify@example.invalid",
            "-c",
            "user.name=Codexify Tests",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    );
    (access_root, project_root)
}

fn config(root: &TempDir, access_root: PathBuf, mode: WorktreeMode) -> AppConfig {
    let mut config = default_config(access_root);
    config.multi_project = true;
    config.skills.enabled = Some(false);
    config.worktrees.mode = mode;
    config.worktrees.root = root.path().join("managed-worktrees");
    config.worktrees.auto_cleanup_enabled = false;
    config
}

fn identity(name: &str) -> ConversationIdentity {
    ConversationIdentity::from_openai_session(name).unwrap()
}

#[tokio::test]
async fn auto_mode_uses_the_source_checkout_once_then_creates_a_worktree() {
    let root = TempDir::new().unwrap();
    let (access_root, project_root) = repository(&root);
    let config = config(&root, access_root, WorktreeMode::Auto);
    let bindings = root.path().join("bindings");
    let store = ProjectBindingStore::new(bindings.clone());

    let first = store
        .select_project_root(&config, &identity("first"), "demo")
        .await
        .unwrap();
    assert!(!first.managed_worktree);
    assert_eq!(first.project_root, fs::canonicalize(&project_root).unwrap());

    let second_identity = identity("second");
    let second = store
        .select_project_root(&config, &second_identity, "demo")
        .await
        .unwrap();
    assert!(second.managed_worktree);
    assert_ne!(second.project_root, first.project_root);
    assert_eq!(
        fs::read_to_string(second.project_root.join("tracked.txt")).unwrap(),
        "tracked\n"
    );
    assert_eq!(
        second.worktrees_root.as_ref().unwrap(),
        &fs::canonicalize(&config.worktrees.root).unwrap()
    );
    assert_eq!(
        git(
            second.worktree_git_root.as_ref().unwrap(),
            &["rev-parse", "--is-inside-work-tree"]
        ),
        "true"
    );

    let restarted = ProjectBindingStore::new(bindings);
    assert_eq!(
        restarted
            .selected_project_root(&config, &second_identity)
            .unwrap(),
        Some(second.project_root)
    );
}

#[tokio::test]
async fn explicit_modes_override_auto_allocation() {
    let root = TempDir::new().unwrap();
    let (access_root, project_root) = repository(&root);

    let always = config(&root, access_root.clone(), WorktreeMode::Always);
    let always_selection = ProjectBindingStore::new(root.path().join("always-bindings"))
        .select_project_root(&always, &identity("always"), "demo")
        .await
        .unwrap();
    assert!(always_selection.managed_worktree);

    let never = config(&root, access_root, WorktreeMode::Never);
    let never_store = ProjectBindingStore::new(root.path().join("never-bindings"));
    let first = never_store
        .select_project_root(&never, &identity("never-first"), "demo")
        .await
        .unwrap();
    let second = never_store
        .select_project_root(&never, &identity("never-second"), "demo")
        .await
        .unwrap();
    let canonical_project = fs::canonicalize(project_root).unwrap();
    assert!(!first.managed_worktree);
    assert!(!second.managed_worktree);
    assert_eq!(first.project_root, canonical_project);
    assert_eq!(second.project_root, canonical_project);
}

#[tokio::test]
async fn concurrent_auto_bindings_claim_one_source_checkout_and_one_worktree() {
    let root = TempDir::new().unwrap();
    let (access_root, project_root) = repository(&root);
    let config = config(&root, access_root, WorktreeMode::Auto);
    let store = ProjectBindingStore::new(root.path().join("bindings"));
    let barrier = Arc::new(tokio::sync::Barrier::new(3));

    let attempts = ["first", "second"].map(|name| {
        let barrier = barrier.clone();
        let config = config.clone();
        let store = store.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            store
                .select_project_root(&config, &identity(name), "demo")
                .await
                .unwrap()
        })
    });
    barrier.wait().await;
    let [first, second] = attempts;
    let (first, second) = tokio::join!(first, second);
    let selections = [first.unwrap(), second.unwrap()];

    assert_eq!(
        selections
            .iter()
            .filter(|selection| selection.managed_worktree)
            .count(),
        1
    );
    assert_eq!(
        selections
            .iter()
            .filter(|selection| !selection.managed_worktree)
            .count(),
        1
    );
    assert_eq!(
        selections
            .iter()
            .find(|selection| !selection.managed_worktree)
            .unwrap()
            .project_root,
        fs::canonicalize(project_root).unwrap()
    );
}
