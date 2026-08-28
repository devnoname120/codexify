//! Regression tests for the behavioral-fidelity fixes found by the adversarial
//! review of the Rust port against the TypeScript original.

use serde_json::json;
use tempfile::TempDir;

use codexify::config::default_config;
use codexify::exec_sessions::{SessionState, ShellType, resolve_shell, shell_type_of};
use codexify::tool::{Tool, arg_u64};
use codexify::tools::glob::Glob;
use codexify::tools::grep::Grep;
use codexify::tools::read_file::ReadFile;

// #1: shell_type_of strips `.exe` case-insensitively (cmd.Exe -> Cmd, not Posix).
#[test]
fn shell_type_strips_mixed_case_exe() {
    assert_eq!(shell_type_of("cmd.Exe"), ShellType::Cmd);
    assert_eq!(shell_type_of("PowerShell.Exe"), ShellType::PowerShell);
    assert_eq!(shell_type_of("pwsh.EXE"), ShellType::PowerShell);
    assert_eq!(resolve_shell(Some("cmd.Exe")), vec!["cmd.Exe", "/c"]);
}

// #18: integer-valued JSON floats (5.0 from e.g. a Python client) are accepted.
#[test]
fn arg_u64_accepts_integer_valued_floats() {
    assert_eq!(arg_u64(&json!({ "n": 5.0 }), "n"), Some(5));
    assert_eq!(arg_u64(&json!({ "n": 5 }), "n"), Some(5));
    assert_eq!(arg_u64(&json!({ "n": 5.5 }), "n"), None);
    assert_eq!(arg_u64(&json!({ "n": -1.0 }), "n"), None);
}

// #3 / #4: an empty `path` argument falls back to the work dir instead of erroring.
#[tokio::test]
async fn glob_and_grep_empty_path_falls_back_to_workdir() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.ts"), "const TODO = 1;\n").unwrap();
    let config = default_config(dir.path().to_path_buf());
    let session = SessionState::new();

    let g = Glob
        .call(json!({ "pattern": "*.ts", "path": "" }), &config, &session)
        .await;
    assert!(
        !g.is_error,
        "glob with empty path should not error: {:?}",
        g.joined_text()
    );
    assert!(g.joined_text().contains("a.ts"));

    let r = Grep
        .call(json!({ "pattern": "TODO", "path": "" }), &config, &session)
        .await;
    assert!(
        !r.is_error,
        "grep with empty path should not error: {:?}",
        r.joined_text()
    );
    assert!(r.joined_text().contains("a.ts"));
}

// #6: an unparseable glob pattern is graceful (no match), not an error.
#[tokio::test]
async fn glob_invalid_pattern_is_graceful() {
    let dir = TempDir::new().unwrap();
    let config = default_config(dir.path().to_path_buf());
    let session = SessionState::new();
    let g = Glob
        .call(json!({ "pattern": "src/[abc" }), &config, &session)
        .await;
    assert!(!g.is_error);
    assert!(g.joined_text().contains("No files found"));
}

// #9: a file with invalid UTF-8 is read lossily, not rejected as an error.
#[tokio::test]
async fn read_file_reads_invalid_utf8_lossily() {
    let dir = TempDir::new().unwrap();
    // "caf" + 0xE9 (Latin-1 é, invalid UTF-8) + "\n"
    std::fs::write(dir.path().join("latin.txt"), b"caf\xe9\n").unwrap();
    let config = default_config(dir.path().to_path_buf());
    let session = SessionState::new();
    let r = ReadFile
        .call(json!({ "path": "latin.txt" }), &config, &session)
        .await;
    assert!(!r.is_error, "invalid UTF-8 should not be an error");
    assert!(r.joined_text().contains("caf"));
    assert!(
        r.joined_text().contains('\u{fffd}'),
        "should contain the replacement char"
    );
}

// Numeric paging arguments are exact non-negative integers, matching the schema.
#[tokio::test]
async fn read_file_numeric_offset_and_limit() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("f.txt"), "l1\nl2\nl3\nl4\n").unwrap();
    let config = default_config(dir.path().to_path_buf());
    let session = SessionState::new();

    let valid = ReadFile
        .call(json!({ "path": "f.txt", "offset": 2 }), &config, &session)
        .await;
    assert!(!valid.is_error);
    assert!(valid.joined_text().contains("3\tl3"));

    let fractional = ReadFile
        .call(json!({ "path": "f.txt", "offset": 2.9 }), &config, &session)
        .await;
    assert!(fractional.is_error);

    let negative = ReadFile
        .call(json!({ "path": "f.txt", "limit": -1 }), &config, &session)
        .await;
    assert!(negative.is_error);
}

// #15: skill lookup is case-insensitive with full-Unicode folding.
#[test]
fn find_skill_is_unicode_case_insensitive() {
    let root = TempDir::new().unwrap();
    let skill_dir = root.path().join("uber-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: \u{dc}ber\ndescription: does a thing\n---\nbody\n",
    )
    .unwrap();

    let mut config = default_config(root.path().to_path_buf());
    config.skills.dirs = Some(vec![root.path().to_string_lossy().into_owned()]);

    let catalog = codexify::skills::discover_skills(&config);
    assert_eq!(catalog.skills.len(), 1, "skill should be discovered");
    // Uppercase Ü lookup should resolve via the Unicode-lowercasing fallback.
    assert!(codexify::skills::find_skill(&catalog, "\u{dc}BER").is_some());
}

// #16: an empty-string plan explanation is not rendered.
#[test]
fn render_memory_omits_empty_explanation() {
    use codexify::memory::render_memory;
    use codexify::types::{PlanItem, PlanState, PlanStepStatus};
    let mem = codexify::memory::Memory {
        work_dir: "/w".into(),
        plan: Some(PlanState {
            explanation: Some(String::new()),
            plan: vec![PlanItem {
                step: "do it".into(),
                status: PlanStepStatus::InProgress,
            }],
        }),
        notes: Default::default(),
    };
    let rendered = render_memory(&mem).unwrap();
    // "Plan in progress:" then the step directly — no blank explanation line.
    assert!(
        rendered.contains("Plan in progress:\n[~] do it"),
        "got:\n{rendered}"
    );
}
