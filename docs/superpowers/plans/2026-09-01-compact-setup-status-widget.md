# Compact Setup Status Widget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the oversized setup dashboard with compact status rows, explicit update/doctor actions, structured background diagnostics, and conversational Refresh/Autofix handoffs.

**Architecture:** Add one app-only `check_for_updates` tool backed by a force-refresh variant of the existing latest-release inspection. Return structured `DoctorReport` data from the app-only doctor tool. Refactor the setup widget into a small client-side state machine that renders setup state immediately, starts doctor asynchronously after initialization, and sends `ui/message` requests with ChatGPT compatibility fallbacks for Refresh and Autofix. Remove obsolete connector-settings URL discovery/configuration because link construction moves into the follow-up prompt.

**Tech Stack:** Rust, Tokio, serde/serde_json, rmcp tool metadata, embedded HTML/CSS/JavaScript, MCP Apps JSON-RPC (`tools/call`, `ui/message`, size notifications), ChatGPT `window.openai` compatibility APIs, Cargo tests.

---

## File map

- Create `src/tools/check_for_updates.rs`: shared UI-facing latest-version result types, schema, serialization, and the app-only manual update-check tool.
- Modify `src/self_update.rs`: expose a force-refresh latest-version inspection while preserving the existing cache for setup/doctor.
- Modify `src/tools/setup.rs`: reuse the shared update output, remove settings URL output/plumbing, retain model-only `nextStep`.
- Modify `src/tools/doctor.rs`: advertise and return structured `DoctorReport` data.
- Modify `src/setup_ui.rs`: compact rows, background doctor, colored diagnostics, Upgrade/Refresh/Autofix, `ui/message`, and responsive sizing.
- Modify `src/tools/mod.rs` and `src/registry.rs`: register the new app-only tool.
- Modify `src/tool.rs`, `src/server.rs`, `src/types.rs`, `src/config.rs`, `src/tools/import_host_file.rs`, `tests/tools_core.rs`: remove obsolete connector-ID/settings-URL plumbing.
- Modify `tests/meta_suite.rs`: pin the new tool count, annotations, visibility, names, and structured-content contract.
- Modify `README.md`, `docs/ARCHITECTURE.md`, `CHANGELOG.md`, `codexify.config.json`: document the new interaction model and remove the obsolete setting.

---

### Task 1: Add a force-refresh latest-version path

**Files:**
- Modify: `src/self_update.rs:284-310`
- Test: `src/self_update.rs` unit tests near existing latest-version inspection tests

- [ ] **Step 1: Write a failing cache-policy unit test**

Add an internal helper that accepts `force_refresh` and test that a forced call skips a fresh cached result while an ordinary call reuses it. Keep network functions injectable in the test.

```rust
#[tokio::test]
async fn forced_latest_version_check_bypasses_a_fresh_cache_entry() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cache = tokio::sync::Mutex::new(Some(CachedLatestVersionInspection {
        checked_at: Instant::now(),
        result: Ok(LatestVersionInspection {
            status: LatestVersionStatus::UpToDate,
            current: Version::new(1, 0, 0),
            latest: Version::new(1, 0, 0),
            source: LatestVersionSource::GithubApi,
        }),
    }));

    let result = inspect_latest_version_cached_with(&cache, true, {
        let calls = calls.clone();
        move || async move {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(LatestVersionInspection {
                status: LatestVersionStatus::UpdateAvailable,
                current: Version::new(1, 0, 0),
                latest: Version::new(1, 1, 0),
                source: LatestVersionSource::GithubCli,
            })
        }
    })
    .await
    .unwrap();

    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(result.latest, Version::new(1, 1, 0));
}
```

- [ ] **Step 2: Run the new test and verify failure**

Run:

```bash
cargo test self_update::tests::forced_latest_version_check_bypasses_a_fresh_cache_entry -- --exact
```

Expected: compilation failure because `inspect_latest_version_cached_with` does not exist.

- [ ] **Step 3: Implement cached and forced inspection entry points**

Refactor the existing function around a shared helper:

```rust
async fn inspect_latest_version_cached_with<F, Fut>(
    cache: &tokio::sync::Mutex<Option<CachedLatestVersionInspection>>,
    force_refresh: bool,
    inspect: F,
) -> anyhow::Result<LatestVersionInspection>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<LatestVersionInspection, String>>,
{
    let mut cache = cache.lock().await;
    if !force_refresh {
        if let Some(cached) = cache.as_ref() {
            let ttl = if cached.result.is_ok() {
                LATEST_VERSION_CACHE_TTL
            } else {
                LATEST_VERSION_ERROR_CACHE_TTL
            };
            if cached.checked_at.elapsed() < ttl {
                return cached.result.clone().map_err(anyhow::Error::msg);
            }
        }
    }

    let result = inspect().await;
    *cache = Some(CachedLatestVersionInspection {
        checked_at: Instant::now(),
        result: result.clone(),
    });
    result.map_err(anyhow::Error::msg)
}

async fn inspect_latest_version_with_cache(force_refresh: bool) -> anyhow::Result<LatestVersionInspection> {
    let cache = LATEST_VERSION_CACHE.get_or_init(|| tokio::sync::Mutex::new(None));
    inspect_latest_version_cached_with(cache, force_refresh, || async {
        inspect_latest_version_with(
            env!("CARGO_PKG_VERSION"),
            || latest_release_tag_via_gh(GH_LATEST_VERSION_CHECK_TIMEOUT),
            || latest_release_tag_via_api(ReleaseSource::default(), LATEST_VERSION_CHECK_TIMEOUT),
        )
        .await
        .map_err(|error| format!("{error:#}"))
    })
    .await
}

pub async fn inspect_latest_version() -> anyhow::Result<LatestVersionInspection> {
    inspect_latest_version_with_cache(false).await
}

pub async fn refresh_latest_version() -> anyhow::Result<LatestVersionInspection> {
    inspect_latest_version_with_cache(true).await
}
```

- [ ] **Step 4: Run latest-version tests**

Run:

```bash
cargo test self_update::tests:: --lib
```

Expected: all self-update unit tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/self_update.rs
git commit -m "refactor: support forced update checks"
```

---

### Task 2: Add the app-only manual update-check tool

**Files:**
- Create: `src/tools/check_for_updates.rs`
- Modify: `src/tools/setup.rs`
- Modify: `src/tools/mod.rs`
- Modify: `src/registry.rs`
- Test: `src/tools/check_for_updates.rs`
- Test: `src/tools/setup.rs`
- Test: `tests/meta_suite.rs`

- [ ] **Step 1: Write failing registry and contract tests**

Update counts in `tests/meta_suite.rs` from 32/34/31/33/35 to 33/35/32/34/36 respectively. Add `check_for_updates` to the expected tool names and behavior map as `(true, false, true, true)`. Add it to the list of tools that must build structured content.

Add a focused test:

```rust
#[test]
fn check_for_updates_is_private_app_only() {
    let tool = codexify::tools::check_for_updates::CheckForUpdates;
    let meta = tool.meta().unwrap();
    assert_eq!(
        meta.get("ui").and_then(|value| value.get("visibility")),
        Some(&json!(["app"]))
    );
    assert_eq!(meta.get("openai/visibility"), Some(&json!("private")));
    assert_eq!(meta.get("openai/widgetAccessible"), Some(&json!(true)));
}
```

- [ ] **Step 2: Run the registry test and verify failure**

Run:

```bash
cargo test --test meta_suite loads_all_33_tools_including_app_only_diagnostics
```

Expected: failure because the tool is not registered and the count is still 32.

- [ ] **Step 3: Implement the shared update output and tool**

Create `src/tools/check_for_updates.rs` with:

```rust
use async_trait::async_trait;
use rmcp::model::MetaObject;
use serde::Serialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::self_update::{LatestVersionInspection, LatestVersionSource, LatestVersionStatus};
use crate::tool::{Tool, ToolBehavior, empty_object_schema};
use crate::types::{AppConfig, ToolResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UpdateCheckStatus {
    UpdateAvailable,
    UpToDate,
    AheadOfLatest,
    CheckFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateCheckOutput {
    pub(crate) status: UpdateCheckStatus,
    pub(crate) current_version: String,
    pub(crate) latest_version: Option<String>,
    pub(crate) source: Option<LatestVersionSource>,
    pub(crate) detail: Option<String>,
}

fn compact_error(error: &str) -> String {
    error
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(512)
        .collect()
}

pub(crate) fn output_from_result(
    result: Result<LatestVersionInspection, String>,
) -> UpdateCheckOutput {
    match result {
        Ok(inspection) => UpdateCheckOutput {
            status: match inspection.status {
                LatestVersionStatus::UpdateAvailable => UpdateCheckStatus::UpdateAvailable,
                LatestVersionStatus::UpToDate => UpdateCheckStatus::UpToDate,
                LatestVersionStatus::AheadOfLatest => UpdateCheckStatus::AheadOfLatest,
            },
            current_version: inspection.current.to_string(),
            latest_version: Some(inspection.latest.to_string()),
            source: Some(inspection.source),
            detail: None,
        },
        Err(error) => UpdateCheckOutput {
            status: UpdateCheckStatus::CheckFailed,
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            latest_version: None,
            source: None,
            detail: Some(compact_error(&error)),
        },
    }
}

pub(crate) fn output_schema_value() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": {
                "type": "string",
                "enum": ["update_available", "up_to_date", "ahead_of_latest", "check_failed"]
            },
            "currentVersion": { "type": "string" },
            "latestVersion": {
                "anyOf": [{ "type": "string" }, { "type": "null" }]
            },
            "source": {
                "anyOf": [
                    { "type": "string", "enum": ["github_cli", "github_api"] },
                    { "type": "null" }
                ]
            },
            "detail": {
                "anyOf": [{ "type": "string" }, { "type": "null" }]
            }
        },
        "required": ["status", "currentVersion", "latestVersion", "source", "detail"],
        "additionalProperties": false
    })
}

