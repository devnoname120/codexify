use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use codexify::config::default_config;
use codexify::diff::{
    DiffBaseline, DiffCheckpointManager, DiffOwner, DiffRequest, TransportDiffState,
};
use codexify::project_bindings::ConversationIdentity;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git must be installed");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn init_repo() -> TempDir {
    let repo = TempDir::new().unwrap();
    git(repo.path(), &["init", "--quiet"]);
    git(repo.path(), &["config", "user.name", "Test"]);
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    repo
}

fn commit_all(repo: &Path, message: &str) {
    git(repo, &["add", "-f", "-A"]);
    git(repo, &["commit", "--quiet", "-m", message]);
}

fn conversation(value: &str) -> DiffOwner {
    DiffOwner::conversation(&ConversationIdentity::from_openai_session(value).unwrap())
}

fn request(since: DiffBaseline, advance: bool) -> DiffRequest {
    DiffRequest {
        since,
        advance,
        include_patch: true,
    }
}

#[tokio::test]
async fn nested_project_excludes_sibling_changes_and_preserves_the_real_index() {
    let repo = init_repo();
    let app = repo.path().join("packages/app");
    let sibling = repo.path().join("packages/other");
    std::fs::create_dir_all(app.join("src")).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(app.join("src/app.txt"), "app\n").unwrap();
    std::fs::write(sibling.join("other.txt"), "other\n").unwrap();
    commit_all(repo.path(), "seed");

    std::fs::write(sibling.join("staged.txt"), "staged\n").unwrap();
    git(repo.path(), &["add", "-f", "packages/other/staged.txt"]);
    let index_before = git(repo.path(), &["diff", "--cached", "--binary"]);
    let index_path = PathBuf::from(git(repo.path(), &["rev-parse", "--git-path", "index"]));
    let index_path = if index_path.is_absolute() {
        index_path
    } else {
        repo.path().join(index_path)
    };
    let index_bytes_before = std::fs::read(&index_path).unwrap();

    let config = default_config(app.clone());
    let manager = DiffCheckpointManager::new();
    let owner = conversation("nested-project");
    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();
    let index_after = git(repo.path(), &["diff", "--cached", "--binary"]);
    let index_bytes_after = std::fs::read(&index_path).unwrap();
    assert_eq!(index_after, index_before);
    assert_eq!(index_bytes_after, index_bytes_before);

    std::fs::write(app.join("src/app.txt"), "app changed\n").unwrap();
    std::fs::write(sibling.join("other.txt"), "sibling changed\n").unwrap();
    let result = manager
        .show_diff(&config, owner, request(DiffBaseline::ProjectOpen, false))
        .await
        .unwrap();

    assert_eq!(result.scope, "packages/app");
    assert_eq!(result.summary.files, 1);
    assert_eq!(result.files[0].path, "src/app.txt");
    assert!(result.patch.contains("a/src/app.txt"));
    assert!(result.patch.contains("b/src/app.txt"));
    assert!(!result.patch.contains("packages/app"));
    assert!(!result.patch.contains("packages/other"));
    assert!(!result.patch.contains("sibling changed"));
}

#[tokio::test]
async fn last_diff_advances_while_project_open_remains_immutable() {
    let repo = init_repo();
    let file = repo.path().join("file.txt");
    std::fs::write(&file, "zero\n").unwrap();
    commit_all(repo.path(), "seed");

    let config = default_config(repo.path().to_path_buf());
    let manager = DiffCheckpointManager::new();
    let owner = conversation("incremental-diff");
    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();

    std::fs::write(&file, "one\n").unwrap();
    let first = manager
        .show_diff(
            &config,
            owner.clone(),
            request(DiffBaseline::LastDiff, true),
        )
        .await
        .unwrap();
    assert!(first.checkpoint_advanced);
    assert!(first.patch.contains("+one"));

    std::fs::write(&file, "two\n").unwrap();
    let incremental = manager
        .show_diff(
            &config,
            owner.clone(),
            request(DiffBaseline::LastDiff, false),
        )
        .await
        .unwrap();
    assert!(incremental.patch.contains("-one"));
    assert!(incremental.patch.contains("+two"));
    assert!(!incremental.patch.contains("-zero"));

    let from_open = manager
        .show_diff(&config, owner, request(DiffBaseline::ProjectOpen, false))
        .await
        .unwrap();
    assert!(from_open.patch.contains("-zero"));
    assert!(from_open.patch.contains("+two"));
}

