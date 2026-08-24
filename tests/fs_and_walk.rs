//! Integration tests for the filesystem-walking tools and the shared ignore /
//! output-budget machinery. Ports these Bun/TS suites:
//!   - src/__tests__/ignore.test.ts
//!   - src/tools/__tests__/filesystem.test.ts
//!   - src/tools/__tests__/output-caps.test.ts
//!   - src/tools/__tests__/read-file.test.ts
//!   - src/tools/__tests__/write-file.test.ts
//!
//! Adaptations from the TS originals are noted inline where the Rust port
//! intentionally diverges (e.g. `fastGlobIgnore` has no Rust counterpart, and
//! `to_rel_posix` strips lexically rather than normalizing first).

use std::path::Path;

use serde_json::json;
use tempfile::TempDir;

use codexify::config::default_config;
use codexify::exec_sessions::SessionState;
use codexify::ignore_rules::{DEFAULT_IGNORE, build_ignore, to_rel_posix};
use codexify::tool::Tool;
use codexify::tools::glob::Glob;
use codexify::tools::grep::Grep;
use codexify::tools::list_directory::ListDirectory;
use codexify::tools::read_file::ReadFile;
use codexify::tools::tree::Tree;
use codexify::tools::write_file::WriteFile;
use codexify::types::{AppConfig, ToolResult};

// --- helpers -----------------------------------------------------------

/// Write a file at `rel` under `root`, creating parent directories.
fn write(root: &Path, rel: &str, content: &str) {
    let abs = root.join(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(abs, content).unwrap();
}

/// A config whose ignore behaviour matches the TS `makeConfig` in ignore.test.ts:
/// no tree-level ignore patterns, so only the default set + gitignore apply.
fn ignore_config(root: &Path) -> AppConfig {
    let mut config = default_config(root.to_path_buf());
    config.tree.ignore = Vec::new();
    config
}

async fn run_result(tool: &dyn Tool, args: serde_json::Value, config: &AppConfig) -> ToolResult {
    let session = SessionState::new();
    tool.call(args, config, &session).await
}

async fn run_text(tool: &dyn Tool, args: serde_json::Value, config: &AppConfig) -> String {
    run_result(tool, args, config).await.joined_text()
}

// --- toRelPosix --------------------------------------------------------

#[test]
fn to_rel_posix_returns_none_for_workdir_and_outside() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    // The work directory itself.
    assert_eq!(to_rel_posix(root, root), None);
    // A path outside the work directory. The TS passes join(root,"..","elsewhere")
    // and relies on normalization; the Rust `to_rel_posix` strips lexically, so we
    // use a genuinely-outside absolute path (a sibling of root) to exercise the
    // same "outside -> None" intent.
    let outside = root.parent().unwrap().join("codex-elsewhere-xyz");
    assert_eq!(to_rel_posix(&outside, root), None);
}

#[test]
fn to_rel_posix_returns_forward_slash_child() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let child = root.join("src").join("a.ts");
    assert_eq!(to_rel_posix(&child, root).as_deref(), Some("src/a.ts"));
}

// --- buildIgnore -------------------------------------------------------

#[test]
fn build_ignore_ignores_default_heavy_dirs_by_name_and_content() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let ig = build_ignore(&ignore_config(root));

    for name in DEFAULT_IGNORE {
        // The directory itself (is_dir = true).
        assert!(
            ig.is_ignored(&root.join(name), true),
            "expected {name} dir to be ignored"
        );
        // A file deep inside it (is_dir = false).
        let deep = root.join(name).join("deep").join("file.js");
        assert!(
            ig.is_ignored(&deep, false),
            "expected content under {name} to be ignored"
        );
    }

    assert!(!ig.is_ignored(&root.join("src").join("index.ts"), false));
}

#[test]
fn build_ignore_reads_workdir_gitignore() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "secret.txt\nlogs/\n");
    let ig = build_ignore(&ignore_config(root));

    assert!(ig.is_ignored(&root.join("secret.txt"), false));
    assert!(ig.is_ignored(&root.join("logs").join("today.log"), false));
    assert!(!ig.is_ignored(&root.join("keep.txt"), false));
}

#[test]
fn build_ignore_reads_git_info_exclude() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, ".git/info/exclude", "local-only.tmp\n");
    let ig = build_ignore(&ignore_config(root));

    assert!(ig.is_ignored(&root.join("local-only.tmp"), false));
}