pub struct CheckForUpdates;
```

Implement `Tool` with name `check_for_updates`, app-only/private metadata, an empty input schema, the shared output schema, `requires_project_root() == false`, and `call()` using `crate::self_update::refresh_latest_version()`.

Return a concise compatibility text plus `serde_json::to_value(output)` as structured content.

- [ ] **Step 4: Reuse the shared shape in setup**

Remove `SetupUpdateStatus`, `SetupUpdateInfo`, and the local `compact_error` from `src/tools/setup.rs`. Import:

```rust
use crate::tools::check_for_updates::{
    UpdateCheckOutput, UpdateCheckStatus, output_from_result, output_schema_value,
};
```

Build setup update state with:

```rust
let update = output_from_result(update_result);
```

Use `output_schema_value()` for the nested `update` property.

- [ ] **Step 5: Register the tool**

Add `pub mod check_for_updates;` to `src/tools/mod.rs` and insert `Box::new(tools::check_for_updates::CheckForUpdates)` next to doctor/self-update in `src/registry.rs`. Adjust the fixed array length.

- [ ] **Step 6: Run focused tests**

Run:

```bash
cargo test tools::check_for_updates:: --lib
cargo test tools::setup:: --lib
cargo test --test meta_suite
```

Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add src/self_update.rs src/tools/check_for_updates.rs src/tools/setup.rs src/tools/mod.rs src/registry.rs tests/meta_suite.rs
git commit -m "feat: add manual update checks"
```

---

### Task 3: Return structured doctor reports

**Files:**
- Modify: `src/tools/doctor.rs`
- Test: `src/tools/doctor.rs`
- Test: `tests/meta_suite.rs`

- [ ] **Step 1: Write a failing structured-output test**

Add a helper that converts a supplied report to a tool result and validate it:

```rust
#[test]
fn doctor_result_matches_its_structured_schema() {
    let report = crate::doctor::DoctorReport::new(vec![
        crate::doctor::DoctorCheck::pass("runtime", "usable"),
        crate::doctor::DoctorCheck::warning("release", "unavailable")
            .with_detail("offline")
            .with_remediation("retry later"),
    ]);
    let result = Doctor::result(report.clone());
    let structured = result.structured_content.as_ref().unwrap();
    let validator = jsonschema::options()
        .build(&Doctor::output_schema_value())
        .unwrap();

    assert!(validator.is_valid(structured));
    assert_eq!(structured["summary"]["passed"], 1);
    assert_eq!(structured["summary"]["warnings"], 1);
    assert_eq!(structured["checks"][1]["status"], "warning");
    assert!(result.joined_text().contains("WARN release"));
}
```

- [ ] **Step 2: Run the test and verify failure**

Run:

```bash
cargo test tools::doctor::tests::doctor_result_matches_its_structured_schema --lib
```

Expected: compilation failure because the helper/schema do not exist.

- [ ] **Step 3: Implement the exact doctor output schema**

Replace `text_output_schema()` with an object schema containing:

```rust
{
    "ok": boolean,
    "version": string,
    "platform": { "os": string, "arch": string },
    "checks": [{
        "id": string,
        "status": "pass" | "warning" | "failure" | "skipped",
        "summary": string,
        "detail": string | null,
        "remediation": string | null
    }],
    "summary": {
        "passed": integer >= 0,
        "warnings": integer >= 0,
        "failures": integer >= 0,
        "skipped": integer >= 0
    }
}
```

