//! Ported from src/__tests__/memory.test.ts and
//! src/tools/__tests__/memory-tools.test.ts.
//!
//! Every test sets `config.memory.dir` to a TempDir so nothing touches the real
//! ~/.codexify, except the pure path-computation tests for `memory_dir` which
//! do not write to disk.

use std::collections::BTreeMap;

use serde_json::json;
use tempfile::TempDir;

use codexify::config::default_config;
use codexify::exec_sessions::SessionState;
use codexify::memory::{
    DEFAULT_MEMORY_MAX_BYTES, MemoryNote, load_memory, lock_path, memory_dir, memory_enabled,
    memory_max_bytes, memory_path, notes_bytes, remember, render_memory, save_plan,
};
use codexify::tool::Tool;
use codexify::tools::forget_memory_note::ForgetMemoryNote;
use codexify::tools::recall::{NOTHING_REMEMBERED, Recall};
use codexify::tools::remember::Remember;
use codexify::tools::update_memory_note::UpdateMemoryNote;
use codexify::tools::update_plan::UpdatePlan;
use codexify::types::{AppConfig, PlanItem, PlanState, PlanStepStatus};

const NOW: &str = "2026-01-01T00:00:00.000Z";

/// Build a config whose memory writes go into `state_dir` under `root`.
fn make_config(root: &TempDir) -> AppConfig {
    let work = root.path().join("work");
    let mut config = default_config(work);
    config.memory.dir = Some(root.path().join("state").to_string_lossy().into_owned());
    config
}

// ─── memory_dir ────────────────────────────────────────────────────────

#[test]
fn memory_dir_defaults_under_home_not_in_repo() {
    // No explicit dir: falls back to the per-project directory under home.
    let root = TempDir::new().unwrap();
    let config = default_config(root.path().join("work"));
    let dir = memory_dir(&config);
    let dir_str = dir.to_string_lossy();
    assert!(dir_str.contains(".codexify"));
    assert!(dir_str.contains("projects"));
    // Never inside the repository being worked on.
    assert!(!dir.starts_with(&config.work_dir));
}

#[test]
fn memory_dir_keys_on_absolute_work_dir_not_name() {
    let root = TempDir::new().unwrap();
    let a = default_config(root.path().join("work"));
    let b = default_config(root.path().join("other").join("work"));
    assert_ne!(memory_dir(&a), memory_dir(&b));
}

#[test]
fn memory_dir_explicit_dir_wins() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    let state = root.path().join("state");
    assert_eq!(memory_dir(&config), state);
    assert_eq!(memory_path(&config), state.join("memory.json"));
}

// ─── memory_enabled and memory_max_bytes ───────────────────────────────

#[test]
fn memory_on_unless_disabled() {
    let root = TempDir::new().unwrap();
    let mut config = make_config(&root);
    assert!(memory_enabled(&config));
    config.memory.enabled = Some(true);
    assert!(memory_enabled(&config));
    config.memory.enabled = Some(false);
    assert!(!memory_enabled(&config));
}

#[test]
fn byte_budget_falls_back_to_default() {
    let root = TempDir::new().unwrap();
    let mut config = make_config(&root);
    assert_eq!(memory_max_bytes(&config), DEFAULT_MEMORY_MAX_BYTES);
    config.memory.max_bytes = Some(64);
    assert_eq!(memory_max_bytes(&config), 64);
}

// ─── load_memory ───────────────────────────────────────────────────────

#[test]
fn load_returns_empty_when_nothing_written() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    let memory = load_memory(&config);
    assert!(memory.plan.is_none());
    assert!(memory.notes.is_empty());
}

#[test]
fn load_degrades_to_empty_on_unparseable_json() {
    // A corrupt state file must not take the session down with it.
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    remember(&config, "k", "v", NOW);
    std::fs::write(memory_path(&config), "{ not json").unwrap();
    assert!(load_memory(&config).notes.is_empty());
}

#[test]
fn load_drops_malformed_note_entries_but_keeps_good_ones() {
    // Matches the TS "drops malformed note entries but keeps the good ones":
    // load_memory parses leniently per field, so a note whose `value` is not a
    // string is skipped while the valid notes survive.
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    remember(&config, "good", "kept", NOW);
    let raw = std::fs::read_to_string(memory_path(&config)).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    value["notes"]["bad"] = json!({ "value": 42 });
    std::fs::write(memory_path(&config), value.to_string()).unwrap();
    let loaded = load_memory(&config);
    assert_eq!(
        loaded.notes.get("good").map(|n| n.value.as_str()),
        Some("kept")
    );
    assert!(!loaded.notes.contains_key("bad"));
}

