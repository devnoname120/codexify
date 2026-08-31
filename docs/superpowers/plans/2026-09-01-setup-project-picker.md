# Setup Project Picker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend Codexify's setup card with a searchable multi-project chooser and a durable, immutable **Chat without a project** choice while preserving all existing project isolation and worktree behavior.

**Architecture:** Add a tri-state binding snapshot (`unselected`, `project`, `without project`) around the existing conversation and transport binding implementations. Persist ChatGPT no-project choices in a separate versioned marker guarded by the same per-conversation lock as project bindings. Extend `set_project_root`, setup structured output, and the existing MCP App; no new public tool is required.

**Tech Stack:** Rust 2024, Tokio, Serde/JSON Schema, RMCP MCP Apps metadata, embedded HTML/CSS/JavaScript, Cargo tests.

---

## File map

- `src/project_bindings.rs` — durable conversation binding state, no-project marker, immutable selection APIs.
- `src/exec_sessions.rs` — equivalent transport-scoped tri-state binding.
- `src/tools/set_project_root.rs` — union input and common project/no-project receipt.
- `src/tool.rs`, `src/server.rs` — expose the shared binding store to setup tool calls.
- `src/tools/setup.rs` — emit current project state and state-specific next steps.
- `src/setup_ui.rs` — searchable chooser and selected-state rendering.
- `tests/project_selection.rs` — end-to-end binding semantics.
- `tests/meta_suite.rs` and existing module tests — schema/registry compatibility.
- `README.md`, `docs/ARCHITECTURE.md`, `CHANGELOG.md` — user and implementation contracts.

### Task 1: Durable conversation and transport no-project state

**Files:**
- Modify: `src/project_bindings.rs`
- Modify: `src/exec_sessions.rs`
- Test: `tests/project_selection.rs`
- Test: `src/project_bindings.rs`
- Test: `src/exec_sessions.rs`

- [ ] **Step 1: Write failing conversation-state tests**

Add tests that use the desired API:

```rust
let selected = store
    .select_without_project(&config, &identity)
    .await
    .unwrap();
assert!(selected.newly_selected);
assert!(matches!(
    store.binding_state(&config, &identity).unwrap(),
    ProjectBindingState::WithoutProject { .. }
));
assert!(store.effective_config(&config, &identity).unwrap_err().contains("without a project"));
```

Recreate the store and assert the marker survives. Assert repeating no-project is idempotent and selecting a project afterward is rejected. Add the inverse test: a selected project rejects no-project mode.

- [ ] **Step 2: Write failing transport-state tests**

```rust
let selected = session.select_without_project(&config).await.unwrap();
assert!(selected.newly_selected);
assert!(matches!(
    session.binding_state(&config).unwrap(),
    ProjectBindingState::WithoutProject { .. }
));
assert!(session.effective_config(&config).unwrap_err().contains("without a project"));
```

Assert idempotence and immutable switching in both directions.

- [ ] **Step 3: Write the concurrent winner test**

Start one project selection and one no-project selection for the same ChatGPT identity behind a barrier. Assert exactly one succeeds and the persisted state matches the winner.

- [ ] **Step 4: Run the tests and verify RED**

Run:

```bash
cargo test --test project_selection without_project -- --nocapture
```

Expected: compilation failures for the missing `ProjectBindingState`, `select_without_project`, and `binding_state` APIs.

- [ ] **Step 5: Implement the conversation state**

Add public state and receipt types:

```rust
pub enum ProjectBindingState {
    Unselected { access_root: PathBuf, scope: ProjectBindingScope },
    Project(ProjectRootSelection),
    WithoutProject { access_root: PathBuf, scope: ProjectBindingScope },
}

pub struct WithoutProjectSelection {
    pub access_root: PathBuf,
    pub newly_selected: bool,
    pub scope: ProjectBindingScope,
}
```

Persist a versioned no-project marker at a distinct extension under the same access-root/identity namespace. Read both records under one state helper, reject a dual-record inconsistency, and acquire the existing project-binding lock for both mutations.

- [ ] **Step 6: Implement the transport state**

Replace the optional transport project record with an optional enum carrying either the existing project placement or `WithoutProject`. Add `binding_state` and `select_without_project`; keep project validation unchanged.

- [ ] **Step 7: Run focused tests and verify GREEN**

```bash
cargo test --test project_selection without_project -- --nocapture
cargo test project_bindings::tests exec_sessions::tests --lib
```

Expected: all selected tests pass.

- [ ] **Step 8: Commit**

```bash
git add src/project_bindings.rs src/exec_sessions.rs tests/project_selection.rs
git commit -m "feat: add explicit no-project bindings"
```

### Task 2: Extend `set_project_root`

**Files:**
- Modify: `src/tools/set_project_root.rs`
- Test: `tests/project_selection.rs`
- Test: `tests/meta_suite.rs`

- [ ] **Step 1: Write failing input/output tests**

Assert the schema accepts exactly one of:

```json
{ "path": "alpha" }
```

and:

```json
{ "withoutProject": true }
```

Reject `{}`, both fields, `withoutProject: false`, and an empty path. Assert project results contain `mode: "project"`, a derived `project_name`, and the active path. Assert no-project results contain `mode: "without_project"`, `project_name: "Chat without a project"`, and null placement fields.

- [ ] **Step 2: Run and verify RED**

```bash
cargo test --test project_selection set_project_root -- --nocapture
```

Expected: failures because the no-project input and common receipt do not exist.

- [ ] **Step 3: Implement the union request**

Use one manually validated request structure:

```rust
enum ProjectSelectionRequest {
    Project(String),
    WithoutProject,
}
```

Publish a closed JSON Schema with a root `oneOf`, preserving the existing `path` form and adding `withoutProject` with `const: true`.