Close every nested object with `additionalProperties: false`.

Add:

```rust
fn result(report: crate::doctor::DoctorReport) -> ToolResult {
    let text = report.render_human();
    ToolResult::text(text).with_structured(
        serde_json::to_value(report).expect("doctor report must serialize"),
    )
}
```

Set `fills_structured_content()` to `false` and call `Doctor::result(run_for_config(...).await)`.

- [ ] **Step 4: Update structured-content pinning**

Add `doctor` to `tools_that_need_their_own_structured_content` in `tests/meta_suite.rs`.

- [ ] **Step 5: Run focused tests**

Run:

```bash
cargo test tools::doctor:: --lib
cargo test --test meta_suite
```

Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/tools/doctor.rs tests/meta_suite.rs
git commit -m "feat: expose structured doctor reports"
```

---

### Task 4: Remove obsolete connector-settings plumbing

**Files:**
- Modify: `src/setup_ui.rs`
- Modify: `src/tools/setup.rs`
- Modify: `src/tool.rs`
- Modify: `src/server.rs`
- Modify: `src/types.rs`
- Modify: `src/config.rs`
- Modify: `src/tools/import_host_file.rs`
- Modify: `tests/tools_core.rs`
- Test: `src/tools/setup.rs`
- Test: `src/server.rs`
- Test: `src/config.rs`

- [ ] **Step 1: Update failing tests to the new contract**

In setup tests, remove `settingsUrl` expectations and the request-connector-ID test. Assert that the connector schema requires only:

```rust
[
    "status",
    "advertisedVersion",
    "observedVersion",
    "refreshRecommended"
]
```

Replace the config test with one that confirms the historical `chatgptConnectorSettingsUrl` key is ignored rather than exposed in `AppConfig`, preserving the existing permissive unknown-field behavior.

Delete the server metadata-discovery test.

- [ ] **Step 2: Run focused tests and verify failure**

Run:

```bash
cargo test tools::setup:: --lib
cargo test config::tests:: --lib
cargo test server::tests::connector_id_is_feature_detected_from_request_metadata --lib
```

Expected: setup/config assertions fail and the old server test still exists.

- [ ] **Step 3: Remove output/config fields**

Remove:

```rust
ConnectorSchemaInfo::settings_url
AppConfig::chatgpt_connector_settings_url
FileConfig::chatgpt_connector_settings_url
resolve_chatgpt_connector_settings_url
```

Remove the corresponding default and load-time assignments. Because `FileConfig` does not deny unknown fields, old configs containing the property continue to load and the unused key is ignored.

- [ ] **Step 4: Remove request connector-ID extraction**

Delete `connector_id_from_request_meta`, the request/context lookup in `server.rs`, and `ToolRequestContext::connector_id`. Remove `connector_id: None` from all test/fixture constructors.

- [ ] **Step 5: Remove setup URL construction**

Delete `connector_settings_url()` and its tests from `src/setup_ui.rs`. Remove `settings_url` from `setup_result` and `call_with_context_and_update_check`. Replace the model-facing stale-schema suffix with:

```rust
text.push_str(
    " ChatGPT's cached Codexify connector schema could not be confirmed as current. The setup panel offers a Refresh action that can provide the correct settings link and instructions.",
);
```

- [ ] **Step 6: Run focused tests**

Run:

```bash
cargo test tools::setup:: --lib
cargo test config::tests:: --lib
cargo test server::tests:: --lib
cargo test --test tools_core
```

Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add src/setup_ui.rs src/tools/setup.rs src/tool.rs src/server.rs src/types.rs src/config.rs src/tools/import_host_file.rs tests/tools_core.rs
git commit -m "refactor: remove connector settings URL plumbing"
```

---

### Task 5: Implement the compact widget state machine

**Files:**
- Modify: `src/setup_ui.rs`
- Test: `src/setup_ui.rs`

- [ ] **Step 1: Replace static HTML assertions with failing UX-contract assertions**

Update the setup UI resource test to require:

```rust
for expected in [
    "check_for_updates",
    "self_update",
    "doctor",
    "ui/message",
    "window.openai.sendFollowUpMessage",
    "Check for updates",
    "Upgrade",
    "Refresh",
    "Autofix",
    "plugin://dev-<slug>@...",
    "#settings/Plugins/plugin_asdk_app_<slug>",
    "#settings/Plugins",
    "scroll below the list of tools",
] {
    assert!(text.contains(expected), "missing {expected}");
}
assert!(!text.contains("data.nextStep"));
assert!(!text.contains("Connector status and diagnostics"));
assert!(!text.contains("openExternal"));
```