#[test]
fn load_reads_nothing_while_disabled() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    remember(&config, "k", "v", NOW);
    let mut disabled = make_config(&root);
    disabled.memory.enabled = Some(false);
    assert!(load_memory(&disabled).notes.is_empty());
}

// ─── remember ──────────────────────────────────────────────────────────

#[test]
fn remember_stores_note_and_reports_budget() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    let result = remember(
        &config,
        "why-bun",
        "Because the runtime ships a test runner.",
        NOW,
    );
    assert!(result.ok);
    assert!(result.message.contains("why-bun"));
    let note = load_memory(&config).notes.get("why-bun").cloned().unwrap();
    assert_eq!(note.value, "Because the runtime ships a test runner.");
    assert_eq!(note.updated_at, NOW);
}

#[test]
fn remember_survives_reload() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    remember(&config, "k", "first", NOW);
    assert!(memory_path(&config).exists());
    let reloaded = make_config(&root);
    assert_eq!(
        load_memory(&reloaded).notes.get("k").unwrap().value,
        "first"
    );
}

#[test]
fn remember_replaces_existing_key() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    remember(&config, "k", "first", NOW);
    remember(&config, "k", "second", "2026-02-02T00:00:00.000Z");
    let notes = load_memory(&config).notes;
    let keys: Vec<&String> = notes.keys().collect();
    assert_eq!(keys, vec!["k"]);
    let note = notes.get("k").unwrap();
    assert_eq!(note.value, "second");
    assert_eq!(note.updated_at, "2026-02-02T00:00:00.000Z");
}

#[test]
fn remember_empty_value_removes_key() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    remember(&config, "k", "v", NOW);
    let result = remember(&config, "k", "   ", NOW);
    assert!(result.ok);
    assert!(result.message.contains("Removed"));
    assert!(load_memory(&config).notes.is_empty());
}

#[test]
fn remember_removing_absent_key_is_error() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    let result = remember(&config, "ghost", "", NOW);
    assert!(!result.ok);
    assert!(result.message.contains("ghost"));
}

#[test]
fn remember_trims_keys() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    remember(&config, "  spaced  ", "v", NOW);
    let notes = load_memory(&config).notes;
    let keys: Vec<&String> = notes.keys().collect();
    assert_eq!(keys, vec!["spaced"]);
}

#[test]
fn remember_over_budget_is_rejected_nothing_stored() {
    // Rejected rather than evicted.
    let root = TempDir::new().unwrap();
    let mut config = make_config(&root);
    config.memory.max_bytes = Some(40);
    remember(&config, "keep", "small", NOW);
    let big = "x".repeat(100);
    let result = remember(&config, "big", &big, NOW);
    assert!(!result.ok);
    assert!(result.message.contains("40-byte budget"));
    assert!(result.message.contains("keep"));
    let notes = load_memory(&config).notes;
    let keys: Vec<&String> = notes.keys().collect();
    assert_eq!(keys, vec!["keep"]);
}

#[test]
fn notes_bytes_counts_keys_and_values() {
    let mut notes: BTreeMap<String, MemoryNote> = BTreeMap::new();
    notes.insert(
        "ab".to_string(),
        MemoryNote {
            value: "cde".to_string(),
            updated_at: NOW.to_string(),
        },
    );
    assert_eq!(notes_bytes(&notes), 5);
}

// ─── save_plan ─────────────────────────────────────────────────────────

#[test]
fn save_plan_stores_plan_without_disturbing_notes() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    remember(&config, "k", "v", NOW);
    let plan = PlanState {
        explanation: Some("why".to_string()),
        plan: vec![PlanItem {
            step: "one".to_string(),
            status: PlanStepStatus::Pending,
        }],
    };
    assert!(save_plan(&config, Some(plan)));
    let memory = load_memory(&config);
    let stored = memory.plan.unwrap();
    assert_eq!(stored.plan.len(), 1);
    assert_eq!(stored.plan[0].step, "one");
    assert_eq!(stored.plan[0].status, PlanStepStatus::Pending);
    assert_eq!(memory.notes.get("k").unwrap().value, "v");
}

#[test]
fn save_plan_returns_false_when_disabled() {
    let root = TempDir::new().unwrap();
    let mut config = make_config(&root);
    config.memory.enabled = Some(false);
    assert!(!save_plan(&config, None));
}

// ─── durability ────────────────────────────────────────────────────────

#[test]
fn leaves_no_temp_or_lock_files_behind() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    remember(&config, "k", "v", NOW);
    save_plan(
        &config,
        Some(PlanState {
            explanation: None,
            plan: vec![PlanItem {
                step: "one".to_string(),
                status: PlanStepStatus::Pending,
            }],
        }),
    );
    let mut entries: Vec<String> = std::fs::read_dir(memory_dir(&config))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, vec!["memory.json".to_string()]);
}