#[tokio::test]
async fn repeating_the_same_diff_is_effect_idempotent() {
    let repo = init_repo();
    let file = repo.path().join("file.txt");
    std::fs::write(&file, "zero\n").unwrap();
    commit_all(repo.path(), "seed");

    let config = default_config(repo.path().to_path_buf());
    let manager = DiffCheckpointManager::new();
    let owner = conversation("idempotent-diff");
    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();

    std::fs::write(&file, "one\n").unwrap();
    let first = manager
        .show_diff(
            &config,
            owner.clone(),
            request(DiffBaseline::LastDiff, true),
        )
        .await
        .unwrap();
    assert!(first.checkpoint_advanced);
    let first_ref = git(
        repo.path(),
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/codexify/diff/",
        ],
    )
    .lines()
    .find(|line| line.ends_with("/last-diff"))
    .unwrap()
    .to_string();

    let repeated = manager
        .show_diff(&config, owner, request(DiffBaseline::LastDiff, true))
        .await
        .unwrap();
    assert!(repeated.checkpoint_advanced);
    assert_eq!(repeated.summary.files, 0);
    let repeated_ref = git(
        repo.path(),
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/codexify/diff/",
        ],
    )
    .lines()
    .find(|line| line.ends_with("/last-diff"))
    .unwrap()
    .to_string();
    assert_eq!(repeated_ref, first_ref);
}

#[tokio::test]
async fn conversation_checkpoints_survive_manager_replacement() {
    let repo = init_repo();
    let file = repo.path().join("file.txt");
    std::fs::write(&file, "before\n").unwrap();
    commit_all(repo.path(), "seed");
    let config = default_config(repo.path().to_path_buf());
    let identity = ConversationIdentity::from_openai_session("persistent-diff").unwrap();

    DiffCheckpointManager::new()
        .ensure_initialized(&config, DiffOwner::conversation(&identity))
        .await
        .unwrap();
    std::fs::write(&file, "after\n").unwrap();

    let result = DiffCheckpointManager::new()
        .show_diff(
            &config,
            DiffOwner::conversation(&identity),
            request(DiffBaseline::ProjectOpen, false),
        )
        .await
        .unwrap();
    assert_eq!(result.summary.files, 1);
    assert!(result.patch.contains("+after"));
}

#[tokio::test]
async fn deleting_persistent_refs_reinitializes_a_live_manager() {
    let repo = init_repo();
    let file = repo.path().join("file.txt");
    std::fs::write(&file, "before\n").unwrap();
    commit_all(repo.path(), "seed");
    let config = default_config(repo.path().to_path_buf());
    let manager = DiffCheckpointManager::new();
    let owner = conversation("live-ref-reset");

    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();
    let refs = git(
        repo.path(),
        &["for-each-ref", "--format=%(refname)", "refs/codexify/diff/"],
    );
    assert_eq!(refs.lines().count(), 2);
    for reference in refs.lines() {
        git(repo.path(), &["update-ref", "-d", reference]);
    }

    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();
    let recreated = git(
        repo.path(),
        &["for-each-ref", "--format=%(refname)", "refs/codexify/diff/"],
    );
    assert_eq!(recreated.lines().count(), 2);

    std::fs::write(&file, "after\n").unwrap();
    let result = manager
        .show_diff(&config, owner, request(DiffBaseline::ProjectOpen, false))
        .await
        .unwrap();
    assert_eq!(result.summary.files, 1);
    assert!(result.patch.contains("+after"));
}

