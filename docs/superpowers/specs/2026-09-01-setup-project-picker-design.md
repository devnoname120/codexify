# Setup project picker design

## Goal

When Codexify runs in multi-project mode and the current ChatGPT conversation or generic MCP transport has not chosen a project, the existing setup MCP App must offer an in-card project picker rather than relying only on model-mediated clarification. The picker must provide server-side search, an explicit **Chat without a project** choice at the top, and an immutable project/no-project decision scoped exactly like the existing project binding.

After a project is selected, the setup card must replace the picker with the selected project name and effective active path. When worktree isolation created a managed checkout, the displayed primary path is the active worktree project path; the source checkout path is secondary context. After **Chat without a project** is selected, the card must show that state and the user may continue using ChatGPT and Codexify's project-independent tools without being prompted to choose a project again.

## State model

Multi-project conversations have three distinct states:

1. **Unselected** — no choice has been made. Project-scoped tools remain unavailable and the setup card renders the picker.
2. **Project selected** — the existing immutable project binding is active. Its effective project path may be the source checkout or a managed worktree.
3. **Without project** — the user explicitly chose to continue without a project. This is an immutable choice for the conversation or transport session, just like selecting a project.

Single-project mode is represented to the setup card as an already-selected static project and never renders the picker.

The no-project state does not map to the configured access root. Doing so would turn a non-project choice into authority over all projects beneath that root. Project-scoped filesystem, command, Git, memory, skill, and project-instruction tools therefore continue to fail closed. Project-independent tools, bridged MCP tools, setup/update/doctor functionality, and ordinary ChatGPT conversation remain usable.

## Persistence and concurrency

ChatGPT conversation no-project choices are stored as a small versioned marker beside, but distinct from, the existing project-binding JSON file. The marker is namespaced by the canonical access-root hash and the hashed `openai/session` identity, so it exposes neither the raw conversation identifier nor another access root's state.

Project selection and no-project selection acquire the same per-conversation binding lock before reading or writing either record. Exactly one state can win concurrent project/no-project attempts. If both records somehow exist, Codexify fails closed and reports inconsistent binding state instead of choosing one.

Generic MCP clients retain the same semantics in transport memory: their binding state is unselected, project, or without-project and disappears on reconnect as before.

Existing project-binding files remain unchanged and need no migration. The no-project marker is a new format read only by versions that support this feature.

## Backend API

`project_bindings.rs` exposes a read-only binding-state snapshot with the access root, scope, and either selected project placement or no-project/unselected state. It also adds an idempotent `select_without_project` operation.

`SessionState` mirrors those APIs for generic MCP transports. Project selection after no-project selection, and no-project selection after project selection, are rejected with the existing new-chat/new-session recovery rule.

`ToolRequestContext` carries the shared `ProjectBindingStore` so `setup` can describe the current conversation state after authorization.

## `set_project_root` contract

The existing tool remains the one mutation entry point. Its input accepts exactly one of:

```json
{ "path": "project-selector-or-supported-repository-url" }
```

or:

```json
{ "withoutProject": true }
```

The project form preserves every existing path, clone, exact-target, and worktree rule. The no-project form performs no filesystem or network work.

The structured result gains a required `mode` field (`project` or `without_project`) and a `project_name` field. Existing placement fields remain present; they are strings for project mode and `null` where no project exists. This gives the setup component one stable result shape while preserving existing project receipts.

## Setup result

The setup structured output gains a `project` object:

- `status`: `unselected`, `selected`, `without_project`, or `check_failed`
- `selectionAvailable`: whether multi-project selection is enabled
- `accessRoot`: configured/canonical access root when available
- `name`: selected display name or `Chat without a project`
- `activePath`: effective project path, including a managed worktree path when applicable
- `sourcePath`: source checkout path when a project is selected
- `managedWorktree`: whether the active path belongs to a managed worktree
- `bindingScope`: `static`, `chatgpt_conversation`, `mcp_transport_session`, or `null`
- `detail`: bounded state-read failure detail, otherwise `null`

Setup remains successful when binding-state inspection fails after authorization; the card reports the failure and does not offer a potentially unsafe replacement selection.

The text fallback tells a new multi-project conversation to choose either a project or no-project mode in the card. Existing project and no-project states receive the appropriate next-step text.

## Widget flow

The project section appears before update/schema diagnostics.

For an unselected multi-project context:

1. Render a fixed **Chat without a project** action first.
2. Render a search input immediately below it.
3. Call `list_projects` with the current query and a bounded result limit. The initial empty query loads the first ranked page.
4. Debounce subsequent queries and discard stale out-of-order responses.
5. Render project name, selector, optional description, and aliases using text nodes only.
6. On a project click, call `set_project_root({ path: selector })`.
7. On the no-project click, call `set_project_root({ withoutProject: true })`.
8. Disable the chooser while selection is in flight. On a successful structured receipt, replace the chooser with the selected-state view. On error, retain the chooser and show the bounded tool error.

The selected project view displays the project name and primary active path. For managed worktrees it labels that path as **Worktree** and also displays the source checkout. Direct selections display **Path**. The no-project view states that normal chat and project-independent tools remain available while local project operations are not attached.

The widget continues to use the existing MCP Apps `tools/call` bridge with `window.openai.callTool` fallback. `list_projects` and `set_project_root` remain model-visible because the model still needs the existing automatic-selection workflow; no new public tool is added.

## Error handling and compatibility

- Empty or malformed project queries do not mutate state.
- Catalogue warnings are shown compactly without exposing filtered paths.
- A project that disappears between listing and selection returns the existing safe selection error.
- A stale widget attempting to switch an immutable binding receives the existing cannot-switch error and leaves its current UI intact.
- Old clients that only send `{ path }` continue to work.
- The new setup output is tied to the connector version marker; stale cached schemas continue to trigger the existing Refresh warning.
- The setup card remains useful on hosts without widget tool calls through its textual fallback.

## Security properties

- The no-project option never grants access to the access root.
- Project candidates remain restricted to the existing canonical catalogue and selection validation.
- No raw ChatGPT session identifier is persisted.
- Project/no-project races are serialized with one lock and cannot overwrite a prior choice.
- The widget renders all server-provided strings with `textContent`; it does not use `innerHTML`.
- Search and selection calls use bounded schemas and existing output budgets.

## Testing

Tests cover:

- durable ChatGPT no-project selection across store recreation;
- transport-session no-project selection;
- idempotence and project/no-project switch rejection in both scopes;
- concurrent project versus no-project selection producing one winner;
- no-project `effective_config` failure without exposing the access root as an active project;
- backward-compatible `set_project_root` input and the new no-project form;
- selected direct and managed-worktree structured receipts;
- setup output for static, unselected, selected, no-project, and failed state inspection;
- picker UI strings, search/debounce path, list/set tool calls, selected path/worktree rendering, no-project action, and text-only rendering;
- existing project-selection, setup, registry-schema, and full repository regression suites.