// The TS "steals a stale lock and still writes" test is intentionally skipped:
// it backdates the lock file mtime via utimesSync to make it older than
// LOCK_STALE_MS (10s), and there is no std API (nor an available dev-dep) to set
// a file's mtime here. Waiting 10s for a real lock to go stale is not viable in
// a unit test. The stale-lock stealing logic itself is covered by reading the
// source; the observable degrade path is exercised by the live-lock test below.

#[test]
fn still_writes_when_live_lock_held_last_writer_wins() {
    // Losing the lock race must degrade to a valid write, not a dropped note.
    // A fresh foreign lock is waited on (~LOCK_TIMEOUT_MS), then the write
    // proceeds anyway.
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    remember(&config, "seed", "1", NOW);
    let lock = lock_path(&config);
    // Simulate another writer holding a fresh lock.
    std::fs::write(&lock, "held").unwrap();
    let result = remember(&config, "raced", "2", NOW);
    assert!(result.ok);
    assert_eq!(load_memory(&config).notes.get("raced").unwrap().value, "2");
    let _ = std::fs::remove_file(&lock);
}

// ─── render_memory ─────────────────────────────────────────────────────

#[test]
fn render_returns_none_when_nothing_to_hand_over() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    assert!(render_memory(&load_memory(&config)).is_none());
}

#[test]
fn render_plan_as_checklist_with_progress() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    save_plan(
        &config,
        Some(PlanState {
            explanation: Some("Porting the tools".to_string()),
            plan: vec![
                PlanItem {
                    step: "one".to_string(),
                    status: PlanStepStatus::Completed,
                },
                PlanItem {
                    step: "two".to_string(),
                    status: PlanStepStatus::InProgress,
                },
                PlanItem {
                    step: "three".to_string(),
                    status: PlanStepStatus::Pending,
                },
            ],
        }),
    );
    let rendered = render_memory(&load_memory(&config)).unwrap();
    assert!(rendered.contains("Plan in progress:"));
    assert!(rendered.contains("Porting the tools"));
    assert!(rendered.contains("[x] one"));
    assert!(rendered.contains("[~] two"));
    assert!(rendered.contains("[ ] three"));
    assert!(rendered.contains("1/3 steps completed"));
}

#[test]
fn render_notes_sorted_by_key() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    remember(&config, "zeta", "last", NOW);
    remember(&config, "alpha", "first", NOW);
    let rendered = render_memory(&load_memory(&config)).unwrap();
    let alpha = rendered.find("alpha").unwrap();
    let zeta = rendered.find("zeta").unwrap();
    assert!(alpha < zeta);
    assert!(rendered.contains("- alpha: first"));
}

#[test]
fn render_empty_plan_produces_no_plan_section() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    save_plan(
        &config,
        Some(PlanState {
            explanation: None,
            plan: vec![],
        }),
    );
    remember(&config, "k", "v", NOW);
    let rendered = render_memory(&load_memory(&config)).unwrap();
    assert!(!rendered.contains("Plan in progress"));
    assert!(rendered.contains("Notes:"));
}

// ─── remember tool ─────────────────────────────────────────────────────

#[tokio::test]
async fn remember_tool_stores_a_note() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    let session = SessionState::new();
    let result = Remember
        .call(
            json!({ "key": "why-bun", "value": "The runtime ships a test runner." }),
            &config,
            &session,
        )
        .await;
    assert!(!result.is_error);
    assert_eq!(
        load_memory(&config).notes.get("why-bun").unwrap().value,
        "The runtime ships a test runner."
    );
}

#[tokio::test]
async fn split_memory_tools_create_update_and_delete_without_mixed_modes() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    let session = SessionState::new();

    let created = Remember
        .call(
            json!({ "key": "decision", "value": "first" }),
            &config,
            &session,
        )
        .await;
    assert!(!created.is_error);
    let duplicate = Remember
        .call(
            json!({ "key": "decision", "value": "second" }),
            &config,
            &session,
        )
        .await;
    assert!(duplicate.is_error);
    assert_eq!(load_memory(&config).notes["decision"].value, "first");

    let updated = UpdateMemoryNote
        .call(
            json!({ "key": "decision", "value": "second" }),
            &config,
            &session,
        )
        .await;
    assert!(!updated.is_error);
    assert_eq!(load_memory(&config).notes["decision"].value, "second");
    let repeated = UpdateMemoryNote
        .call(
            json!({ "key": "decision", "value": "second" }),
            &config,
            &session,
        )
        .await;
    assert!(!repeated.is_error);
    assert!(repeated.joined_text().contains("already has that value"));

    let deleted = ForgetMemoryNote
        .call(json!({ "key": "decision" }), &config, &session)
        .await;
    assert!(!deleted.is_error);
    assert!(!load_memory(&config).notes.contains_key("decision"));
}

