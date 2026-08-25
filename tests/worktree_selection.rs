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
    // Keep the checkout byte-for-byte across platforms: without this, a
    // developer whose global `core.autocrlf` is on would see `tracked.txt`
    // round-trip through the worktree as CRLF and the content assertions fail.
    git(&project_root, &["config", "core.autocrlf", "false"]);
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

/// Plant a per-worktree setup script and the local git config that selects it,
/// exactly the way an untrusted source repository could. The script drops a
/// marker file into whatever worktree it runs in; `echo ... > file` behaves the
/// same under `sh`, `cmd`, and PowerShell, so the marker's mere existence is a
/// portable "the setup script executed" signal.
const SETUP_MARKER: &str = "setup-ran.txt";

fn plant_setup_script(project_root: &Path) {
    let environment = "version = 1\n\
name = \"planted\"\n\n\
[setup]\n\
script = \"echo ran > setup-ran.txt\"\n";
    fs::write(project_root.join("environment.toml"), environment).unwrap();
    git(project_root, &["add", "environment.toml"]);
    git(
        project_root,
        &[
            "-c",
            "user.email=codexify@example.invalid",
            "-c",
            "user.name=Codexify Tests",
            "commit",
            "--quiet",
            "-m",
            "add environment",
        ],
    );
    git(
        project_root,
        &[
            "config",
            "--local",
            "codex.localEnvironmentConfigPath",
            "environment.toml",
        ],
    );
}

#[tokio::test]
async fn setup_script_is_not_run_when_the_opt_in_is_disabled() {
    let root = TempDir::new().unwrap();
    let (access_root, project_root) = repository(&root);
    plant_setup_script(&project_root);

    // Default configuration leaves `allow_setup_script` off.
    let config = config(&root, access_root, WorktreeMode::Always);
    assert!(!config.worktrees.allow_setup_script);

    let selection = ProjectBindingStore::new(root.path().join("bindings"))
        .select_project_root(&config, &identity("disabled"), "demo")
        .await
        .unwrap();
    assert!(selection.managed_worktree);

    let worktree = selection.worktree_git_root.as_ref().unwrap();
    assert!(
        !worktree.join(SETUP_MARKER).exists(),
        "setup script must not run while the opt-in is disabled"
    );
}

#[tokio::test]
async fn setup_script_runs_only_when_the_opt_in_is_enabled() {
    let root = TempDir::new().unwrap();
    let (access_root, project_root) = repository(&root);
    plant_setup_script(&project_root);

    let mut config = config(&root, access_root, WorktreeMode::Always);
    config.worktrees.allow_setup_script = true;

    let selection = ProjectBindingStore::new(root.path().join("bindings"))
        .select_project_root(&config, &identity("enabled"), "demo")
        .await
        .unwrap();
    assert!(selection.managed_worktree);

    let worktree = selection.worktree_git_root.as_ref().unwrap();
    assert!(
        worktree.join(SETUP_MARKER).exists(),
        "setup script must run once the opt-in is enabled"
    );
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
    // Compare through `fs::canonicalize` on both sides: the returned root is a
    // plain path (the `\\?\` verbatim prefix is stripped so git can use it),
    // while canonicalizing the configured root re-adds that prefix on Windows.
    assert_eq!(
        fs::canonicalize(second.worktrees_root.as_ref().unwrap()).unwrap(),
        fs::canonicalize(&config.worktrees.root).unwrap()
    );
    assert_eq!(
        git(
            second.worktree_git_root.as_ref().unwrap(),
            &["rev-parse", "--is-inside-work-tree"]
        ),
        "true"
    );

    let restarted = ProjectBindingStore::new(bindings);
    let restored = restarted
        .selected_project_root(&config, &second_identity)
        .unwrap()
        .expect("the worktree binding must survive a restart");
    // Both name the same directory; canonicalize so the comparison ignores
    // Windows path-form differences (verbatim prefix, trailing separator).
    assert_eq!(
        fs::canonicalize(&restored).unwrap(),
        fs::canonicalize(&second.project_root).unwrap()
    );
}