- [ ] **Step 2: Run the resource test and verify failure**

Run:

```bash
cargo test setup_ui::tests::setup_resource_contains_compact_actions_and_follow_up_prompts --lib
```

Expected: failure because the old dashboard and settings navigation remain.

- [ ] **Step 3: Implement bridge helpers**

Retain the existing JSON-RPC request map and tool-call fallback. Add structured-result and message helpers:

```javascript
function structuredPayload(value, predicate) {
  return objectFrom(value, candidate => {
    const payload = candidate.structuredContent || candidate.structured_content;
    return payload && typeof payload === "object" && predicate(payload) ? payload : null;
  });
}

async function sendFollowUpMessage(prompt) {
  if (initialized) {
    try {
      return await request("ui/message", {
        role: "user",
        content: { type: "text", text: prompt }
      });
    } catch (error) {
      if (!(window.openai && typeof window.openai.sendFollowUpMessage === "function")) {
        throw error;
      }
    }
  }
  if (window.openai && typeof window.openai.sendFollowUpMessage === "function") {
    return window.openai.sendFollowUpMessage({ prompt, scrollToBottom: true });
  }
  throw new Error("This host does not expose follow-up messages.");
}
```

- [ ] **Step 4: Implement widget state**

Track:

```javascript
let currentData = null;
let currentMetadata = null;
let updateState = null;
let doctorState = {
  phase: "idle",
  report: null,
  error: null,
  expanded: false,
  manual: false
};
let actionState = { kind: null, message: null, tone: null };
let automaticDoctorStarted = false;
```

When setup data arrives, initialize `updateState = data.update`, render immediately, and call `maybeStartAutomaticDoctor()` after both setup data and `ui/initialize` are available.

- [ ] **Step 5: Implement compact rendering**

Use a single card with `max-width: 500px`, no title/header, and row-local actions:

```text
Codexify: v1.2.0 ✓                    [Check for updates]
Connector schema: v1.2.0 ✓
[Doctor]
```

For an update:

```text
Codexify: v1.2.0 → v1.3.0 available  [Upgrade] [Check for updates]
```

For stale schema:

```text
Connector schema: v1.1.0 · refresh required  [Refresh]
```

Use green/amber/red/muted status classes and text labels; do not rely on color alone.

- [ ] **Step 6: Implement update actions**

`Check for updates` sets a row-local checking state, calls `check_for_updates`, extracts the shared structured output, replaces `updateState`, records timing, and rerenders. Errors become `check_failed` without discarding the running version.

`Upgrade` calls `self_update` with `{ confirm: true }`, displays concise scheduling/error text, and does not duplicate restart monitoring.

- [ ] **Step 7: Implement automatic and manual doctor behavior**

Use one function:

```javascript
async function runDoctor(manual) {
  doctorState = {
    phase: "running",
    report: doctorState.report,
    error: null,
    expanded: manual || doctorState.expanded,
    manual
  };
  renderCurrent();
  const startedAt = performance.now();
  try {
    const result = await callTool("doctor", {});
    const report = structuredPayload(result, payload =>
      Array.isArray(payload.checks) && payload.summary && typeof payload.ok === "boolean"
    );
    if (!report) throw new Error("Doctor returned no structured report.");
    captureTiming("doctor", result, startedAt);
    doctorState = {
      phase: "ready",
      report,
      error: null,
      expanded: manual || Number(report.summary.failures || 0) > 0,
      manual
    };
  } catch (error) {
    doctorState = {
      phase: "error",
      report: null,
      error: error && error.message ? error.message : String(error),
      expanded: true,
      manual
    };
  }
  renderCurrent();
}
```

Automatic healthy results render no doctor panel. Warning-only results render a compact summary and Autofix. Failures auto-expand warning/failure checks. Manual results render all checks, including healthy reports.

- [ ] **Step 8: Implement Autofix and Refresh prompts**

Build the Autofix prompt from only warning/failure checks, preserving ID, summary, detail, and remediation. Bound individual fields and total prompt length.

Use this Refresh prompt text:

```text
The Codexify connector schema is outdated and needs to be refreshed.

Build a clickable relative ChatGPT settings link for the user:
- If the current conversation context exposes a connector URI containing `plugin://dev-<slug>@...`, extract `<slug>` and use exactly `#settings/Plugins/plugin_asdk_app_<slug>`.
- Otherwise use exactly `#settings/Plugins`.
- Never invent or guess a slug.

Tell the user to open that link, select the Codexify plugin if necessary, scroll below the list of tools, and click Refresh. Keep the response concise.
```

Both buttons call `sendFollowUpMessage`, disable while pending, and report rejection/transport errors inline without removing the underlying state.

- [ ] **Step 9: Implement sizing, responsiveness, and debug timing**

Keep the existing size notification, debug metadata extraction, and timing footer. Use a fluid layout below 500 px; wrap row actions under status text on narrow hosts. Call `reportSize()` after every asynchronous state transition.

- [ ] **Step 10: Run setup UI tests**

Run:

```bash
cargo test setup_ui::tests:: --lib
cargo test tools::setup:: --lib
```

Expected: all pass.

- [ ] **Step 11: Commit**

```bash
git add src/setup_ui.rs
git commit -m "feat: compact the setup status widget"
```

---

### Task 6: Update documentation and checked-in defaults

**Files:**
- Modify: `README.md`
- Modify: `docs/ARCHITECTURE.md`
- Modify: `CHANGELOG.md`
- Modify: `codexify.config.json`

- [ ] **Step 1: Update the checked-in config**

Remove:

```json
"chatgptConnectorSettingsUrl": null
```

Keep valid JSON formatting.

- [ ] **Step 2: Rewrite setup-widget documentation**

Document:

- compact Codexify/schema rows;
- persistent `Check for updates` and `Doctor` actions;
- row-local `Upgrade` and `Refresh` actions;
- background private doctor call;
- structured colored warnings/failures;
- Autofix and Refresh as user-initiated `ui/message` handoffs;
- no user-visible `nextStep` text;
- removal of `chatgptConnectorSettingsUrl`.

- [ ] **Step 3: Add an Unreleased changelog entry**

Add under `[Unreleased]`:

```markdown
### Changed

- The setup MCP App now uses compact status rows, supports an explicit cached-bypass update check, runs structured doctor diagnostics in the background, surfaces warnings/failures with Autofix, and delegates stale connector refresh instructions to a ChatGPT follow-up message. The obsolete `chatgptConnectorSettingsUrl` setting and connector-ID metadata probing were removed.
```

- [ ] **Step 4: Verify stale references are gone**

Run:

```bash
rg -n 'chatgptConnectorSettingsUrl|chatgpt_connector_settings_url|connector_settings_url|connector_id_from_request_meta' .
```

Expected: no matches outside historical release notes if those notes intentionally remain unchanged.

- [ ] **Step 5: Commit**

```bash
git add README.md docs/ARCHITECTURE.md CHANGELOG.md codexify.config.json
git commit -m "docs: describe compact setup diagnostics"
```

---

### Task 7: Verify, inspect, and publish

**Files:**
- Verify all modified files

- [ ] **Step 1: Format**

Run:

```bash
cargo fmt --all -- --check
```

If it fails, run `cargo fmt --all`, then rerun the check.

- [ ] **Step 2: Run focused suites**

Run:

```bash
cargo test tools::check_for_updates:: --lib
cargo test tools::doctor:: --lib
cargo test tools::setup:: --lib
cargo test setup_ui::tests:: --lib
cargo test --test meta_suite
cargo test --test tools_core
```

Expected: all pass.

- [ ] **Step 3: Run the complete suite**

Run:

```bash
cargo test --all-targets
```

Expected: all tests pass.

- [ ] **Step 4: Build a release binary**

Run:

```bash
cargo build --release
```

Expected: successful release build.

- [ ] **Step 5: Inspect repository state and aggregate diff**

Run:

```bash
git status --short
git diff --check
```

Then call Codexify `show_diff` once for the complete project-scoped diff. The only remaining untracked path may be the `.superpowers/` visual-companion scratch directory; delete that directory before publishing because it was created by this conversation and is not product source.

- [ ] **Step 6: Verify the default branch relationship**

Run:

```bash
git fetch origin
git merge-base --is-ancestor origin/main HEAD
git log --oneline --decorate origin/main..HEAD
```

Expected: `origin/main` is an ancestor of `HEAD`, and the listed commits are only the approved design/implementation commits.

- [ ] **Step 7: Push to the default branch**

Run:

```bash
git push origin HEAD:main
```

Expected: a fast-forward update of `origin/main`.
