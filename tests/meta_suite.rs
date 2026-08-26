//! Ported from the Bun/TypeScript suites:
//!   - src/__tests__/registry.test.ts
//!   - src/__tests__/structured-content.test.ts
//!   - src/tools/__tests__/git-status.test.ts
//!   - src/tools/__tests__/git-tools.test.ts
//!
//! Plus a small set of resolve_safe_path integration checks (the Rust module the
//! assignment calls out). All assertions are written against the ACTUAL Rust
//! behavior, not the old JS strings.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use async_trait::async_trait;
use serde_json::{Value, json};
use tempfile::TempDir;

use codexify::config::default_config;
use codexify::exec_sessions::SessionState;
use codexify::registry::{load_tools, load_tools_for_config, load_tools_for_mode};
use codexify::safe_path::resolve_safe_path;
use codexify::tool::Tool;
use codexify::types::{AppConfig, ToolContent, ToolResult};

// ─── registry.test.ts ──────────────────────────────────────────────────

#[test]
fn loads_all_27_tools() {
    let tools = load_tools();
    assert_eq!(tools.len(), 27);
}

#[test]
fn multi_project_mode_adds_catalogue_and_session_selector() {
    let tools = load_tools_for_mode(true);
    assert_eq!(tools.len(), 29);
    assert_eq!(tools[0].name(), "list_projects");
    assert_eq!(tools[1].name(), "set_project_root");
}

#[test]
fn artifact_ingress_can_be_omitted_by_configuration() {
    let mut config = default_config(PathBuf::from("/tmp"));
    config.artifact_ingress.enabled = false;
    let names = load_tools_for_config(&config)
        .into_iter()
        .map(|tool| tool.name())
        .collect::<Vec<_>>();
    assert_eq!(names.len(), 26);
    assert!(!names.contains(&"import_host_file"));
}

#[test]
fn conversation_auth_mode_adds_authenticate_before_protected_tools() {
    let mut config = default_config(PathBuf::from("/tmp"));
    config.conversation_auth_token =
        Some("codexify_chat_0123456789abcdef0123456789abcdef".into());
    let tools = load_tools_for_config(&config);
    assert_eq!(tools.len(), 28);
    assert_eq!(tools[0].name(), "authenticate");
    let schema = tools[0].input_schema();
    assert!(schema["properties"].get("checksum").is_some());
    assert!(schema["properties"].get("token").is_none());
    assert_eq!(schema["required"], serde_json::json!(["checksum"]));
    let description = tools[0].description().to_ascii_lowercase();
    assert!(description.contains("checksum"));
    assert!(!description.contains("token"));
    assert!(!description.contains("api key"));

    config.multi_project = true;
    let tools = load_tools_for_config(&config);
    assert_eq!(tools.len(), 30);
    assert_eq!(tools[0].name(), "authenticate");
    assert_eq!(tools[1].name(), "list_projects");
    assert_eq!(tools[2].name(), "set_project_root");
}

#[test]
fn all_tools_have_unique_names() {
    let tools = load_tools();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    let total = names.len();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), total);
}

#[test]
fn all_tools_have_required_fields() {
    let tools = load_tools();
    for tool in &tools {
        assert!(!tool.name().is_empty(), "name must be non-empty");
        assert!(
            !tool.description().is_empty(),
            "description must be non-empty"
        );
        // inputSchema truthy: must be a JSON object.
        assert!(
            tool.input_schema().is_object(),
            "input_schema for {} must be an object",
            tool.name()
        );
    }
}

#[test]
fn includes_expected_tool_names() {
    let tools = load_tools();
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    for expected in [
        "read_file",
        "write_file",
        "import_host_file",
        "run_command",
        "git_status",
        "show_changes",
        "git_push",
        "git_commit",
        "git_log",
        "glob",
        "grep",
        "list_directory",
        "tree",
    ] {
        assert!(names.contains(&expected), "missing tool: {expected}");
    }
}