#[tokio::test]
async fn targeted_github_urls_use_isolated_worktrees_without_moving_the_source() {
    let root = TempDir::new().unwrap();
    let upstream = root.path().join("upstream");
    let access_root = root.path().join("projects");
    let project_root = access_root.join("demo");
    fs::create_dir_all(&upstream).unwrap();
    fs::create_dir_all(&access_root).unwrap();

    git(&upstream, &["init", "--quiet"]);
    git(&upstream, &["config", "user.name", "Codexify Tests"]);
    git(
        &upstream,
        &["config", "user.email", "codexify@example.invalid"],
    );
    git(&upstream, &["checkout", "--quiet", "-b", "main"]);
    fs::write(upstream.join("base.txt"), "base\n").unwrap();
    git(&upstream, &["add", "base.txt"]);
    git(&upstream, &["commit", "--quiet", "-m", "base"]);
    let base_commit = git(&upstream, &["rev-parse", "HEAD"]);

    git(&upstream, &["checkout", "--quiet", "-b", "split_db"]);
    fs::write(upstream.join("branch.txt"), "branch\n").unwrap();
    git(&upstream, &["add", "branch.txt"]);
    git(&upstream, &["commit", "--quiet", "-m", "branch"]);
    let branch_commit = git(&upstream, &["rev-parse", "HEAD"]);
    git(
        &upstream,
        &["update-ref", "refs/pull/886/head", &branch_commit],
    );
    git(&upstream, &["checkout", "--quiet", "main"]);

    git(
        &access_root,
        &[
            "clone",
            "--quiet",
            upstream.to_str().unwrap(),
            project_root.to_str().unwrap(),
        ],
    );
    git(
        &project_root,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/acme/demo.git",
        ],
    );
    let rewrite_key = format!("url.{}.insteadOf", upstream.display());
    git(
        &project_root,
        &[
            "config",
            rewrite_key.as_str(),
            "https://github.com/acme/demo.git",
        ],
    );
    git(&project_root, &["config", "protocol.file.allow", "always"]);

    let mut auto = config(&root, access_root.clone(), WorktreeMode::Auto);
    auto.project_catalog.codex_config.enabled = false;
    let state_dir = root.path().join("target-bindings");
    let store = ProjectBindingStore::new(state_dir.clone());
    let branch_identity = identity("target-branch");
    let selection = store
        .select_project_root(
            &auto,
            &branch_identity,
            "https://github.com/acme/demo/tree/split_db",
        )
        .await
        .unwrap();

    assert!(selection.managed_worktree);
    assert_eq!(
        selection.repository_url.as_deref(),
        Some("https://github.com/acme/demo/tree/split_db")
    );
    assert_eq!(git(&project_root, &["rev-parse", "HEAD"]), base_commit);
    assert!(!project_root.join("branch.txt").exists());
    assert_eq!(
        git(&selection.project_root, &["rev-parse", "HEAD"]),
        branch_commit
    );
    assert_eq!(
        fs::read_to_string(selection.project_root.join("branch.txt")).unwrap(),
        "branch\n"
    );

    let repeated = ProjectBindingStore::new(state_dir)
        .select_project_root(
            &auto,
            &branch_identity,
            "https://github.com/ACME/DEMO/tree/split_db",
        )
        .await
        .unwrap();
    assert!(!repeated.newly_selected);
    assert_eq!(repeated.project_root, selection.project_root);

    let fetched_refs_before_switch = git(
        &project_root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/codexify/project-selection",
        ],
    );
    let switch_error = store
        .select_project_root(
            &auto,
            &branch_identity,
            "https://github.com/acme/demo/pull/886",
        )
        .await
        .unwrap_err();
    assert!(switch_error.contains("cannot switch"), "{switch_error}");
    assert_eq!(
        git(
            &project_root,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/codexify/project-selection",
            ],
        ),
        fetched_refs_before_switch
    );

    let pull_selection = store
        .select_project_root(
            &auto,
            &identity("target-pull-request"),
            "https://github.com/acme/demo/pull/886",
        )
        .await
        .unwrap();
    assert!(pull_selection.managed_worktree);
    assert_eq!(
        pull_selection.repository_url.as_deref(),
        Some("https://github.com/acme/demo/pull/886")
    );
    assert_eq!(
        git(&pull_selection.project_root, &["rev-parse", "HEAD"]),
        branch_commit
    );
    assert_eq!(git(&project_root, &["rev-parse", "HEAD"]), base_commit);

    let mut never = auto.clone();
    never.worktrees.mode = WorktreeMode::Never;
    let error = ProjectBindingStore::new(root.path().join("never-target-bindings"))
        .select_project_root(
            &never,
            &identity("target-branch-never"),
            "https://github.com/acme/demo/tree/split_db",
        )
        .await
        .unwrap_err();
    assert!(error.contains("worktree isolation is disabled"), "{error}");
    assert_eq!(git(&project_root, &["rev-parse", "HEAD"]), base_commit);
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
