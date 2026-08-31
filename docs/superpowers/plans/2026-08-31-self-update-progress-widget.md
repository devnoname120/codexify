# Self-update Progress Widget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add checksum-bound changelog display and restart-safe in-chat monitoring to Codexify self-updates while keeping polling app-only.

**Architecture:** Extend `self_update.rs` with a durable, atomically written status record and multi-file archive extraction. Link `self_update` to a dedicated MCP App whose component-only metadata contains the changelog, and register a read-only `self_update_status` tool with app-only visibility for polling across service restart. Keep executable verification, rollback, and service supervision as the authoritative update path.

**Tech Stack:** Rust, Tokio, serde/serde_json, semver, tar/flate2/zip, rmcp MCP Apps metadata, embedded HTML/CSS/JavaScript, GitHub Actions.

---

## File map

- Create `src/self_update_ui.rs`: updater tool/resource metadata, component payload, and embedded restart-monitoring MCP App.
- Create `src/tools/self_update_status.rs`: app-only status lookup tool.
- Modify `src/self_update.rs`: archive payload extraction, changelog selection, durable records, worker transitions, and cleanup.
- Modify `src/tools/self_update.rs`: attach the UI, expose bounded structured receipt, and put changelog in component-only metadata.
- Modify `src/tools/mod.rs`, `src/registry.rs`, `src/lib.rs`: register the new module and tool.
- Modify `src/server.rs`: advertise and serve both UI resources.
- Modify `tests/meta_suite.rs`: update tool inventory and visibility assertions.
- Modify `.github/workflows/release.yml`: package `CHANGELOG.md` in every archive and verify it is staged.
- Modify `README.md`, `docs/ARCHITECTURE.md`, and `CHANGELOG.md`: document the feature and durable protocol.

### Task 1: Durable update records and changelog selection

**Files:**
- Modify: `src/self_update.rs`

- [ ] **Step 1: Write failing tests for the record schema, private atomic writes, bounded retention, and changelog interval selection**

Add tests that construct records in a temporary `~/.codexify/update/status` root, round-trip them through the reader, reject malformed update IDs, and verify that only versions in `(current, target]` are retained from a multi-version changelog.

```rust
#[test]
fn changelog_selection_includes_every_skipped_release_in_range() {
    let selected = select_changelog_sections(FIXTURE, &Version::new(1, 0, 0), &Version::new(1, 2, 0)).unwrap();
    assert!(selected.contains("## [1.1.0]"));
    assert!(selected.contains("## [1.2.0]"));
    assert!(!selected.contains("## [1.0.0]"));
}

#[test]
fn status_record_round_trips_through_private_atomic_file() {
    let root = tempfile::tempdir().unwrap();
    let record = UpdateStatusRecord::scheduled("0123456789abcdef01234567", "1.0.0", "2.0.0");
    write_status_record(root.path(), &record).unwrap();
    assert_eq!(read_status_record_from(root.path(), &record.update_id).unwrap(), record);
}
```

- [ ] **Step 2: Run the focused tests and verify failure**

Run:

```bash
cargo test self_update::tests::changelog_selection self_update::tests::status_record --lib
```

Expected: compilation failure because the new record and selection APIs do not exist.

- [ ] **Step 3: Implement the durable protocol and changelog parser**