#[test]
fn includes_tools_ported_from_codex() {
    let tools = load_tools();
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    for expected in [
        "apply_patch",
        "exec_command",
        "write_stdin",
        "view_image",
        "update_plan",
        "clock_curr_time",
        "clock_sleep",
        "skills_list",
        "skills_read",
    ] {
        assert!(names.contains(&expected), "missing tool: {expected}");
    }
}

#[test]
fn includes_tools_codex_has_no_equivalent_of() {
    let tools = load_tools();
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    for expected in [
        "get_environment",
        "get_project_doc",
        "get_agent_brief",
        "remember",
        "recall",
    ] {
        assert!(names.contains(&expected), "missing tool: {expected}");
    }
}

/// Validate a name against the MCP tool-name pattern `^[a-zA-Z0-9_-]{1,64}$`
/// manually (the `regex` crate is not a guaranteed integration-test dep).
fn is_valid_mcp_name(name: &str) -> bool {
    let len = name.chars().count();
    if len == 0 || len > 64 {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[test]
fn all_tool_names_are_valid_mcp_names() {
    let tools = load_tools();
    for tool in &tools {
        assert!(
            is_valid_mcp_name(tool.name()),
            "invalid MCP tool name: {}",
            tool.name()
        );
    }
}

#[test]
fn show_changes_links_the_review_mcp_app() {
    let tools = load_tools();
    let tool = tools
        .iter()
        .find(|tool| tool.name() == "show_changes")
        .unwrap();
    assert_eq!(tool.title().as_deref(), Some("Show changes"));
    let meta = tool.meta().unwrap();
    assert_eq!(
        meta.get("ui")
            .and_then(|value| value.get("resourceUri"))
            .and_then(Value::as_str),
        Some(codexify::review_ui::REVIEW_UI_URI)
    );
}

#[test]
fn mutating_tools_are_classified_for_checkpoint_fail_closed_behavior() {
    let mut names = load_tools()
        .into_iter()
        .filter(|tool| tool.may_modify_project())
        .map(|tool| tool.name().to_string())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        [
            "apply_patch".to_string(),
            "exec_command".to_string(),
            "git_commit".to_string(),
            "import_host_file".to_string(),
            "run_command".to_string(),
            "write_file".to_string(),
            "write_stdin".to_string(),
        ]
    );
}

// ─── structured-content.test.ts ────────────────────────────────────────
//
// There is no free `withStructuredContent` function in Rust; the default-fill
// rule lives in `server.call_tool`. We replicate that exact rule here and drive
// it with a fake Tool whose output_schema is configurable, mirroring the TS
// `makeTool(outputSchema?)` helper.

/// The server's default-fill rule (server.rs::call_tool), reproduced verbatim:
/// a tool that advertises an output schema, did not error, and did not build its
/// own structured content gets `{ "content": joined_text }`. Returns a new
/// result; the input is not mutated (Rust ownership makes this structural).
fn apply_default_structured(tool: &dyn Tool, result: &ToolResult) -> ToolResult {
    let mut out = result.clone();
    if tool.output_schema().is_some() && !out.is_error && out.structured_content.is_none() {
        out.structured_content = Some(json!({ "content": out.joined_text() }));
    }
    out
}

struct FakeTool {
    schema: Option<Value>,
}

#[async_trait]
impl Tool for FakeTool {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn description(&self) -> String {
        String::new()
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn output_schema(&self) -> Option<Value> {
        self.schema.clone()
    }
    async fn call(&self, _args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        ToolResult {
            content: vec![],
            is_error: false,
            structured_content: None,
            meta: None,
            audit: Default::default(),
        }
    }
}

fn content_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "content": { "type": "string" } }
    })
}

fn tool_with_schema(schema: Option<Value>) -> FakeTool {
    FakeTool { schema }
}

