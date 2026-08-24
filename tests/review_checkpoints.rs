use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use codexify::config::default_config;
use codexify::project_bindings::ConversationIdentity;
use codexify::review::{
    ReviewBaseline, ReviewCheckpointManager, ReviewOwner, ReviewRequest, TransportReviewState,
};
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

fn conversation(value: &str) -> ReviewOwner {
    ReviewOwner::conversation(&ConversationIdentity::from_openai_session(value).unwrap())
}

fn request(since: ReviewBaseline, advance: bool) -> ReviewRequest {
    ReviewRequest {
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
    let manager = ReviewCheckpointManager::new();
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
        .show_changes(&config, owner, request(ReviewBaseline::ProjectOpen, false))
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
async fn last_review_advances_while_project_open_remains_immutable() {
    let repo = init_repo();
    let file = repo.path().join("file.txt");
    std::fs::write(&file, "zero\n").unwrap();
    commit_all(repo.path(), "seed");

    let config = default_config(repo.path().to_path_buf());
    let manager = ReviewCheckpointManager::new();
    let owner = conversation("incremental-review");
    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();

    std::fs::write(&file, "one\n").unwrap();
    let first = manager
        .show_changes(
            &config,
            owner.clone(),
            request(ReviewBaseline::LastReview, true),
        )
        .await
        .unwrap();
    assert!(first.checkpoint_advanced);
    assert!(first.patch.contains("+one"));

    std::fs::write(&file, "two\n").unwrap();
    let incremental = manager
        .show_changes(
            &config,
            owner.clone(),
            request(ReviewBaseline::LastReview, false),
        )
        .await
        .unwrap();
    assert!(incremental.patch.contains("-one"));
    assert!(incremental.patch.contains("+two"));
    assert!(!incremental.patch.contains("-zero"));

    let from_open = manager
        .show_changes(&config, owner, request(ReviewBaseline::ProjectOpen, false))
        .await
        .unwrap();
    assert!(from_open.patch.contains("-zero"));
    assert!(from_open.patch.contains("+two"));
}

#[tokio::test]
async fn conversation_checkpoints_survive_manager_replacement() {
    let repo = init_repo();
    let file = repo.path().join("file.txt");
    std::fs::write(&file, "before\n").unwrap();
    commit_all(repo.path(), "seed");
    let config = default_config(repo.path().to_path_buf());
    let identity = ConversationIdentity::from_openai_session("persistent-review").unwrap();

    ReviewCheckpointManager::new()
        .ensure_initialized(&config, ReviewOwner::conversation(&identity))
        .await
        .unwrap();
    std::fs::write(&file, "after\n").unwrap();

    let result = ReviewCheckpointManager::new()
        .show_changes(
            &config,
            ReviewOwner::conversation(&identity),
            request(ReviewBaseline::ProjectOpen, false),
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
    let manager = ReviewCheckpointManager::new();
    let owner = conversation("live-ref-reset");

    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();
    let refs = git(
        repo.path(),
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/codexify/review/",
        ],
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
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/codexify/review/",
        ],
    );
    assert_eq!(recreated.lines().count(), 2);

    std::fs::write(&file, "after\n").unwrap();
    let result = manager
        .show_changes(&config, owner, request(ReviewBaseline::ProjectOpen, false))
        .await
        .unwrap();
    assert_eq!(result.summary.files, 1);
    assert!(result.patch.contains("+after"));
}

#[tokio::test]
async fn mutation_guard_serializes_review_for_the_same_scope() {
    let repo = init_repo();
    std::fs::write(repo.path().join("file.txt"), "before\n").unwrap();
    commit_all(repo.path(), "seed");
    let config = default_config(repo.path().to_path_buf());
    let manager = ReviewCheckpointManager::new();
    let owner = conversation("serialized-review");

    let (_, guard) = manager
        .begin_mutation(&config, owner.clone())
        .await
        .unwrap();
    let blocked = tokio::time::timeout(
        Duration::from_millis(50),
        manager.show_changes(
            &config,
            owner.clone(),
            request(ReviewBaseline::ProjectOpen, false),
        ),
    )
    .await;
    assert!(blocked.is_err());

    drop(guard);
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        manager.show_changes(&config, owner, request(ReviewBaseline::ProjectOpen, false)),
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
    let manager = ReviewCheckpointManager::new();
    let state = TransportReviewState::new();
    let owner = ReviewOwner::transport(state.clone());

    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();
    git(repo.path(), &["reflog", "expire", "--expire=now", "--all"]);
    git(repo.path(), &["gc", "--prune=now"]);

    std::fs::write(&file, "after\n").unwrap();
    let result = manager
        .show_changes(&config, owner, request(ReviewBaseline::ProjectOpen, false))
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
    let manager = ReviewCheckpointManager::new();

    let first_conversation = conversation("first");
    manager
        .ensure_initialized(&config, first_conversation.clone())
        .await
        .unwrap();
    std::fs::write(&file, "one\n").unwrap();

    let second_conversation = manager
        .show_changes(
            &config,
            conversation("second"),
            request(ReviewBaseline::ProjectOpen, false),
        )
        .await
        .unwrap();
    assert_eq!(second_conversation.summary.files, 0);

    let first_result = manager
        .show_changes(
            &config,
            first_conversation,
            request(ReviewBaseline::ProjectOpen, false),
        )
        .await
        .unwrap();
    assert_eq!(first_result.summary.files, 1);

    let transport_one = ReviewOwner::transport(TransportReviewState::new());
    manager
        .ensure_initialized(&config, transport_one.clone())
        .await
        .unwrap();
    std::fs::write(&file, "two\n").unwrap();
    let transport_two = manager
        .show_changes(
            &config,
            ReviewOwner::transport(TransportReviewState::new()),
            request(ReviewBaseline::ProjectOpen, false),
        )
        .await
        .unwrap();
    assert_eq!(transport_two.summary.files, 0);
    let transport_one = manager
        .show_changes(
            &config,
            transport_one,
            request(ReviewBaseline::ProjectOpen, false),
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
    let manager = ReviewCheckpointManager::new();
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
        .show_changes(&config, owner, request(ReviewBaseline::ProjectOpen, false))
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
    let manager = ReviewCheckpointManager::new();
    let owner = conversation("unborn");
    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();

    std::fs::write(&file, "after\n").unwrap();
    let result = manager
        .show_changes(&config, owner, request(ReviewBaseline::ProjectOpen, false))
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
    config.review.max_patch_bytes = 128;
    let manager = ReviewCheckpointManager::new();
    let owner = conversation("patch-budget");
    manager
        .ensure_initialized(&config, owner.clone())
        .await
        .unwrap();

    std::fs::write(&file, format!("{}\n", "after".repeat(2_000))).unwrap();
    let result = manager
        .show_changes(&config, owner, request(ReviewBaseline::ProjectOpen, false))
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
async fn non_git_project_reports_a_clear_error() {
    let root = TempDir::new().unwrap();
    let config = default_config(PathBuf::from(root.path()));
    let error = ReviewCheckpointManager::new()
        .show_changes(
            &config,
            conversation("not-git"),
            request(ReviewBaseline::ProjectOpen, false),
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
    let manager = ReviewCheckpointManager::new();
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
        .show_changes(&config, owner, request(ReviewBaseline::ProjectOpen, false))
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
    let identity = ConversationIdentity::from_openai_session("concurrent-review").unwrap();
    ReviewCheckpointManager::new()
        .ensure_initialized(&config, ReviewOwner::conversation(&identity))
        .await
        .unwrap();
    std::fs::write(&file, "after\n").unwrap();

    let first = ReviewCheckpointManager::new();
    let second = ReviewCheckpointManager::new();
    let (left, right) = tokio::join!(
        first.show_changes(
            &config,
            ReviewOwner::conversation(&identity),
            request(ReviewBaseline::LastReview, true),
        ),
        second.show_changes(
            &config,
            ReviewOwner::conversation(&identity),
            request(ReviewBaseline::LastReview, true),
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
    let availability = ReviewCheckpointManager::new()
        .ensure_initialized(&config, conversation("not-git-initialization"))
        .await
        .unwrap();
    assert!(matches!(
        availability,
        codexify::review::ReviewAvailability::Unavailable(_)
    ));
}

#[tokio::test]
async fn malformed_git_configuration_is_a_checkpoint_error() {
    let repo = init_repo();
    std::fs::write(repo.path().join(".git/config"), "[broken\n").unwrap();
    let config = default_config(repo.path().to_path_buf());

    let error = ReviewCheckpointManager::new()
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
        "broken.txt filter=review-test-failure\n",
    )
    .unwrap();
    git(
        repo.path(),
        &["config", "filter.review-test-failure.clean", "false"],
    );
    git(
        repo.path(),
        &["config", "filter.review-test-failure.required", "true"],
    );

    let config = default_config(repo.path().to_path_buf());
    let error = ReviewCheckpointManager::new()
        .ensure_initialized(&config, conversation("snapshot-failure"))
        .await
        .unwrap_err();
    assert!(error.contains("git add failed"), "{error}");
}