#[test]
fn build_ignore_use_default_patterns_false_stops_skipping_builtins() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mut config = ignore_config(root);
    config.ignore.use_default_patterns = Some(false);
    let ig = build_ignore(&config);

    assert!(!ig.is_ignored(&root.join("dist").join("app.js"), false));
}

#[test]
fn build_ignore_use_gitignore_false_stops_reading_gitignore() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "secret.txt\n");
    let mut config = ignore_config(root);
    config.ignore.use_gitignore = Some(false);
    let ig = build_ignore(&config);

    assert!(!ig.is_ignored(&root.join("secret.txt"), false));
}

#[test]
fn build_ignore_custom_patterns_applied_on_top() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mut config = ignore_config(root);
    config.ignore.custom_patterns = Some(vec!["*.snap".to_string()]);
    let ig = build_ignore(&config);

    assert!(ig.is_ignored(&root.join("a.snap"), false));
}

// --- shouldPrune -------------------------------------------------------

#[test]
fn should_prune_always_prunes_node_modules_and_git_even_with_defaults_off() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mut config = ignore_config(root);
    config.ignore.use_default_patterns = Some(false);
    let ig = build_ignore(&config);

    assert!(ig.should_prune("node_modules", &root.join("node_modules")));
    assert!(ig.should_prune(".git", &root.join(".git")));
    assert!(!ig.should_prune("dist", &root.join("dist")));
}

// NOTE: The TS `fastGlobIgnore` tests are intentionally omitted. The Rust port
// (ignore_rules.rs) drives every walking tool through a single `IgnoreMatcher`
// built on the `ignore` crate and has no `fastGlobIgnore` glob-list equivalent;
// there is nothing analogous to assert against.

// --- glob / grep / tree / list_directory ignore behaviour --------------

#[tokio::test]
async fn glob_does_not_descend_node_modules_and_respects_gitignore() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, "src/a.ts", "x");
    write(root, "node_modules/pkg/index.ts", "x");
    write(root, "secret.ts", "x");
    write(root, ".gitignore", "secret.ts\n");

    let out = run_text(&Glob, json!({ "pattern": "**/*.ts" }), &ignore_config(root)).await;
    assert!(out.contains("src/a.ts"), "{out}");
    assert!(!out.contains("node_modules"), "{out}");
    assert!(!out.contains("secret.ts"), "{out}");
}

#[tokio::test]
async fn grep_skips_node_modules_and_gitignored_files() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, "src/a.ts", "needle here");
    write(root, "node_modules/pkg/b.ts", "needle here");
    write(root, "ignored.ts", "needle here");
    write(root, ".gitignore", "ignored.ts\n");

    let out = run_text(&Grep, json!({ "pattern": "needle" }), &ignore_config(root)).await;
    assert!(out.contains("src/a.ts"), "{out}");
    assert!(!out.contains("node_modules"), "{out}");
    assert!(!out.contains("ignored.ts"), "{out}");
}

#[tokio::test]
async fn tree_omits_ignored_directories_and_files() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, "src/a.ts", "x");
    write(root, "node_modules/pkg/index.ts", "x");
    write(root, "build/out.js", "x");
    write(root, "keep.txt", "x");
    write(root, ".gitignore", "build/\n");

    let out = run_text(&Tree, json!({}), &ignore_config(root)).await;
    assert!(out.contains("src"), "{out}");
    assert!(out.contains("keep.txt"), "{out}");
    assert!(!out.contains("node_modules"), "{out}");
    assert!(!out.contains("build"), "{out}");
}

#[tokio::test]
async fn list_directory_hides_ignored_entries_from_normal_directory() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, "src/a.ts", "x");
    write(root, "node_modules/pkg/index.ts", "x");
    write(root, "keep.txt", "x");

    let out = run_text(&ListDirectory, json!({}), &ignore_config(root)).await;
    assert!(out.contains("keep.txt"), "{out}");
    assert!(out.contains("src/"), "{out}");
    assert!(!out.contains("node_modules"), "{out}");
}

#[tokio::test]
async fn list_directory_still_lists_contents_when_pointed_at_ignored_dir() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(root, "node_modules/pkg/index.ts", "x");

    let out = run_text(
        &ListDirectory,
        json!({ "path": "node_modules" }),
        &ignore_config(root),
    )
    .await;
    assert!(out.contains("pkg"), "{out}");
}

// --- filesystem.test.ts: glob / grep / list / tree happy paths ---------