#[tokio::test]
async fn mutation_guard_serializes_diff_for_the_same_scope() {
    let repo = init_repo();
    std::fs::write(repo.path().join("file.txt"), "before\n").unwrap();
    commit_all(repo.path(), "seed");
    let config = default_config(repo.path().to_path_buf());
    let manager = DiffCheckpointManager::new();
    let owner = conversation("serialized-diff");

    let (_, guard) = manager
        .begin_mutation(&config, owner.clone())
        .await
        .unwrap();
    let blocked = tokio::time::timeout(
        Duration::from_millis(50),
        manager.show_diff(
            &config,
            owner.clone(),
            request(DiffBaseline::ProjectOpen, false),
        ),
    )
    .await;
    assert!(blocked.is_err());

    drop(guard);
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        manager.show_diff(&config, owner, request(DiffBaseline::ProjectOpen, false)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.summary.files, 0);
}

#[tokio::test]
async fn transport_checkpoints_survive_aggressive_git_gc() {
    let repo = init_repo();
    let file = repo.path().join("file.txt");
    std::fs::write(&file, "before\n").unwrap();
    commit_all(repo.path(), "seed");
    let config = default_config(repo.path().to_path_buf());
    let manager = DiffCheckpointManager::new();
    let state = TransportDiffState::new();
    let owner = DiffOwner::transport(state.clone());

    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();
    git(repo.path(), &["reflog", "expire", "--expire=now", "--all"]);
    git(repo.path(), &["gc", "--prune=now"]);

    std::fs::write(&file, "after\n").unwrap();
    let result = manager
        .show_diff(&config, owner, request(DiffBaseline::ProjectOpen, false))
        .await
        .unwrap();
    assert_eq!(result.summary.files, 1);
    assert!(result.patch.contains("+after"));
}

#[tokio::test]
async fn conversations_and_transports_have_independent_open_checkpoints() {
    let repo = init_repo();
    let file = repo.path().join("file.txt");
    std::fs::write(&file, "zero\n").unwrap();
    commit_all(repo.path(), "seed");
    let config = default_config(repo.path().to_path_buf());
    let manager = DiffCheckpointManager::new();

    let first_conversation = conversation("first");
    manager
        .ensure_initialized(&config, first_conversation.clone())
        .await
        .unwrap();
    std::fs::write(&file, "one\n").unwrap();

    let second_conversation = manager
        .show_diff(
            &config,
            conversation("second"),
            request(DiffBaseline::ProjectOpen, false),
        )
        .await
        .unwrap();
    assert_eq!(second_conversation.summary.files, 0);

    let first_result = manager
        .show_diff(
            &config,
            first_conversation,
            request(DiffBaseline::ProjectOpen, false),
        )
        .await
        .unwrap();
    assert_eq!(first_result.summary.files, 1);

    let transport_one = DiffOwner::transport(TransportDiffState::new());
    manager
        .ensure_initialized(&config, transport_one.clone())
        .await
        .unwrap();
    std::fs::write(&file, "two\n").unwrap();
    let transport_two = manager
        .show_diff(
            &config,
            DiffOwner::transport(TransportDiffState::new()),
            request(DiffBaseline::ProjectOpen, false),
        )
        .await
        .unwrap();
    assert_eq!(transport_two.summary.files, 0);
    let transport_one = manager
        .show_diff(
            &config,
            transport_one,
            request(DiffBaseline::ProjectOpen, false),
        )
        .await
        .unwrap();
    assert_eq!(transport_one.summary.files, 1);
}

#[tokio::test]
async fn captures_renames_deletions_untracked_and_binary_files() {
    let repo = init_repo();
    std::fs::write(repo.path().join("rename-me.txt"), "same\n").unwrap();
    std::fs::write(repo.path().join("delete-me.txt"), "gone\n").unwrap();
    commit_all(repo.path(), "seed");
    let config = default_config(repo.path().to_path_buf());
    let manager = DiffCheckpointManager::new();
    let owner = conversation("change-kinds");
    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();

    std::fs::rename(
        repo.path().join("rename-me.txt"),
        repo.path().join("renamed.txt"),
    )
    .unwrap();
    std::fs::remove_file(repo.path().join("delete-me.txt")).unwrap();
    std::fs::write(repo.path().join("untracked.txt"), "new\n").unwrap();
    std::fs::write(repo.path().join("binary.bin"), [0_u8, 1, 2, 0, 4]).unwrap();

    let result = manager
        .show_diff(&config, owner, request(DiffBaseline::ProjectOpen, false))
        .await
        .unwrap();
    assert_eq!(result.summary.files, 4);
    assert_eq!(result.summary.binary_files, 1);
    assert!(result.files.iter().any(|file| file.status == "renamed"));
    assert!(result.files.iter().any(|file| file.status == "deleted"));
    assert!(result.files.iter().any(|file| file.path == "untracked.txt"));
    assert!(result.files.iter().any(|file| file.binary));
    assert!(result.patch.contains("GIT binary patch"));
}