Add public serializable types and bounded helpers:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    Scheduled,
    Installing,
    Validating,
    Restarting,
    Succeeded,
    Failed,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatusRecord {
    pub update_id: String,
    pub from_version: String,
    pub target_version: String,
    pub state: UpdatePhase,
    pub updated_at: String,
    pub failure_code: Option<String>,
    pub failure_detail: Option<String>,
}
```

Implement strict 24-character lowercase hexadecimal ID validation, private directory creation, same-directory temporary writes followed by rename/replace, bounded JSON reads, and oldest-first cleanup beyond 32 records. Parse Keep-a-Changelog version headings and return bounded complete sections in semantic-version order.

- [ ] **Step 4: Run focused tests**

Run:

```bash
cargo test self_update::tests --lib
```

Expected: all `self_update` unit tests pass.

### Task 2: Extract changelog and persist worker transitions

**Files:**
- Modify: `src/self_update.rs`

- [ ] **Step 1: Write failing archive and worker-script tests**

Extend tar and ZIP fixtures to contain `CHANGELOG.md`. Assert that the extraction result contains exactly one executable and at most one bounded changelog. Assert that Unix and PowerShell scripts mention the status path, write `installing`, `validating`, and `restarting`, and terminate as `succeeded`, `rolled_back`, or `failed`.

```rust
assert_eq!(payload.binary, expected_binary);
assert!(payload.changelog.unwrap().contains("## [2.0.0]"));
assert!(script.contains("write_status restarting"));
assert!(script.contains("write_status succeeded"));
assert!(script.contains("rolled_back"));
```

- [ ] **Step 2: Run worker and archive tests and verify failure**

Run:

```bash
cargo test self_update::tests::extracts self_update::tests::unix_worker self_update::tests::windows_task --lib
```

Expected: failures because extraction and scripts do not yet carry status/changelog data.

- [ ] **Step 3: Implement archive payload extraction and scheduled-record creation**

Replace `extract_binary` with an `ExtractedRelease` result containing `binary` and optional UTF-8 `changelog`. After checksum verification, select the interval text, create a `scheduled` record before scheduling, and include the changelog in `SelfUpdateReceipt`.

```rust
pub struct SelfUpdateReceipt {
    pub status: SelfUpdateStatus,
    pub current_version: String,
    pub target_version: String,
    pub update_id: Option<String>,
    pub service_restart: bool,
    pub log_path: String,
    pub changelog: Option<String>,
}
```

If scheduling fails, atomically mark the record `failed` with code `schedule_failed` before returning the existing tool error.

- [ ] **Step 4: Implement worker transitions on Unix and Windows**

Pass the status record path, source version, and target version into generated scripts. Use script-local atomic JSON writers. Set `installing` before service stop/swap, `validating` before probing, `restarting` before service enable, and terminal state only after rollback/restart handling is known. Increase the response-delivery grace period from 5 to 10 seconds.

- [ ] **Step 5: Run all self-update tests**

Run:

```bash
cargo test self_update::tests tools::self_update::tests --lib
```

Expected: all focused updater tests pass.

### Task 3: App-only polling tool

**Files:**
- Create: `src/tools/self_update_status.rs`
- Modify: `src/tools/mod.rs`
- Modify: `src/registry.rs`
- Modify: `tests/meta_suite.rs`

- [ ] **Step 1: Write failing metadata and behavior tests**

Add tool tests that require an exact closed schema, reject malformed IDs before filesystem access, report `env!("CARGO_PKG_VERSION")` as `runningVersion`, and assert app-only compatibility metadata.

```rust
assert_eq!(meta["ui"]["visibility"], json!(["app"]));
assert_eq!(meta["openai/visibility"], "private");
assert_eq!(meta["openai/widgetAccessible"], true);
```

Update registry count and behavior tables in `tests/meta_suite.rs`, and assert that `self_update_status` is app-only while `self_update` remains model-visible.

- [ ] **Step 2: Run focused tests and verify failure**

Run:

```bash
cargo test tools::self_update_status tests::meta_suite --all
```

Expected: compilation/test failure because the tool is not registered.

- [ ] **Step 3: Implement and register the status tool**

The input is `{ "updateId": string }`. The output mirrors the durable record and adds `runningVersion`. The tool is project-independent and returns bounded errors for unknown, malformed, or corrupt records.

```rust
ToolBehavior::new(
    true,
    false,
    true,
    false,
    "Reads one private Codexify updater record and does not modify files or external state.",
)
```

- [ ] **Step 4: Run tool and metadata tests**

Run:

```bash
cargo test tools::self_update_status --lib
cargo test --test meta_suite
```

Expected: all tests pass.

### Task 4: Updater MCP App and tool result metadata

**Files:**
- Create: `src/self_update_ui.rs`
- Modify: `src/lib.rs`
- Modify: `src/tools/self_update.rs`

- [ ] **Step 1: Write failing UI and result tests**

Test the resource URI/MIME type, model-facing `self_update` metadata, component-only payload, safe changelog rendering, standard handshake, `tools/call` invocation, absolute 60-second deadline, backoff, manual retry, size reporting, and private widget-state persistence.

```rust
assert!(SELF_UPDATE_UI_HTML.contains("ui/initialize"));
assert!(SELF_UPDATE_UI_HTML.contains("tools/call"));
assert!(SELF_UPDATE_UI_HTML.contains("self_update_status"));
assert!(SELF_UPDATE_UI_HTML.contains("60_000"));
assert!(!SELF_UPDATE_UI_HTML.contains("innerHTML"));
```

Extend `tools::self_update` tests so the structured receipt excludes changelog text while result metadata contains it.

- [ ] **Step 2: Run focused tests and verify failure**

Run:

```bash
cargo test self_update_ui tools::self_update::tests --lib
```

Expected: failures because the UI module and metadata are absent.

- [ ] **Step 3: Implement UI metadata and component payload**

Create stable constants and helpers analogous to `diff_ui.rs`:

```rust
pub const SELF_UPDATE_UI_URI: &str = "ui://codexify/self-update/v1/mcp-app.html";
pub const SELF_UPDATE_RESULT_META_KEY: &str = "io.github.devnoname120/codexify/self-update";
```

`self_update` advertises the resource URI, `visibility: ["model"]`, invoking/invoked labels, and widget access compatibility. Its result metadata contains the full receipt and changelog; structured content contains only status, versions, update ID, restart flag, and log path.

- [ ] **Step 4: Implement the embedded app**

Use text nodes only for server-provided strings. Initialize from tool-result metadata, persist `deadlineAt` and terminal state in private widget state, poll `self_update_status` through `tools/call`, tolerate transport errors, and expose a manual retry. Render terminal failure codes as controlled labels and bounded details.

- [ ] **Step 5: Run UI and tool tests**

Run:

```bash
cargo test self_update_ui tools::self_update::tests --lib
```

Expected: all tests pass.

### Task 5: Serve the updater resource

**Files:**
- Modify: `src/server.rs`

- [ ] **Step 1: Extend the server resource test to fail**

Require both resource URIs in `resources/list` and verify that `resources/read` returns the update HTML with the MCP Apps MIME type.

- [ ] **Step 2: Run the focused server test and verify failure**

Run:

```bash
cargo test server::tests::advertises_diff_resources_and_mcp_apps_extension --lib -- --exact
```

Expected: failure because only the diff resource is advertised.

- [ ] **Step 3: Wire resource listing and dispatch**

Advertise `diff_ui::resource()` and `self_update_ui::resource()`. After bridge/artifact resolution, try the diff UI and then update UI before returning `resource_not_found`. Reuse the existing MCP Apps extension capability and MIME type.

- [ ] **Step 4: Run server tests**

Run:

```bash
cargo test server::tests --lib
```

Expected: all server tests pass when rerun individually if an unrelated timing-sensitive test flakes.

### Task 6: Release packaging and documentation

**Files:**
- Modify: `.github/workflows/release.yml`
- Modify: `README.md`
- Modify: `docs/ARCHITECTURE.md`
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Package and verify `CHANGELOG.md`**

Add `CHANGELOG.md` to the staging copy and a shell assertion before archiving:

```bash
cp codexify.config.json CHANGELOG.md LICENSE README.md "${STAGE}/"
test -s "${STAGE}/CHANGELOG.md"
```

- [ ] **Step 2: Document exact behavior and limitations**

Describe the card, app-only status tool, durable record path and states, 60-second non-failure timeout, checksum-bound changelog, 10-second grace period, foreground Unix semantics, and manual ChatGPT connector refresh.

- [ ] **Step 3: Validate workflow syntax and documentation references**

Run:

```bash
ruby -e 'require "yaml"; YAML.load_file(".github/workflows/release.yml", aliases: true); puts "workflow yaml ok"'
rg -n "self_update_status|update/status|CHANGELOG.md|60 seconds|10 seconds" README.md docs/ARCHITECTURE.md CHANGELOG.md .github/workflows/release.yml
```

Expected: YAML parses and every protocol element is documented.

### Task 7: Full verification, review, commit, and push

**Files:**
- All changed files

- [ ] **Step 1: Format and lint**

Run:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
```

