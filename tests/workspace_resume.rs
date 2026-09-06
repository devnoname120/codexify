use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use codexify::config::default_config;
use codexify::exec_sessions::SessionState;
use codexify::project_bindings::{ConversationIdentity, ProjectBindingStore, WorkspaceSelection};
use codexify::tool::Tool;
use codexify::tools::set_project_root::SetProjectRoot;
use codexify::types::{AppConfig, WorktreeMode};
use serde_json::json;
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn fixture() -> (TempDir, AppConfig, ProjectBindingStore, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("projects/demo");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("tracked.txt"), "original\n").unwrap();
    git(&project, &["init", "--quiet"]);
    git(&project, &["config", "core.autocrlf", "false"]);
    git(&project, &["add", "tracked.txt"]);
    git(
        &project,
        &[
            "-c",
            "user.name=Tests",
            "-c",
            "user.email=tests@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    );
    let mut config = default_config(root.path().join("projects"));
    config.multi_project = true;
    config.worktrees.root = root.path().join("worktrees");
    config.worktrees.auto_cleanup_enabled = false;
    config.memory.dir = Some(root.path().join("memory").to_string_lossy().into_owned());
    let store = ProjectBindingStore::new(root.path().join("bindings"));
    (root, config, store, project)
}

fn identity(value: &str) -> ConversationIdentity {
    ConversationIdentity::from_openai_session(value).unwrap()
}

fn records(root: &Path, extension: &str) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry.path().extension().is_some_and(|ext| ext == extension)
        })
        .map(|entry| entry.into_path())
        .collect()
}

#[tokio::test]
async fn resume_keeps_dirty_checkout_and_index_under_every_worktree_policy() {
    for managed in [false, true] {
        let (_root, mut config, store, project) = fixture();
        config.worktrees.mode = if managed {
            WorktreeMode::Always
        } else {
            WorktreeMode::Never
        };
        let original = store
            .select_project_root(&config, &identity("original"), "demo")
            .await
            .unwrap();
        let effective = store
            .effective_config(&config, &identity("original"))
            .unwrap();
        assert!(
            codexify::memory::create_note(
                &effective,
                "continuation-handoff",
                "Finish the pending task",
                "2026-09-07T12:00:00Z"
            )
            .ok
        );
        fs::write(original.project_root.join("tracked.txt"), "staged\n").unwrap();
        git(&original.project_root, &["add", "tracked.txt"]);
        fs::write(original.project_root.join("tracked.txt"), "unstaged\n").unwrap();
        fs::write(original.project_root.join("untracked.txt"), "untracked\n").unwrap();
        let head = git(&original.project_root, &["rev-parse", "HEAD"]);
        let status = git(&original.project_root, &["status", "--porcelain"]);
        let index = git(&original.project_root, &["show", ":tracked.txt"]);
        let worktrees = git(&project, &["worktree", "list", "--porcelain"]);
        let original_record = records(store.base_dir(), "json").remove(0);
        for (number, mode) in [
            WorktreeMode::Auto,
            WorktreeMode::Always,
            WorktreeMode::Never,
        ]
        .into_iter()
        .enumerate()
        {
            config.worktrees.mode = mode;
            let new_identity = identity(&format!("resumed-{number}"));
            let WorkspaceSelection::Project(resumed) = store
                .resume_workspace(
                    &config,
                    &new_identity,
                    original.project_root.to_str().unwrap(),
                )
                .await
                .unwrap()
            else {
                panic!("expected project");
            };
            assert_eq!(resumed.project_root, original.project_root);
            assert_eq!(resumed.source_project_root, original.source_project_root);
            assert_eq!(resumed.managed_worktree, managed);
            assert_eq!(resumed.worktree_git_root, original.worktree_git_root);
            assert!(!resumed.cloned);
            assert!(resumed.newly_selected);
            assert_eq!(
                git(&project, &["worktree", "list", "--porcelain"]),
                worktrees
            );
            assert_eq!(git(&resumed.project_root, &["rev-parse", "HEAD"]), head);
            assert_eq!(
                git(&resumed.project_root, &["status", "--porcelain"]),
                status
            );
            assert_eq!(git(&resumed.project_root, &["show", ":tracked.txt"]), index);
            assert_eq!(
                fs::read_to_string(resumed.project_root.join("untracked.txt")).unwrap(),
                "untracked\n"
            );
            let restarted = ProjectBindingStore::new(store.base_dir().into());
            assert_eq!(
                restarted
                    .selected_project_root(&config, &new_identity)
                    .unwrap(),
                Some(original.project_root.clone())
            );
            let before = restarted
                .effective_config(&config, &identity("original"))
                .unwrap();
            let after = restarted.effective_config(&config, &new_identity).unwrap();
            assert_eq!(
                codexify::memory::load_memory(&after).notes["continuation-handoff"].value,
                "Finish the pending task"
            );
            assert_eq!(
                codexify::memory::memory_dir(&before),
                codexify::memory::memory_dir(&after)
            );
            let WorkspaceSelection::Project(repeated) = restarted
                .resume_workspace(
                    &config,
                    &new_identity,
                    original.project_root.to_str().unwrap(),
                )
                .await
                .unwrap()
            else {
                panic!("expected project");
            };
            assert!(!repeated.newly_selected);
            assert!(
                restarted
                    .resume_workspace(&config, &new_identity, config.work_dir.to_str().unwrap())
                    .await
                    .is_err()
            );
        }
        fs::remove_file(original_record).unwrap();
        if managed {
            assert!(
                store
                    .referenced_managed_project_roots(&config)
                    .unwrap()
                    .contains(&original.project_root)
            );
            config.worktrees.auto_cleanup_enabled = true;
            config.worktrees.keep_count = 0;
            let referenced = store.referenced_managed_project_roots(&config).unwrap();
            assert!(
                codexify::worktrees::cleanup_managed_worktrees(&config, &referenced)
                    .await
                    .is_empty()
            );
            assert!(original.project_root.is_dir());
        }
    }
}