#[test]
fn derives_content_from_text_blocks() {
    let tool = tool_with_schema(Some(content_schema()));
    let result = ToolResult {
        content: vec![ToolContent::Text("hello".into())],
        is_error: false,
        structured_content: None,
        meta: None,
        audit: Default::default(),
    };
    let filled = apply_default_structured(&tool, &result);
    assert_eq!(
        filled.structured_content,
        Some(json!({ "content": "hello" }))
    );
}

#[test]
fn joins_multiple_text_blocks_and_skips_non_text() {
    let tool = tool_with_schema(Some(content_schema()));
    let result = ToolResult {
        content: vec![
            ToolContent::Text("one".into()),
            ToolContent::Image {
                data: "AAAA".into(),
                mime_type: "image/png".into(),
            },
            ToolContent::Text("two".into()),
        ],
        is_error: false,
        structured_content: None,
        meta: None,
        audit: Default::default(),
    };
    let filled = apply_default_structured(&tool, &result);
    assert_eq!(
        filled.structured_content,
        Some(json!({ "content": "one\ntwo" }))
    );
}

#[test]
fn leaves_tools_own_structured_content_alone() {
    let tool = tool_with_schema(Some(content_schema()));
    let result = ToolResult {
        content: vec![ToolContent::Text("{}".into())],
        is_error: false,
        structured_content: Some(json!({ "current_time": "2026-01-01 00:00:00 UTC" })),
        meta: None,
        audit: Default::default(),
    };
    let filled = apply_default_structured(&tool, &result);
    assert_eq!(
        filled.structured_content,
        Some(json!({ "current_time": "2026-01-01 00:00:00 UTC" }))
    );
}

#[test]
fn adds_nothing_when_tool_declares_no_output_schema() {
    let tool = tool_with_schema(None);
    let result = ToolResult {
        content: vec![ToolContent::Text("hello".into())],
        is_error: false,
        structured_content: None,
        meta: None,
        audit: Default::default(),
    };
    let filled = apply_default_structured(&tool, &result);
    assert_eq!(filled.structured_content, None);
}

#[test]
fn adds_nothing_to_an_error_result() {
    let tool = tool_with_schema(Some(content_schema()));
    let result = ToolResult {
        content: vec![ToolContent::Text("boom".into())],
        is_error: true,
        structured_content: None,
        meta: None,
        audit: Default::default(),
    };
    let filled = apply_default_structured(&tool, &result);
    assert_eq!(filled.structured_content, None);
}

#[test]
fn does_not_mutate_the_result_it_was_given() {
    let tool = tool_with_schema(Some(content_schema()));
    let result = ToolResult {
        content: vec![ToolContent::Text("hello".into())],
        is_error: false,
        structured_content: None,
        meta: None,
        audit: Default::default(),
    };
    let _ = apply_default_structured(&tool, &result);
    assert_eq!(result.structured_content, None);
}

/// registry output schemas: the generic default satisfies every schema that only
/// requires `content`; any tool whose output schema `required` lists a key other
/// than `content` must build its own structuredContent. This pins that list.
#[test]
fn tools_that_need_their_own_structured_content() {
    let tools = load_tools();
    let mut needs_own: Vec<String> = tools
        .iter()
        .filter(|tool| {
            let required: Vec<String> = tool
                .output_schema()
                .and_then(|s| {
                    s.get("required").and_then(|r| r.as_array()).map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                })
                .unwrap_or_default();
            required.iter().any(|key| key != "content")
        })
        .map(|tool| tool.name().to_string())
        .collect();
    // Rust str::cmp; the expected list is ASCII-sorted so it matches JS here.
    needs_own.sort();

    assert_eq!(
        needs_own,
        vec![
            "clock_curr_time".to_string(),
            "exec_command".to_string(),
            "get_environment".to_string(),
            "get_project_doc".to_string(),
            "import_host_file".to_string(),
            "show_changes".to_string(),
            "skills_list".to_string(),
            "write_stdin".to_string(),
        ]
    );
}

