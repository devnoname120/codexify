# Review checkpoints and MCP Apps UI design

## Goal

Add an explicit review workflow to Codexify without coupling checkpoint correctness to a particular MCP host UI. The server must be able to report changes since project opening or since the last emitted review, advance the incremental baseline atomically, and optionally render the complete review as an MCP App without placing the diff in model-visible context.

## Non-goals

- This does not replace Git commits, branches, or the user's index.
- This does not continuously watch the filesystem or push unsolicited diffs.
- This does not make arbitrary shell execution project-confined; existing execution policy remains unchanged.
- This does not expose repository-wide changes when the selected project is a subdirectory.

## Invariants

1. The logical project root is the review scope. A project nested in a monorepo must never include sibling changes.
2. Snapshot creation must not modify the real Git index or working tree.
3. The project-open checkpoint is immutable once created for an owner and canonical project root.
4. The last-review checkpoint advances with compare-and-swap semantics when a review snapshot is emitted, so concurrent reviews cannot silently overwrite one another.
5. ChatGPT conversation checkpoints survive MCP transport replacement and server restart. Generic MCP-client checkpoints remain transport-local.
6. Raw ChatGPT conversation identifiers are never stored; the existing hashed conversation identity is used in ref names.
7. The ordinary MCP content is concise and model-visible. Complete review data is carried only in namespaced result `_meta`, which MCP Apps receive but the model does not.
8. `show_changes` is host-facing read-only even though it updates a connector-private incremental-delivery cursor. Cursor advancement completes before the result is returned and never depends on whether the user expands, collapses, or otherwise interacts with the widget.

## Ownership model

A checkpoint owner is one of:

- **ChatGPT conversation**: keyed by the existing SHA-256-derived conversation identity. Its two checkpoints are persisted as Git refs so they survive transport and process replacement.
- **Generic MCP transport**: stored only in the transport's in-memory session state. Its synthetic commits are deliberately unreferenced after the transport closes and become ordinary Git-GC candidates.

The persistent refs are namespaced by the canonical logical project root, not merely the repository root, so two selected subprojects in the same monorepo have independent histories:

```text
refs/codexify/review/<project-hash>/<conversation-hash>/project-open
refs/codexify/review/<project-hash>/<conversation-hash>/last-review
```

Only the two current commits remain referenced per conversation/project pair. Advancing a checkpoint does not create an append-only ref history.

## Scoped snapshot algorithm

For a canonical project root `P` inside canonical Git root `G`:

1. Resolve `G` with `git rev-parse --show-toplevel`.
2. Derive the Git pathspec `S = P relative to G`; use `.` only when `P == G`.
3. Read only the scope's tracked index entries with `git ls-files --stage -z -- S`.
4. Create a private temporary Git index and initialise it with `git read-tree --empty`.
5. Seed only those scoped tracked entries through `git update-index -z --index-info`.
6. Refresh the temporary index from the working tree with `git add -A -- S`, invoked from `G`.
7. Write its tree with `git write-tree`.
8. Create a synthetic commit with `git commit-tree` and controlled identity metadata.

The scoped seed handles repositories whose selected project path is globally ignored but already tracked, without force-adding new ignored files. Starting the temporary index empty is intentional: the synthetic tree contains only the logical scope. It cannot inherit staged entries from siblings, and the real index is read only for scoped tracked-entry metadata and never written. Untracked non-ignored files, deletions, executable-bit changes, symlinks, renames inferred by diff, and binary files are represented by normal Git object semantics.

Every comparison also carries the same explicit `-- S` pathspec as a defence in depth. Structured file records and unified-patch headers are rebased from repository-relative to logical-project-relative form.

## Checkpoint lifecycle

Before the first project-scoped tool call for an owner, the server attempts to initialise both checkpoints from one scoped snapshot. This occurs before execution, including before shell tools, so a mutation cannot precede the baseline merely because it entered through an unclassified command.

Checkpoint initialisation is best-effort for unrelated tools: a non-Git project must still be readable and editable. `show_changes` reports the underlying Git error explicitly.

`show_changes` accepts:

- `since: "last_review" | "project_open"`, default `last_review`;
- `advance: boolean`, default `true`;
- `include_patch: boolean`, default `true`.

It creates one current snapshot, compares it with the requested baseline, and by default records that emitted snapshot as `last-review` through `git update-ref <ref> <new> <expected-old>`. A compare-and-swap conflict preserves the returned diff but reports that the incremental cursor was not advanced. `advance=false` leaves the cursor unchanged. The cursor controls only which changes the connector emits on the next incremental call; it is not project, Git-history, user-owned, or external state, so the tool remains annotated read-only and idempotent.

## Result contract

The tool deliberately advertises no `outputSchema` and returns no `structuredContent`. Its text result is concise and usable by the model. Namespaced result `_meta` is the authoritative component-only UI payload:

- requested and effective baseline;
- whether private incremental-cursor advancement was requested and completed;
- scope relative to the repository root;
- aggregate file/addition/deletion/binary counts;
- bounded file records with workspace-relative paths, status, rename source, and line counts;
- unified binary-capable patch when it fits the configured budget;
- explicit omission metadata when the patch or file list is bounded;
- warnings such as compare-and-swap conflicts.

A patch larger than `review.maxPatchBytes` is omitted rather than cut mid-hunk. File metadata and aggregate statistics remain available. The default is 4 MiB, regression-tested with 10,000 changed code lines whose source lines are roughly 300 bytes long. Byte bounding remains necessary because line length and binary-patch expansion are unbounded. `0` disables patch bodies.

## MCP Apps integration

The server advertises the standard `io.modelcontextprotocol/ui` extension and a single resource:

```text
ui://codexify/review/v3/mcp-app.html
text/html;profile=mcp-app
```

The `show_changes` tool descriptor links that resource through both the current nested `_meta.ui.resourceUri` shape and the compatibility `_meta["ui/resourceUri"]` key. Its nested UI visibility is model-only because the component never calls tools. The tool result stores the complete `ReviewResult` under `_meta["io.github.devnoname120/codexify/review"]`. Unsupported clients ignore the component metadata and continue to receive the concise text result. The v2 and prior unversioned resource URIs remain readable with the current HTML, which falls back to historical structured review results only when component metadata is absent; current tool results never produce that model-visible payload.

The resource is a self-contained HTML/CSS/JavaScript document embedded in the Rust binary. It has no external script, font, style, network, or direct browser-storage dependency. PrismJS is vendored under its MIT license and used only as a tokenizer; the review renderer creates every visible node through `textContent` or `createTextNode` rather than inserting highlighted HTML. The app performs the MCP Apps handshake, validates `postMessage` source identity, consumes component-only result metadata from `ui/notifications/tool-result` and ChatGPT's canonical `toolResponseMetadata` envelope, and reports size changes. The client parses unified hunks into old/new line-number gutters, wraps code inside the available card width, applies full-row GitHub-compatible diff colors, computes bounded order-preserving intraline highlights, and applies bounded syntax highlighting selected from the file path without increasing the component payload. Changed line numbers use the normal code color while context numbers remain muted; per-line `+`/`-` markers are omitted. The app does not request host border chrome, leaves the iframe canvas transparent while painting the review panel itself with the host-aware background color, and does not repeat checkpoint/scope metadata above the collapsible review summary. Binary patches are summarized rather than rendering Git's binary body. The view never performs checkpoint mutations itself. Its overall disclosure state, per-file expansions, and larger-file-list disclosure are saved through `window.openai.setWidgetState` under `privateContent` and restored from `window.openai.widgetState` after iframe remounts; none of that state is exposed to the model. Hosts without the ChatGPT state API retain normal interaction for the current iframe lifetime.

## Concurrency and failure handling

Checkpoint initialization, mutating tool calls, and reviews for one owner/project pair are serialized in-process through completion of the tool call. Git refs provide cross-process atomicity for persistent checkpoint creation and cursor advancement. A resident process returned by `exec_command` can outlive its initiating call, so later filesystem mutations from that process are intentionally observed as point-in-time state rather than held behind an unbounded review lock. Snapshot and diff subprocesses run with Codexify's secret-scrubbing policy and controlled temporary-index/identity variables.

Failure rules:

- not a Git worktree: `show_changes` returns a tool error;
- both missing baseline refs are initialized from the current scoped state; a missing last-review ref is restored from project-open, while last-review without project-open fails closed;
- oversized patch: success with explicit omission;
- concurrent baseline advancement: the diff is returned, the cursor is not advanced, and `show_changes` includes an explicit warning;
- non-Git projects: record review as unavailable and continue normal tools;
- checkpoint capture failure inside a Git project: fail closed before mutating tools, while unrelated read-only tools may continue with a diagnostic;
- real index contents: preserved byte-for-byte by construction.

## Testing strategy

Tests use real temporary Git repositories and cover:

- a selected subdirectory excluding modified siblings;
- byte-for-byte preservation of the real index, including staged sibling state;
- immutable project-open and advancing last-review baselines;
- persistence across manager replacement for ChatGPT owners;
- isolation between conversations and transport owners;
- untracked, deleted, renamed, executable, and binary files;
- unborn repositories;
- patch budget omission and logical-project-relative patch headers;
- live deletion and recreation of persistent checkpoint refs;
- serialization between mutating tool calls and reviews;
- malformed Git configuration distinguished from a non-Git project;
- MCP tool metadata, resource discovery/read, handshake ordering, and HTML protocol hooks.