/// The filesystem.test.ts fixtures: a small project tree under one work dir.
fn fs_fixture() -> (TempDir, AppConfig) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(
        root,
        "src/index.ts",
        "export const hello = 'world';\nconsole.log(hello);\n",
    );
    write(
        root,
        "src/utils.ts",
        "export function add(a: number, b: number) { return a + b; }\n",
    );
    write(root, "docs/README.md", "# Hello\nThis is a readme.\n");
    write(root, "package.json", "{\"name\": \"test\"}");
    let config = default_config(root.to_path_buf());
    (dir, config)
}

#[tokio::test]
async fn glob_finds_files_matching_pattern() {
    let (_dir, config) = fs_fixture();
    let out = run_text(&Glob, json!({ "pattern": "**/*.ts" }), &config).await;
    assert!(out.contains("src/index.ts"), "{out}");
    assert!(out.contains("src/utils.ts"), "{out}");
    assert!(!out.contains("README.md"), "{out}");
}

#[tokio::test]
async fn glob_finds_in_subdirectory() {
    let (_dir, config) = fs_fixture();
    let out = run_text(&Glob, json!({ "pattern": "*.md", "path": "docs" }), &config).await;
    assert!(out.contains("README.md"), "{out}");
}

#[tokio::test]
async fn grep_finds_matching_lines() {
    let (_dir, config) = fs_fixture();
    let out = run_text(&Grep, json!({ "pattern": "hello" }), &config).await;
    assert!(out.contains("index.ts"), "{out}");
    assert!(out.contains("hello"), "{out}");
}

#[tokio::test]
async fn grep_includes_context_lines() {
    let (_dir, config) = fs_fixture();
    let out = run_text(&Grep, json!({ "pattern": "hello", "context": 1 }), &config).await;
    assert!(out.contains("console.log"), "{out}");
}

#[tokio::test]
async fn grep_filters_by_include_pattern() {
    let (_dir, config) = fs_fixture();
    let out = run_text(
        &Grep,
        json!({ "pattern": "Hello", "include": "*.md" }),
        &config,
    )
    .await;
    assert!(out.contains("README.md"), "{out}");
    assert!(!out.contains("index.ts"), "{out}");
}

#[tokio::test]
async fn list_directory_lists_root() {
    let (_dir, config) = fs_fixture();
    let out = run_text(&ListDirectory, json!({}), &config).await;
    assert!(out.contains("src"), "{out}");
    assert!(out.contains("docs"), "{out}");
    assert!(out.contains("package.json"), "{out}");
}

#[tokio::test]
async fn list_directory_lists_subdirectory() {
    let (_dir, config) = fs_fixture();
    let out = run_text(&ListDirectory, json!({ "path": "src" }), &config).await;
    assert!(out.contains("index.ts"), "{out}");
    assert!(out.contains("utils.ts"), "{out}");
}

#[tokio::test]
async fn tree_shows_directory_tree() {
    let (_dir, config) = fs_fixture();
    let out = run_text(&Tree, json!({}), &config).await;
    assert!(out.contains("src"), "{out}");
    assert!(out.contains("index.ts"), "{out}");
    assert!(out.contains("docs"), "{out}");
}

#[tokio::test]
async fn tree_respects_depth_limit() {
    let (_dir, config) = fs_fixture();
    let out = run_text(&Tree, json!({ "depth": 1 }), &config).await;
    assert!(out.contains("src"), "{out}");
    assert!(!out.contains("index.ts"), "{out}");
}

// --- read-file.test.ts -------------------------------------------------

fn read_fixture() -> (TempDir, AppConfig) {
    let dir = TempDir::new().unwrap();
    write(
        dir.path(),
        "hello.txt",
        "line1\nline2\nline3\nline4\nline5\n",
    );
    let config = default_config(dir.path().to_path_buf());
    (dir, config)
}

#[tokio::test]
async fn read_file_reads_entire_file_with_line_numbers() {
    let (_dir, config) = read_fixture();
    let session = SessionState::new();
    let r = ReadFile
        .call(json!({ "path": "hello.txt" }), &config, &session)
        .await;
    assert!(!r.is_error);
    let text = r.joined_text();
    assert!(text.contains("1\tline1"), "{text}");
    assert!(text.contains("5\tline5"), "{text}");
}

#[tokio::test]
async fn read_file_reads_with_offset_and_limit() {
    let (_dir, config) = read_fixture();
    let session = SessionState::new();
    let r = ReadFile
        .call(
            json!({ "path": "hello.txt", "offset": 2, "limit": 2 }),
            &config,
            &session,
        )
        .await;
    let text = r.joined_text();
    assert!(text.contains("3\tline3"), "{text}");
    assert!(text.contains("4\tline4"), "{text}");
    assert!(!text.contains("1\tline1"), "{text}");
}