- [ ] **Step 4: Implement the common selection result**

Dispatch the request to the conversation store in `server.rs` or the transport session in the tool fallback. Render one structured shape with a required `mode`, nullable placement fields, and existing scope/worktree information.

- [ ] **Step 5: Run focused tests and verify GREEN**

```bash
cargo test --test project_selection
cargo test meta_suite --test meta_suite
```

- [ ] **Step 6: Commit**

```bash
git add src/tools/set_project_root.rs src/server.rs tests/project_selection.rs tests/meta_suite.rs
git commit -m "feat: let set_project_root choose no-project mode"
```

### Task 3: Add project state to setup

**Files:**
- Modify: `src/tool.rs`
- Modify: `src/server.rs`
- Modify: `src/tools/setup.rs`
- Modify: `src/tools/import_host_file.rs`
- Modify: `tests/tools_core.rs`
- Test: `src/tools/setup.rs`

- [ ] **Step 1: Write failing setup-state tests**

Construct static, unselected, selected, and no-project configurations and assert:

```rust
assert_eq!(structured["project"]["status"], "unselected");
assert_eq!(structured["project"]["selectionAvailable"], true);
```

For a selected managed worktree, assert `activePath` is the worktree project path, `sourcePath` is the source checkout, and `managedWorktree` is true. For no-project, assert the name and no active path. Add a state-read failure fixture that produces `check_failed` without failing setup authorization.

- [ ] **Step 2: Run and verify RED**

```bash
cargo test tools::setup::tests --lib
```

Expected: missing `project` output and binding-store context.

- [ ] **Step 3: Extend request context**

Add `Arc<ProjectBindingStore>` to `ToolRequestContext`, populate it in `server.rs`, and update every test fixture that constructs the context.

- [ ] **Step 4: Emit project state**

Add `SetupProjectInfo` to the setup output schema. Resolve a static project in single-project mode; otherwise query the conversation store or transport session after authorization. Derive a bounded display name from the source project path and produce state-specific `nextStep` text.

- [ ] **Step 5: Run focused tests and verify GREEN**

```bash
cargo test tools::setup::tests --lib
cargo test --test tools_core
```

- [ ] **Step 6: Commit**

```bash
git add src/tool.rs src/server.rs src/tools/setup.rs src/tools/import_host_file.rs tests/tools_core.rs
git commit -m "feat: report project state from setup"
```

### Task 4: Build the searchable setup chooser

**Files:**
- Modify: `src/setup_ui.rs`
- Test: `src/setup_ui.rs`

- [ ] **Step 1: Write failing widget contract tests**

Require the embedded resource to contain:

```text
Chat without a project
list_projects
set_project_root
withoutProject
project-search
project_root
source_project_root
managed_worktree
```

Also retain the existing assertion that server strings are not assigned through `innerHTML`.

- [ ] **Step 2: Run and verify RED**

```bash
cargo test setup_ui::tests --lib
```

Expected: missing project-picker strings and flows.

- [ ] **Step 3: Add project payload parsers**

Parse setup project state, `list_projects` structured output, and `set_project_root` structured output through the existing nested-result walker. Add explicit tool-error detection.

- [ ] **Step 4: Add chooser presentation**

Place the project section before the status grid. For `unselected`, render the fixed no-project option, search input, loading/error state, result count, and project rows. Query `list_projects` initially and after a short debounce; discard stale responses with a monotonic request generation.

- [ ] **Step 5: Add selection transitions**

Call:

```javascript
callTool("set_project_root", { path: project.selector })
callTool("set_project_root", { withoutProject: true })
```

On success, normalize the receipt into setup project state and call the existing `render` function. Display **Worktree** for managed active paths, **Path** otherwise, and source checkout as secondary context.

- [ ] **Step 6: Run focused tests and verify GREEN**

```bash
cargo test setup_ui::tests tools::setup::tests --lib
```

- [ ] **Step 7: Commit**

```bash
git add src/setup_ui.rs src/tools/setup.rs
git commit -m "feat: add project chooser to setup card"
```

### Task 5: Documentation and aggregate verification

**Files:**
- Modify: `README.md`
- Modify: `docs/ARCHITECTURE.md`
- Modify: `CHANGELOG.md`
- Modify: `docs/superpowers/plans/2026-09-01-setup-project-picker.md`

- [ ] **Step 1: Document the user flow**

Describe automatic/model selection when intent is exact, the setup-card chooser when it is ambiguous, server-side search, selected worktree path display, and immutable no-project semantics.

- [ ] **Step 2: Document architecture and security**

Document the tri-state binding, separate marker, shared lock, and the fact that no-project mode never maps to the access root.

- [ ] **Step 3: Run formatting and focused suites**

```bash
cargo fmt --all -- --check
cargo test --test project_selection -- --test-threads=1
cargo test tools::setup::tests setup_ui::tests --lib -- --test-threads=1
```

- [ ] **Step 4: Run the complete gate**

```bash
cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets --all-features -- -D warnings
git diff --check
```

Expected: zero failures and zero warnings.

- [ ] **Step 5: Present and review the aggregate diff**

Call `show_diff` once after the final related file change, then inspect `git diff --stat`, `git diff --check`, and the sensitive binding/UI portions.

- [ ] **Step 6: Commit implementation and docs**

```bash
git add CHANGELOG.md README.md docs/ARCHITECTURE.md docs/superpowers/plans/2026-09-01-setup-project-picker.md
git commit -m "docs: document setup project picker"
```

- [ ] **Step 7: Rebase and publish safely**

Fetch `origin/main`, rebase the feature branch, rerun the focused verification if the base changed, and update `main` only with `--force-with-lease` or a fast-forward-safe push. Verify GitHub's main SHA and tree, then delete the feature branch if publication succeeds.