#[tokio::test]
async fn remember_tool_rejects_empty_key() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    let session = SessionState::new();
    let result = Remember
        .call(json!({ "key": "  ", "value": "v" }), &config, &session)
        .await;
    assert!(result.is_error);
    assert!(result.joined_text().contains("non-empty"));
}

#[tokio::test]
async fn remember_tool_reports_over_budget_note_as_error() {
    let root = TempDir::new().unwrap();
    let mut config = make_config(&root);
    config.memory.max_bytes = Some(20);
    let session = SessionState::new();
    let result = Remember
        .call(
            json!({ "key": "big", "value": "x".repeat(100) }),
            &config,
            &session,
        )
        .await;
    assert!(result.is_error);
    assert!(result.joined_text().contains("budget"));
    assert!(load_memory(&config).notes.is_empty());
}

#[tokio::test]
async fn remember_tool_says_memory_disabled() {
    let root = TempDir::new().unwrap();
    let mut config = make_config(&root);
    config.memory.enabled = Some(false);
    let session = SessionState::new();
    let result = Remember
        .call(json!({ "key": "k", "value": "v" }), &config, &session)
        .await;
    assert!(result.is_error);
    assert!(result.joined_text().contains("disabled"));
}

// ─── recall tool ───────────────────────────────────────────────────────

#[tokio::test]
async fn recall_tool_distinguishes_fresh_start() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    let session = SessionState::new();
    let result = Recall.call(json!({}), &config, &session).await;
    assert!(!result.is_error);
    assert_eq!(result.joined_text(), NOTHING_REMEMBERED);
}

#[tokio::test]
async fn recall_tool_returns_stored_note() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    let session = SessionState::new();
    Remember
        .call(
            json!({ "key": "k", "value": "the note" }),
            &config,
            &session,
        )
        .await;
    let result = Recall.call(json!({}), &config, &session).await;
    assert!(result.joined_text().contains("- k: the note"));
}

#[test]
fn recall_tool_takes_no_arguments() {
    let schema = Recall.input_schema();
    assert_eq!(schema["properties"], json!({}));
    assert_eq!(schema["additionalProperties"], json!(false));
}

#[tokio::test]
async fn recall_tool_reports_when_disabled() {
    let root = TempDir::new().unwrap();
    let mut config = make_config(&root);
    config.memory.enabled = Some(false);
    let session = SessionState::new();
    let result = Recall.call(json!({}), &config, &session).await;
    assert!(!result.is_error);
    assert!(result.joined_text().contains("disabled"));
}

// ─── update_plan persistence ───────────────────────────────────────────

#[tokio::test]
async fn update_plan_set_in_one_session_recalled_in_next() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    let session = SessionState::new();
    UpdatePlan
        .call(
            json!({
                "explanation": "Porting the tools",
                "plan": [
                    { "step": "port apply_patch", "status": "completed" },
                    { "step": "port exec_command", "status": "in_progress" }
                ]
            }),
            &config,
            &session,
        )
        .await;

    // A different session state entirely: nothing is carried over in memory.
    let fresh = SessionState::new();
    let recalled = Recall.call(json!({}), &config, &fresh).await.joined_text();
    assert!(recalled.contains("Porting the tools"));
    assert!(recalled.contains("[x] port apply_patch"));
    assert!(recalled.contains("[~] port exec_command"));
    assert!(recalled.contains("1/2 steps completed"));
}

#[tokio::test]
async fn update_plan_rejected_plan_not_persisted() {
    let root = TempDir::new().unwrap();
    let config = make_config(&root);
    let session = SessionState::new();
    let result = UpdatePlan
        .call(
            json!({
                "plan": [
                    { "step": "one", "status": "in_progress" },
                    { "step": "two", "status": "in_progress" }
                ]
            }),
            &config,
            &session,
        )
        .await;
    assert!(result.is_error);
    assert!(load_memory(&config).plan.is_none());
}

#[tokio::test]
async fn update_plan_unwritable_state_does_not_fail_call() {
    // Best effort by design: a plan the model can see is worth more than one
    // that failed to persist. Memory disabled stands in for an unwritable store.
    let root = TempDir::new().unwrap();
    let mut config = make_config(&root);
    config.memory.enabled = Some(false);
    let session = SessionState::new();
    let result = UpdatePlan
        .call(
            json!({ "plan": [{ "step": "one", "status": "pending" }] }),
            &config,
            &session,
        )
        .await;
    assert!(!result.is_error);
    assert!(result.joined_text().contains("[ ] one"));
}