#[tokio::test]
async fn scratch_resume_survives_multiple_handoffs_and_restart() {
    let (_root, config, store, _) = fixture();
    let original = store
        .select_without_project(&config, &identity("first"))
        .await
        .unwrap();
    fs::write(original.scratch_root.join("task.txt"), "in progress").unwrap();
    let original_record = records(store.base_dir(), "no-project").remove(0);
    for name in ["second", "third"] {
        let restarted = ProjectBindingStore::new(store.base_dir().into());
        let WorkspaceSelection::WithoutProject(resumed) = restarted
            .resume_workspace(
                &config,
                &identity(name),
                original.scratch_root.to_str().unwrap(),
            )
            .await
            .unwrap()
        else {
            panic!("expected scratch");
        };
        assert!(resumed.newly_selected);
        assert_eq!(resumed.scratch_root, original.scratch_root);
        assert_eq!(
            fs::read_to_string(resumed.scratch_root.join("task.txt")).unwrap(),
            "in progress"
        );
        assert_eq!(
            restarted
                .selected_project_root(&config, &identity(name))
                .unwrap(),
            Some(original.scratch_root.clone())
        );
        let repeated = restarted
            .select_without_project(&config, &identity(name))
            .await
            .unwrap();
        assert!(!repeated.newly_selected);
        assert_eq!(repeated.scratch_root, original.scratch_root);
        if name == "second" {
            fs::remove_file(&original_record).unwrap();
        }
    }
    let workspaces = walkdir::WalkDir::new(store.base_dir().join("scratch"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() == "task.txt")
        .count();
    assert_eq!(workspaces, 1);
}

#[tokio::test]
async fn resume_preserves_managed_metadata_when_a_direct_alias_also_exists() {
    let (_root, mut config, store, _) = fixture();
    config.worktrees.root = config.work_dir.join("worktrees");
    config.worktrees.mode = WorktreeMode::Always;
    let original = store
        .select_project_root(&config, &identity("original"), "demo")
        .await
        .unwrap();
    config.worktrees.mode = WorktreeMode::Never;
    let alias = store
        .select_project_root(
            &config,
            &identity("alias"),
            original.project_root.to_str().unwrap(),
        )
        .await
        .unwrap();
    assert!(!alias.managed_worktree);
    let WorkspaceSelection::Project(resumed) = store
        .resume_workspace(
            &config,
            &identity("resumed"),
            original.project_root.to_str().unwrap(),
        )
        .await
        .unwrap()
    else {
        panic!("expected project");
    };
    assert!(resumed.managed_worktree);
    assert_eq!(resumed.source_project_root, original.source_project_root);
    assert_eq!(resumed.worktree_git_root, original.worktree_git_root);
}

#[tokio::test]
async fn concurrent_resumes_cannot_rebind_one_conversation() {
    let (_root, mut config, store, _) = fixture();
    config.worktrees.mode = WorktreeMode::Never;
    fs::create_dir(config.work_dir.join("other")).unwrap();
    let first = store
        .select_project_root(&config, &identity("first"), "demo")
        .await
        .unwrap();
    let second = store
        .select_project_root(&config, &identity("second"), "other")
        .await
        .unwrap();
    let destination = identity("destination");
    let (a, b) = tokio::join!(
        store.resume_workspace(&config, &destination, first.project_root.to_str().unwrap()),
        store.resume_workspace(&config, &destination, second.project_root.to_str().unwrap()),
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let expected = if a.is_ok() {
        first.project_root
    } else {
        second.project_root
    };
    assert_eq!(
        store.selected_project_root(&config, &destination).unwrap(),
        Some(expected)
    );
}

#[tokio::test]
async fn scratch_resume_rejects_malformed_namespace_keys() {
    let (_root, config, store, _) = fixture();
    let original = store
        .select_without_project(&config, &identity("old"))
        .await
        .unwrap();
    let marker = records(store.base_dir(), "no-project").remove(0);
    let initial: serde_json::Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
    for key in ["../outside", "", "/absolute", "not-a-namespace"] {
        let mut value = initial.clone();
        value["workspaceKey"] = json!(key);
        fs::write(&marker, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            store
                .selected_project_root(&config, &identity("old"))
                .is_err()
        );
        assert!(
            store
                .resume_workspace(
                    &config,
                    &identity("new"),
                    original.scratch_root.to_str().unwrap()
                )
                .await
                .is_err()
        );
        assert!(
            store
                .selected_project_root(&config, &identity("new"))
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn resume_rejects_unrecorded_deleted_relative_and_wrong_scope_paths() {
    let (root, mut config, store, _) = fixture();
    let outside = root.path().join("unrelated");
    fs::create_dir(&outside).unwrap();
    let original = store
        .select_project_root(&config, &identity("old"), "demo")
        .await
        .unwrap();
    let nested = original.project_root.join("nested");
    fs::create_dir(&nested).unwrap();
    for input in [
        "demo",
        "https://example.invalid/repo.git",
        outside.to_str().unwrap(),
        nested.to_str().unwrap(),
    ] {
        assert!(
            store
                .resume_workspace(&config, &identity("new"), input)
                .await
                .is_err()
        );
        assert!(
            store
                .selected_project_root(&config, &identity("new"))
                .unwrap()
                .is_none()
        );
    }
    let before = records(store.base_dir(), "json").len();
    config.work_dir = outside;
    assert!(
        store
            .resume_workspace(
                &config,
                &identity("new"),
                original.project_root.to_str().unwrap()
            )
            .await
            .is_err()
    );
    assert_eq!(records(store.base_dir(), "json").len(), before);
    config.work_dir = root.path().join("projects");
    fs::remove_dir_all(&original.project_root).unwrap();
    assert!(
        store
            .resume_workspace(
                &config,
                &identity("new"),
                original.project_root.to_str().unwrap()
            )
            .await
            .is_err()
    );
    assert!(!original.project_root.exists());
}

#[tokio::test]
async fn resume_rejects_tampered_worktree_metadata() {
    let (_root, mut config, store, _) = fixture();
    config.worktrees.mode = WorktreeMode::Always;
    let original = store
        .select_project_root(&config, &identity("old"), "demo")
        .await
        .unwrap();
    let metadata = codexify::worktrees::metadata_path_for_worktree(
        original.worktree_git_root.as_ref().unwrap(),
    )
    .unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&metadata).unwrap()).unwrap();
    value["projectRoot"] = json!(config.work_dir);
    fs::write(metadata, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(
        store
            .resume_workspace(
                &config,
                &identity("new"),
                original.project_root.to_str().unwrap()
            )
            .await
            .is_err()
    );
    assert!(
        store
            .selected_project_root(&config, &identity("new"))
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn transport_only_clients_fail_instead_of_allocating_a_workspace() {
    let (_root, config, _store, project) = fixture();
    let session = SessionState::new();
    let result = SetProjectRoot
        .call(json!({"resumePath": project}), &config, &session)
        .await;
    assert!(result.is_error);
    assert!(result.joined_text().contains("stable ChatGPT conversation"));
    assert!(!config.worktrees.root.exists());
}

#[test]
fn resume_schema_is_exclusive_and_backwards_compatible() {
    let schema = SetProjectRoot.input_schema();
    let validator = jsonschema::options().build(&schema).unwrap();
    for valid in [
        json!({"resumePath":"/worktrees/existing"}),
        json!({"path":"demo"}),
        json!({"path":"demo","createWorktree":false}),
        json!({"withoutProject":true}),
    ] {
        assert!(validator.is_valid(&valid), "{valid}");
    }
    for invalid in [
        json!({"resumePath":""}),
        json!({"resumePath":null}),
        json!({"resumePath":"/worktree","path":"demo"}),
        json!({"resumePath":"/worktree","createWorktree":false}),
        json!({"resumePath":"/worktree","withoutProject":true}),
    ] {
        assert!(!validator.is_valid(&invalid), "{invalid}");
    }
}