// ─── git helpers ───────────────────────────────────────────────────────

fn git(dir: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git must be installed to run these tests");
    // Not all commands are expected to succeed by the caller; we only assert the
    // process ran. (Callers that need success invoke commands known to succeed.)
    let _ = status;
}

fn init_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    git(p, &["init"]);
    git(p, &["config", "user.email", "test@test.com"]);
    git(p, &["config", "user.name", "Test"]);
    // Avoid a machine-global signing config aborting commits in CI.
    git(p, &["config", "commit.gpgsign", "false"]);
    dir
}

// ─── git-status.test.ts ────────────────────────────────────────────────

#[tokio::test]
async fn git_status_clean_after_initial_commit() {
    use codexify::tools::git_status::GitStatus;

    let repo = init_repo();
    let p = repo.path();
    std::fs::write(p.join("init.txt"), "init").unwrap();
    git(p, &["add", "."]);
    git(p, &["commit", "-m", "init"]);

    let config = default_config(p.to_path_buf());
    let session = SessionState::new();
    let r = GitStatus.call(json!({}), &config, &session).await;

    assert!(!r.is_error);
    // Rust returns "Working tree clean — no changes."; TS asserts "clean".
    assert!(
        r.joined_text().contains("clean"),
        "got: {}",
        r.joined_text()
    );
}

#[tokio::test]
async fn git_status_shows_untracked_files() {
    use codexify::tools::git_status::GitStatus;

    let repo = init_repo();
    let p = repo.path();
    std::fs::write(p.join("init.txt"), "init").unwrap();
    git(p, &["add", "."]);
    git(p, &["commit", "-m", "init"]);

    std::fs::write(p.join("new-file.txt"), "new").unwrap();

    let config = default_config(p.to_path_buf());
    let session = SessionState::new();
    let r = GitStatus.call(json!({}), &config, &session).await;

    let text = r.joined_text();
    assert!(text.contains("new-file.txt"), "got: {text}");
    assert!(text.contains("??"), "got: {text}");
}

// ─── git-tools.test.ts: git_commit ─────────────────────────────────────

#[tokio::test]
async fn git_commit_commits_staged_changes() {
    use codexify::tools::git_commit::GitCommit;

    let repo = init_repo();
    let p = repo.path();
    std::fs::write(p.join("file.txt"), "content").unwrap();
    git(p, &["add", "file.txt"]);

    let config = default_config(p.to_path_buf());
    let session = SessionState::new();
    let r = GitCommit
        .call(json!({ "message": "initial commit" }), &config, &session)
        .await;

    assert!(!r.is_error, "got error: {}", r.joined_text());
    assert!(
        r.joined_text().contains("initial commit"),
        "got: {}",
        r.joined_text()
    );
}

#[tokio::test]
async fn git_commit_commits_with_all_flag() {
    use codexify::tools::git_commit::GitCommit;

    let repo = init_repo();
    let p = repo.path();
    // First establish a tracked file.
    std::fs::write(p.join("file.txt"), "content").unwrap();
    git(p, &["add", "file.txt"]);
    git(p, &["commit", "-m", "seed"]);

    // Now modify the tracked file and commit with all=true.
    std::fs::write(p.join("file.txt"), "updated content").unwrap();

    let config = default_config(p.to_path_buf());
    let session = SessionState::new();
    let r = GitCommit
        .call(
            json!({ "message": "update file", "all": true }),
            &config,
            &session,
        )
        .await;

    assert!(!r.is_error, "got error: {}", r.joined_text());
    assert!(
        r.joined_text().contains("update file"),
        "got: {}",
        r.joined_text()
    );
}