#[tokio::test]
async fn supports_unborn_repositories() {
    let repo = init_repo();
    let file = repo.path().join("file.txt");
    std::fs::write(&file, "before\n").unwrap();
    let config = default_config(repo.path().to_path_buf());
    let manager = DiffCheckpointManager::new();
    let owner = conversation("unborn");
    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();

    std::fs::write(&file, "after\n").unwrap();
    let result = manager
        .show_diff(&config, owner, request(DiffBaseline::ProjectOpen, false))
        .await
        .unwrap();
    assert_eq!(result.summary.files, 1);
    assert!(result.patch.contains("+after"));
}

#[tokio::test]
async fn omits_instead_of_truncating_an_oversized_patch() {
    let repo = init_repo();
    let file = repo.path().join("large.txt");
    std::fs::write(&file, "before\n").unwrap();
    commit_all(repo.path(), "seed");
    let mut config = default_config(repo.path().to_path_buf());
    config.diff.max_patch_bytes = 128;
    let manager = DiffCheckpointManager::new();
    let owner = conversation("patch-budget");
    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();

    std::fs::write(&file, format!("{}\n", "after".repeat(2_000))).unwrap();
    let result = manager
        .show_diff(&config, owner, request(DiffBaseline::ProjectOpen, false))
        .await
        .unwrap();
    assert!(!result.patch_included);
    assert!(result.patch.is_empty());
    assert!(
        result
            .patch_omitted_reason
            .as_deref()
            .unwrap()
            .contains("maxPatchBytes")
    );
}

#[tokio::test]
async fn default_widget_budget_includes_ten_thousand_changed_code_lines() {
    fn source(prefix: char) -> String {
        let padding = "x".repeat(270);
        (0..5_000)
            .map(|index| format!("const VALUE_{index:04}: &str = \"{prefix}{padding}\";\n"))
            .collect()
    }

    let repo = init_repo();
    let file = repo.path().join("large.rs");
    std::fs::write(&file, source('a')).unwrap();
    commit_all(repo.path(), "seed");
    let config = default_config(repo.path().to_path_buf());
    assert_eq!(config.diff.max_patch_bytes, 4 * 1024 * 1024);
    let manager = DiffCheckpointManager::new();
    let owner = conversation("ten-thousand-lines");
    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();

    std::fs::write(&file, source('b')).unwrap();
    let result = manager
        .show_diff(&config, owner, request(DiffBaseline::ProjectOpen, false))
        .await
        .unwrap();

    assert_eq!(result.summary.additions + result.summary.deletions, 10_000);
    assert!(result.patch_included);
    assert!(result.patch_bytes.unwrap() < config.diff.max_patch_bytes);
    assert!(result.patch.contains("const VALUE_4999"));
}