#[tokio::test]
async fn read_file_rejects_path_traversal() {
    let (_dir, config) = read_fixture();
    let session = SessionState::new();
    let r = ReadFile
        .call(json!({ "path": "../../etc/passwd" }), &config, &session)
        .await;
    assert!(r.is_error);
    assert!(
        r.joined_text()
            .contains("Path must be within work directory")
    );
}

#[tokio::test]
async fn read_file_returns_error_for_missing_file() {
    let (_dir, config) = read_fixture();
    let session = SessionState::new();
    let r = ReadFile
        .call(json!({ "path": "nope.txt" }), &config, &session)
        .await;
    assert!(r.is_error);
    assert!(r.joined_text().contains("File not found"));
}

// --- write-file.test.ts ------------------------------------------------

#[tokio::test]
async fn write_file_writes_content_to_new_file() {
    let dir = TempDir::new().unwrap();
    let config = default_config(dir.path().to_path_buf());
    let session = SessionState::new();
    let r = WriteFile
        .call(
            json!({ "path": "out.txt", "content": "hello world" }),
            &config,
            &session,
        )
        .await;
    assert!(!r.is_error);
    let written = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
    assert_eq!(written, "hello world");
}

#[tokio::test]
async fn write_file_creates_parent_directories() {
    let dir = TempDir::new().unwrap();
    let config = default_config(dir.path().to_path_buf());
    let session = SessionState::new();
    let r = WriteFile
        .call(
            json!({ "path": "deep/nested/file.txt", "content": "nested" }),
            &config,
            &session,
        )
        .await;
    assert!(!r.is_error);
    let written = std::fs::read_to_string(dir.path().join("deep/nested/file.txt")).unwrap();
    assert_eq!(written, "nested");
}

#[tokio::test]
async fn write_file_overwrites_existing_file() {
    let dir = TempDir::new().unwrap();
    let config = default_config(dir.path().to_path_buf());
    let session = SessionState::new();
    WriteFile
        .call(
            json!({ "path": "overwrite.txt", "content": "v1" }),
            &config,
            &session,
        )
        .await;
    WriteFile
        .call(
            json!({ "path": "overwrite.txt", "content": "v2" }),
            &config,
            &session,
        )
        .await;
    let written = std::fs::read_to_string(dir.path().join("overwrite.txt")).unwrap();
    assert_eq!(written, "v2");
}

#[tokio::test]
async fn write_file_rejects_path_traversal() {
    let dir = TempDir::new().unwrap();
    let config = default_config(dir.path().to_path_buf());
    let session = SessionState::new();
    let r = WriteFile
        .call(
            json!({ "path": "../../evil.txt", "content": "bad" }),
            &config,
            &session,
        )
        .await;
    assert!(r.is_error);
    assert!(
        r.joined_text()
            .contains("Path must be within work directory")
    );
}

// --- output-caps.test.ts -----------------------------------------------

#[tokio::test]
async fn caps_read_file_stops_at_line_budget_and_names_offset() {
    let dir = TempDir::new().unwrap();
    let body: String = (1..=50)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    write(dir.path(), "big.txt", &body);
    let mut config = default_config(dir.path().to_path_buf());
    config.output.max_file_lines = Some(10);

    let result = run_result(&ReadFile, json!({ "path": "big.txt" }), &config).await;
    let out = result.joined_text();
    assert_eq!(result.audit.truncated, Some(true));
    assert!(out.contains("10\tline10"), "{out}");
    assert!(!out.contains("11\tline11"), "{out}");
    assert!(
        out.contains("(showing lines 1-10 of 50 \u{2014} call again with offset=10 for the rest)"),
        "{out}"
    );
}

#[tokio::test]
async fn caps_read_file_named_offset_continues() {
    let dir = TempDir::new().unwrap();
    let body: String = (1..=50)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    write(dir.path(), "big.txt", &body);
    let mut config = default_config(dir.path().to_path_buf());
    config.output.max_file_lines = Some(10);

    let out = run_text(
        &ReadFile,
        json!({ "path": "big.txt", "offset": 10 }),
        &config,
    )
    .await;
    assert!(out.contains("11\tline11"), "{out}");
    assert!(out.contains("(showing lines 11-20 of 50"), "{out}");
}