Expected: both commands exit 0.

- [ ] **Step 2: Run the complete isolated test suite**

Run:

```bash
REAL_HOME="$HOME"
TMP_HOME=$(mktemp -d)
trap 'rm -rf "$TMP_HOME"' EXIT
HOME="$TMP_HOME" CARGO_HOME="$REAL_HOME/.cargo" RUSTUP_HOME="$REAL_HOME/.rustup" CODEX_HOME="$TMP_HOME/.codex" cargo test --all
```

Expected: all tests pass. Rerun any known timing-sensitive bridge/server test individually before classifying it as a regression.

- [ ] **Step 3: Inspect aggregate diff and repair findings**

Call `show_diff` once after the final related edit. Review security boundaries, script quoting, rollback terminal states, model-visible output size, widget rendering, and release archive contents. Apply any necessary corrections and rerun the affected checks.

- [ ] **Step 4: Commit**

Stage all intended files and create one cohesive commit:

```bash
git add .github/workflows/release.yml CHANGELOG.md README.md docs/ARCHITECTURE.md docs/superpowers/plans/2026-08-31-self-update-progress-widget.md docs/superpowers/specs/2026-08-31-self-update-progress-widget-design.md src/lib.rs src/registry.rs src/self_update.rs src/self_update_ui.rs src/server.rs src/tools/mod.rs src/tools/self_update.rs src/tools/self_update_status.rs tests/meta_suite.rs
git commit -m "Add self-update progress widget"
```

- [ ] **Step 5: Push the feature branch**

Push `feature/updater-progress-widget` to `origin` with normal hooks enabled and report the resulting commit ID and remote branch.