#[tokio::test]
async fn non_git_project_reports_a_clear_error() {
    let root = TempDir::new().unwrap();
    let config = default_config(PathBuf::from(root.path()));
    let error = DiffCheckpointManager::new()
        .show_diff(
            &config,
            conversation("not-git"),
            request(DiffBaseline::ProjectOpen, false),
        )
        .await
        .unwrap_err();
    assert!(error.contains("requires a Git worktree"), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn captures_executable_and_symlink_changes() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let repo = init_repo();
    let script = repo.path().join("script.sh");
    let link = repo.path().join("current");
    std::fs::write(&script, "#!/bin/sh\necho ok\n").unwrap();
    std::fs::write(repo.path().join("first.txt"), "first\n").unwrap();
    std::fs::write(repo.path().join("second.txt"), "second\n").unwrap();
    symlink("first.txt", &link).unwrap();
    commit_all(repo.path(), "seed");

    let config = default_config(repo.path().to_path_buf());
    let manager = DiffCheckpointManager::new();
    let owner = conversation("modes-and-links");
    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();

    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();
    std::fs::remove_file(&link).unwrap();
    symlink("second.txt", &link).unwrap();

    let result = manager
        .show_diff(&config, owner, request(DiffBaseline::ProjectOpen, false))
        .await
        .unwrap();
    assert_eq!(result.summary.files, 2);
    assert!(result.files.iter().any(|file| file.path == "script.sh"));
    assert!(result.files.iter().any(|file| file.path == "current"));
    assert!(result.patch.contains("old mode 100644"));
    assert!(result.patch.contains("new mode 100755"));
    assert!(result.patch.contains("-first.txt"));
    assert!(result.patch.contains("+second.txt"));
}

#[tokio::test]
async fn concurrent_advancement_uses_compare_and_swap() {
    let repo = init_repo();
    let file = repo.path().join("file.txt");
    std::fs::write(&file, "before\n").unwrap();
    commit_all(repo.path(), "seed");

    let config = default_config(repo.path().to_path_buf());
    let identity = ConversationIdentity::from_openai_session("concurrent-diff").unwrap();
    DiffCheckpointManager::new()
        .ensure_initialized(&config, DiffOwner::conversation(&identity))
        .await
        .unwrap();
    std::fs::write(&file, "after\n").unwrap();

    let first = DiffCheckpointManager::new();
    let second = DiffCheckpointManager::new();
    let (left, right) = tokio::join!(
        first.show_diff(
            &config,
            DiffOwner::conversation(&identity),
            request(DiffBaseline::LastDiff, true),
        ),
        second.show_diff(
            &config,
            DiffOwner::conversation(&identity),
            request(DiffBaseline::LastDiff, true),
        )
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_eq!(
        usize::from(left.checkpoint_advanced) + usize::from(right.checkpoint_advanced),
        1
    );
    let conflicted = if left.checkpoint_advanced {
        right
    } else {
        left
    };
    assert!(
        conflicted
            .warnings
            .iter()
            .any(|warning| warning.contains("changed concurrently"))
    );
}

#[tokio::test]
async fn non_git_initialization_is_explicitly_unavailable() {
    let root = TempDir::new().unwrap();
    let config = default_config(root.path().to_path_buf());
    let availability = DiffCheckpointManager::new()
        .ensure_initialized(&config, conversation("not-git-initialization"))
        .await
        .unwrap();
    assert!(matches!(
        availability,
        codexify::diff::DiffAvailability::Unavailable(_)
    ));
}

#[tokio::test]
async fn malformed_git_configuration_is_a_checkpoint_error() {
    let repo = init_repo();
    std::fs::write(repo.path().join(".git/config"), "[broken\n").unwrap();
    let config = default_config(repo.path().to_path_buf());

    let error = DiffCheckpointManager::new()
        .ensure_initialized(&config, conversation("malformed-git-config"))
        .await
        .unwrap_err();
    assert!(error.contains("bad config"), "{error}");
}

#[tokio::test]
async fn git_snapshot_failures_are_not_treated_as_non_git_projects() {
    let repo = init_repo();
    std::fs::write(repo.path().join("broken.txt"), "before\n").unwrap();
    commit_all(repo.path(), "seed");
    std::fs::write(
        repo.path().join(".gitattributes"),
        "broken.txt filter=diff-test-failure\n",
    )
    .unwrap();
    git(
        repo.path(),
        &["config", "filter.diff-test-failure.clean", "false"],
    );
    git(
        repo.path(),
        &["config", "filter.diff-test-failure.required", "true"],
    );

    let config = default_config(repo.path().to_path_buf());
    let error = DiffCheckpointManager::new()
        .ensure_initialized(&config, conversation("snapshot-failure"))
        .await
        .unwrap_err();
    assert!(error.contains("git add failed"), "{error}");
}