#[tokio::test]
async fn caps_read_file_small_file_comes_back_whole_no_notice() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), "small.txt", "a\nb");
    let config = default_config(dir.path().to_path_buf());

    let result = run_result(&ReadFile, json!({ "path": "small.txt" }), &config).await;
    let out = result.joined_text();
    assert_eq!(result.audit.truncated, Some(false));
    assert_eq!(out, "1\ta\n2\tb");
}

#[tokio::test]
async fn caps_read_file_single_enormous_line_cut_at_byte_budget() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), "bundle.js", &"x".repeat(10_000));
    let mut config = default_config(dir.path().to_path_buf());
    config.output.max_file_bytes = Some(500);

    let out = run_text(&ReadFile, json!({ "path": "bundle.js" }), &config).await;
    assert!(out.len() < 1_000, "len={}", out.len());
    assert!(out.contains("cut at the byte budget"), "{out}");
}

#[tokio::test]
async fn caps_glob_cuts_match_list_and_says_how_to_narrow() {
    let dir = TempDir::new().unwrap();
    for i in 0..20 {
        write(dir.path(), &format!("f{i}.ts"), "");
    }
    let mut config = default_config(dir.path().to_path_buf());
    config.output.max_entries = Some(5);

    let result = run_result(&Glob, json!({ "pattern": "*.ts" }), &config).await;
    let out = result.joined_text();
    assert_eq!(result.audit.truncated, Some(true));
    let ts_lines = out.lines().filter(|l| l.ends_with(".ts")).count();
    assert_eq!(ts_lines, 5, "{out}");
    assert!(out.contains("(showing 5 of 20 matches"), "{out}");
    assert!(out.contains("narrow the pattern"), "{out}");
}

#[tokio::test]
async fn caps_glob_no_limit_notice_when_all_fit() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), "only.ts", "");
    let config = default_config(dir.path().to_path_buf());

    let out = run_text(&Glob, json!({ "pattern": "*.ts" }), &config).await;
    assert!(!out.contains("showing"), "{out}");
}

#[tokio::test]
async fn caps_list_directory_cuts_entry_list_and_points_at_glob() {
    let dir = TempDir::new().unwrap();
    for i in 0..20 {
        write(dir.path(), &format!("f{i}.txt"), "");
    }
    let mut config = default_config(dir.path().to_path_buf());
    config.output.max_entries = Some(5);

    let result = run_result(&ListDirectory, json!({}), &config).await;
    let out = result.joined_text();
    assert_eq!(result.audit.truncated, Some(true));
    assert!(out.contains("(showing 5 of 20 entries"), "{out}");
    assert!(out.contains("use glob"), "{out}");
}

#[tokio::test]
async fn caps_list_directory_leaves_small_directory_alone() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), "a.txt", "");
    let config = default_config(dir.path().to_path_buf());

    let out = run_text(&ListDirectory, json!({}), &config).await;
    assert!(out.contains("a.txt"), "{out}");
    assert!(!out.contains("showing"), "{out}");
}

#[tokio::test]
async fn caps_tree_stops_at_node_budget_and_says_how_to_get_less() {
    let dir = TempDir::new().unwrap();
    for i in 0..20 {
        write(dir.path(), &format!("f{i}.txt"), "");
    }
    let mut config = default_config(dir.path().to_path_buf());
    config.output.max_tree_nodes = Some(6);

    let result = run_result(&Tree, json!({}), &config).await;
    let out = result.joined_text();
    assert_eq!(result.audit.truncated, Some(true));
    assert!(out.contains("(stopped at 6 nodes"), "{out}");
    assert!(out.contains("lower \"depth\""), "{out}");
}

#[tokio::test]
async fn caps_tree_budget_is_shared_across_directories() {
    let dir = TempDir::new().unwrap();
    for d in ["a", "b"] {
        for i in 0..10 {
            write(dir.path(), &format!("{d}/f{i}.txt"), "");
        }
    }
    let mut config = default_config(dir.path().to_path_buf());
    config.output.max_tree_nodes = Some(8);

    let out = run_text(&Tree, json!({}), &config).await;
    let file_lines = out.lines().filter(|l| l.contains('f')).count();
    assert!(file_lines <= 8, "file_lines={file_lines}\n{out}");
    assert!(out.contains("stopped at 8 nodes"), "{out}");
}

#[tokio::test]
async fn caps_tree_that_fits_carries_no_notice() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), "a.txt", "");
    let config = default_config(dir.path().to_path_buf());

    let out = run_text(&Tree, json!({}), &config).await;
    assert!(out.contains("a.txt"), "{out}");
    assert!(!out.contains("stopped at"), "{out}");
}