#[tokio::test]
async fn git_commit_fails_when_nothing_to_commit() {
    use codexify::tools::git_commit::GitCommit;

    let repo = init_repo();
    let p = repo.path();
    // Seed one commit so the working tree is clean afterwards.
    std::fs::write(p.join("file.txt"), "content").unwrap();
    git(p, &["add", "file.txt"]);
    git(p, &["commit", "-m", "seed"]);

    let config = default_config(p.to_path_buf());
    let session = SessionState::new();
    let r = GitCommit
        .call(json!({ "message": "empty" }), &config, &session)
        .await;

    assert!(r.is_error);
    assert!(
        r.joined_text().contains("nothing to commit"),
        "got: {}",
        r.joined_text()
    );
}

// ─── git-tools.test.ts: git_log ────────────────────────────────────────

fn init_log_repo() -> TempDir {
    let repo = init_repo();
    let p = repo.path();
    std::fs::write(p.join("a.txt"), "a").unwrap();
    git(p, &["add", "."]);
    git(p, &["commit", "-m", "first commit"]);
    std::fs::write(p.join("b.txt"), "b").unwrap();
    git(p, &["add", "."]);
    git(p, &["commit", "-m", "second commit"]);
    repo
}

#[tokio::test]
async fn git_log_shows_commit_history() {
    use codexify::tools::git_log::GitLog;

    let repo = init_log_repo();
    let config = default_config(repo.path().to_path_buf());
    let session = SessionState::new();
    let r = GitLog.call(json!({}), &config, &session).await;

    assert!(!r.is_error, "got error: {}", r.joined_text());
    let text = r.joined_text();
    assert!(text.contains("first commit"), "got: {text}");
    assert!(text.contains("second commit"), "got: {text}");
}

#[tokio::test]
async fn git_log_limits_count() {
    use codexify::tools::git_log::GitLog;

    let repo = init_log_repo();
    let config = default_config(repo.path().to_path_buf());
    let session = SessionState::new();
    let r = GitLog.call(json!({ "count": 1 }), &config, &session).await;

    let text = r.joined_text();
    assert!(text.contains("second commit"), "got: {text}");
    assert!(!text.contains("first commit"), "got: {text}");
}

#[tokio::test]
async fn git_log_supports_oneline_format() {
    use codexify::tools::git_log::GitLog;

    let repo = init_log_repo();
    let config = default_config(repo.path().to_path_buf());
    let session = SessionState::new();
    let r = GitLog
        .call(json!({ "oneline": true }), &config, &session)
        .await;

    let text = r.joined_text();
    let lines: Vec<&str> = text.trim().split('\n').collect();
    assert_eq!(lines.len(), 2, "got: {text:?}");
}

// ─── safe_path::resolve_safe_path ──────────────────────────────────────

fn wd() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("C:\\work\\project")
    } else {
        PathBuf::from("/work/project")
    }
}

#[test]
fn safe_path_resolves_relative_within() {
    let p = resolve_safe_path("src/main.rs", &wd(), false).unwrap();
    assert!(p.ends_with("src/main.rs"));
    assert!(p.starts_with(wd()));
}

#[test]
fn safe_path_empty_requires_allow_empty() {
    assert!(resolve_safe_path("", &wd(), true).is_ok());
    let err = resolve_safe_path("", &wd(), false).unwrap_err();
    assert!(err.contains("must not be empty"), "got: {err}");
}

#[test]
fn safe_path_rejects_traversal() {
    let e1 = resolve_safe_path("../secret", &wd(), false).unwrap_err();
    assert!(e1.contains("within work directory"), "got: {e1}");
    assert!(resolve_safe_path("a/../../secret", &wd(), false).is_err());
}

#[test]
fn safe_path_allows_workdir_itself() {
    assert!(resolve_safe_path(".", &wd(), false).is_ok());
}

#[test]
fn safe_path_rejects_absolute_outside() {
    let outside = if cfg!(windows) {
        "C:\\other\\x"
    } else {
        "/other/x"
    };
    assert!(resolve_safe_path(outside, &wd(), false).is_err());
}
