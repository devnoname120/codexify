# Porting OpenAI Codex Tool Calls into the codexify MCP Bridge

**Date:** 2026-08-12
**Status:** Approved
**Stack:** Bun + TypeScript
**Applies to:** codexify v0.3.1 → v0.4.0

## Problem

codexify exposes 11 hand-rolled tools to ChatGPT Web. They work, but two gaps hurt daily use:

1. **Editing is all-or-nothing.** `write_file` overwrites the whole file. Changing one line in a 500-line file means the model regenerates all 500 lines — slow, expensive, and a frequent source of accidental content loss.
2. **Command execution is one-shot.** `run_command` spawns a binary with an argument array, waits, and returns. There is no way to drive an interactive process, resume a long-running build, or stream partial output.

OpenAI Codex solved both problems years of iteration ago. Rather than invent new designs, this spec ports Codex's tool contracts — exact parameter names, defaults, ranges, and semantics — into codexify, so that a model already trained on Codex's tool surface behaves correctly here with no relearning.

## Source of Truth

All schemas in this document were read directly from the Codex repository, **not** from secondary summaries.

```
Repository: https://github.com/openai/codex
Commit:     2230d64464488d8847197722fdca09d90095c705
Date:       2026-08-12
```

Key source paths:

| Path | Provides |
|------|----------|
| `codex-rs/core/src/tools/handlers/shell_spec.rs` | `exec_command`, `write_stdin`, shared output schema |
| `codex-rs/core/src/tools/handlers/apply_patch_spec.rs` | `apply_patch` freeform tool spec |
| `codex-rs/core/src/tools/handlers/apply_patch.lark` | Patch grammar |
| `codex-rs/core/src/tools/handlers/plan_spec.rs` | `update_plan` |
| `codex-rs/core/src/tools/handlers/view_image_spec.rs` | `view_image` |
| `codex-rs/core/src/tools/handlers/current_time.rs` | `clock.curr_time` |
| `codex-rs/core/src/tools/handlers/sleep.rs` | `clock.sleep` |
| `codex-rs/apply-patch/src/parser.rs` | Patch parser, marker constants |
| `codex-rs/apply-patch/src/file_update.rs` | Hunk application |
| `codex-rs/apply-patch/src/seek_sequence.rs` | Context matching with fuzzy fallback |

A note on secondary sources: the project's `openai-codex-tool-calls-findings.md` was used for orientation only. It is wrong in at least one load-bearing place — it documents `write_stdin.session_id` as a string, when the source declares it a **number**. Every schema below was re-derived from source.

## Scope

**In — 7 tools ported:**

| # | Codex name | MCP name | Why it earns its place |
|---|-----------|----------|------------------------|
| 1 | `exec_command` | `exec_command` | Shell with session support; unblocks interactive and long-running work |
| 2 | `write_stdin` | `write_stdin` | Feeds stdin and polls output of a live session |
| 3 | `apply_patch` | `apply_patch` | Token-efficient surgical edits — the single biggest win |
| 4 | `view_image` | `view_image` | Lets the model inspect screenshots and diagrams on disk |
| 5 | `update_plan` | `update_plan` | Keeps the model on-track across long multi-step edits |
| 6 | `clock.curr_time` | `clock_curr_time` | Models have no reliable clock; cheap to provide |
| 7 | `clock.sleep` | `clock_sleep` | Wait for builds, servers, and watchers to settle |

All 11 existing tools are **retained unchanged in behavior**. Only their descriptions are sharpened (see *Coexistence*). Final surface: **18 tools**.

### Out of Scope

Codex declares 44 static tool identifiers. Most assume a runtime codexify does not have. Excluded, with reasons:

| Tool(s) | Count | Reason for exclusion |
|---------|-------|----------------------|
| `shell_command` | 1 | Redundant. Overlaps `run_command` (one-shot) and `exec_command` (shell). A third command tool would only confuse tool selection. |
| `request_permissions`, `wait_for_environment` | 2 | No sandbox and no multi-environment runtime. There are no permissions to escalate and no environment to wait for. |
| `list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource` | 3 | codexify is an MCP *server*, not a client. There is no upstream MCP server whose resources could be listed. |
| `request_user_input` | 1 | ChatGPT Web can simply ask in the chat turn. MCP elicitation is not supported by the ChatGPT plugin client. |
| `new_context`, `get_context_remaining` | 2 | The bridge has no visibility into the model's context window. Any answer would be fabricated. |
| `tool_search` | 1 | Deferred-tool loading is a planner feature. With 18 tools there is nothing to defer. |
| `test_sync_tool` | 1 | Codex-internal concurrency test harness. Not a user-facing capability. |
| `list_available_plugins_to_install`, `request_plugin_install` | 2 | No plugin runtime; installation flow depends on MCP elicitation that ChatGPT Web does not support. |
| `multi_agent_v1.*` (5), `collaboration.*` (6) | 11 | No agent runtime, no thread forking, no mailboxes. Building one is a project, not a tool port. |
| Code Mode `exec`, `wait` | 2 | Requires a sandboxed JavaScript interpreter that can call back into the tool registry. Large enough to be its own project, and a serious security surface. |
| `web.run`, `image_gen.imagegen` | 2 | Require OpenAI provider-side execution and account entitlements. Not reproducible locally. |
| `get_goal`, `create_goal`, `update_goal` | 3 | Depend on Codex's token/time budget accounting, which the bridge cannot observe. |

**Viable locally but deferred to a future spec** — see *Future Work* at the end: `skills.list`, `skills.read`, and the four `memories.*` tools.

## Naming: Dots Become Underscores

Codex namespaces tools as `(namespace, name)` pairs rendered `clock.curr_time`. MCP has no namespace concept — tool names are flat strings, and the ChatGPT plugin UI validates them against `^[a-zA-Z0-9_-]{1,64}$`. A dot risks silent rejection or mangling.

**Rule:** replace `.` with `_`. Applied only where a namespace exists.

| Codex identifier | MCP tool name |
|------------------|---------------|
| `exec_command` | `exec_command` |
| `write_stdin` | `write_stdin` |
| `apply_patch` | `apply_patch` |
| `view_image` | `view_image` |
| `update_plan` | `update_plan` |
| `clock.curr_time` | `clock_curr_time` |
| `clock.sleep` | `clock_sleep` |

Parameter names are **never** renamed. `session_id`, `yield_time_ms`, `max_output_tokens`, `duration_ms`, and `cmd` keep Codex spelling exactly, so a model that knows Codex gets these right on the first attempt.

## Architecture Changes

Three foundation changes in `src/types.ts`, then seven new tool modules.

### 1. `ToolResult.content` becomes a union

`view_image` must return image bytes. The current type cannot express that:

```typescript
// Before
export interface ToolResult {
  content: Array<{ type: "text"; text: string }>;
  isError?: boolean;
}

// After
export type ToolContent =
  | { type: "text"; text: string }
  | { type: "image"; data: string; mimeType: string };  // data is base64

export interface ToolResult {
  content: ToolContent[];
  isError?: boolean;
}
```

This is a widening, so all 11 existing tools type-check unchanged.

### 2. `ToolDefinition.handler` gains a third parameter

```typescript
export interface ToolDefinition {
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
  outputSchema?: Record<string, unknown>;
  handler: (
    args: Record<string, unknown>,
    config: AppConfig,
    session: SessionState,
  ) => Promise<ToolResult>;
}
```

TypeScript permits a function to declare fewer parameters than its target signature, so **no existing tool file needs editing**. Only the new tools that need state declare the third parameter.

### 3. `SessionState` — per-MCP-session mutable state

```typescript
export interface ExecSession {
  id: number;
  proc: Bun.Subprocess;
  buffer: string;          // accumulated stdout+stderr not yet returned
  exited: boolean;
  exitCode: number | null;
  startedAt: number;
  cmd: string;
}

export interface PlanItem {
  step: string;
  status: "pending" | "in_progress" | "completed";
}

export interface SessionState {
  execSessions: Map<number, ExecSession>;
  nextExecSessionId: number;   // starts at 1
  plan: PlanItem[];
  planExplanation?: string;
}
```

Lifecycle, in `src/server.ts`:

- A fresh `SessionState` is constructed alongside each new `WebStandardStreamableHTTPServerTransport` and stored in the existing `sessions` map as a third field.
- It is passed into `createMcpServer(config, tools, sessionState)` and closed over by the `CallToolRequestSchema` handler, which forwards it as the third handler argument.
- On `transport.onclose`, **every live `ExecSession` is killed** before the map entry is deleted. Without this, an abandoned tunnel session leaks orphaned processes.

Two clean units emerge, each independently testable:

- `src/exec-sessions.ts` — spawn, poll, write, truncate, kill. No MCP knowledge.
- `src/apply-patch.ts` — parse and apply patch text. Pure functions over strings plus one filesystem write step.

## Tool Specifications

### `exec_command`

Source: `shell_spec.rs::create_exec_command_tool_with_environment_id`.

```
required:             ["cmd"]
additionalProperties: false
```

| Param | Type | Default | Notes |
|-------|------|---------|-------|
| `cmd` | string | — | Shell command to execute. Required. |
| `workdir` | string | work-dir | Resolved through `resolveSafePath` |
| `tty` | boolean | `false` | Only `false`/omitted supported; see below |
| `yield_time_ms` | number | `10000` | Clamped to 250–30000 |
| `max_output_tokens` | number | `10000` | Output budget |

Codex's `shell`, `login`, `environment_id`, and the three approval parameters (`sandbox_permissions`, `justification`, `prefix_rule`) are **omitted** — each is conditional on a Codex runtime feature the bridge lacks. Omitting them is faithful: Codex itself omits them when the corresponding option is off.

Codex's description reads *"Runs a command in a PTY, returning output or a session ID for ongoing interaction."* We adapt only the first clause — see *On `tty`* below — giving: *"Runs a command in a shell, returning output or a session ID for ongoing interaction."*

Shared output schema, from `unified_exec_output_schema`:

```json
{
  "type": "object",
  "properties": {
    "chunk_id":             { "type": "string" },
    "wall_time_seconds":    { "type": "number" },
    "exit_code":            { "type": "number" },
    "session_id":           { "type": "number" },
    "original_token_count": { "type": "number" },
    "output":               { "type": "string" }
  },
  "required": ["wall_time_seconds", "output"],
  "additionalProperties": false
}
```

Behavior:

1. Validate `cmd` against the allowlist (see *Security*).
2. Spawn through a shell so pipes and redirects work: `powershell -NoProfile -Command <cmd>` on Windows, `/bin/sh -c <cmd>` elsewhere. Configurable via `exec.shell`.
3. Accumulate stdout and stderr into one interleaved buffer.
4. Wait up to `yield_time_ms`.
5. If the process exited, return `output`, `exit_code`, and `wall_time_seconds` — no `session_id`.
6. If still running, register an `ExecSession`, and return `output` so far plus `session_id`. The model continues with `write_stdin`.

**On `tty`.** No native PTY module is required. The source is explicit that PTY allocation is opt-in, not the default path:

> `tty` — *"True allocates a PTY for the command; false or omitted uses plain pipes."*

So the default and overwhelmingly common case is plain pipes, which `Bun.spawn({ stdin: "pipe", stdout: "pipe", stderr: "pipe" })` covers natively. No `node-pty`, no native addon, and the standalone single-binary builds in `.github/workflows/` keep working.

`tty: true` is **rejected** with a clear message: `tty: true is not supported; omit tty or set it to false to use plain pipes.` Failing loudly beats accepting the flag and silently ignoring it — a model that asked for a PTY (for a curses UI or an `isatty`-gated prompt) needs to know it did not get one. The tool description notes the limitation so the model avoids the flag in the first place.

Note that `exec_command`'s own description string says *"Runs a command in a PTY"* while the `tty` parameter documents pipes as the default. The parameter documentation governs actual behavior; our description is adjusted to say *"Runs a command in a shell"* rather than repeat a claim we do not honor.

### `write_stdin`

Source: `shell_spec.rs::create_write_stdin_tool`.

```
required:             ["session_id"]
additionalProperties: false
```

| Param | Type | Default | Notes |
|-------|------|---------|-------|
| `session_id` | **number** | — | Not a string. Identifier of a running session. |
| `chars` | string | `""` | Empty polls without writing |
| `yield_time_ms` | number | see below | Non-empty write: 250, cap 30000. Empty poll: 5000, range 5000–300000. |
| `max_output_tokens` | number | `10000` | Output budget |

Description: *"Writes characters to an existing unified exec session and returns recent output."*

Output schema: identical to `exec_command`.

Behavior: look up the session (unknown id → `isError` with the list of live ids); write `chars` to stdin if non-empty; wait; drain the buffer; return it. When the process has exited, include `exit_code` and remove the session from the map.

### Output Truncation

Both exec tools honor `max_output_tokens`. Tokens are approximated as four UTF-16 code units each, without a tokenizer dependency. Since the bounded-model-output follow-up, a call may lower this budget but cannot raise it beyond `output.maxToolOutputTokens`; output retains bounded head and tail text with an explicit middle marker and reports the pre-truncation estimate. See `2026-08-28-bounded-model-output-design.md` for the connector-wide policy.

### `apply_patch`

Source: `apply_patch_spec.rs`, grammar in `apply_patch.lark`.

**The freeform problem.** In Codex this is `ToolSpec::Freeform` with a Lark grammar — the model emits raw patch text, not JSON. MCP has no freeform tool type; every tool takes a JSON object.

**Resolution:** expose a function tool with exactly one required string parameter, `input`, holding the verbatim patch text.

```json
{
  "type": "object",
  "properties": {
    "input": {
      "type": "string",
      "description": "The full patch text, beginning with '*** Begin Patch' and ending with '*** End Patch'."
    }
  },
  "required": ["input"],
  "additionalProperties": false
}
```

The grammar is embedded verbatim in the tool description so the model can generate conforming text without a grammar-constrained decoder. Codex's own description warns *"This is a FREEFORM tool, so do not wrap the patch in JSON"* — inverted here, since JSON wrapping is exactly what MCP requires. The description states this explicitly to avoid the model double-escaping.

Grammar (unchanged from `apply_patch.lark`, minus the `environment_id` extension, which is off without multi-environment support):

```
start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?

hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?

filename: /(.+)/
add_line: "+" /(.*)/ LF -> line

change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF
```

Marker constants, matching `parser.rs` byte for byte (note the trailing spaces):

```
"*** Begin Patch"     "*** End Patch"
"*** Add File: "      "*** Delete File: "     "*** Update File: "
"*** Move to: "       "*** End of File"
"@@ "                 "@@"
```

**Parser design** (`src/apply-patch.ts`) — a hand-written line scanner, not a Lark runtime. The grammar is line-oriented and small enough that a parser generator would be pure overhead.

Two phases, kept separate so parse errors never leave a half-applied patch:

*Step 1 — parse to an operation list.* Reject if the first line is not `*** Begin Patch` or the last is not `*** End Patch`. Then produce:

```typescript
type PatchOp =
  | { kind: "add";    path: string; lines: string[] }
  | { kind: "delete"; path: string }
  | { kind: "update"; path: string; moveTo?: string; chunks: UpdateChunk[] };

interface UpdateChunk {
  contextHeader?: string;  // text after "@@ "
  oldLines: string[];      // from " " and "-" lines
  newLines: string[];      // from " " and "+" lines
  isEof: boolean;          // "*** End of File" seen
}
```

*Step 2 — validate, then apply.* Every path resolves through `resolveSafePath`, including `moveTo`. All target files are read and all chunks located **before** any write. If any chunk fails to match, nothing is written and the error names the file and the failing context.

**Context matching** — port `seek_sequence.rs` faithfully. Four passes of decreasing strictness, first hit wins:

1. Exact line equality.
2. Ignore trailing whitespace (`trimEnd`).
3. Ignore leading and trailing whitespace (`trim`).
4. Unicode normalization, then compare: dashes `U+2010`–`U+2015` and `U+2212` → `-`; single quotes `U+2018`–`U+201B` → `'`; double quotes `U+201C`–`U+201F` → `"`; spaces `U+00A0`, `U+2002`–`U+200A`, `U+202F`, `U+205F`, `U+3000` → `" "`.

This fuzziness is not sloppiness — it is what makes patches survive a model that reproduces context lines with typographic quotes or normalized indentation. Two guards from the Rust source carry over: an empty pattern matches at `start`, and a pattern longer than the file returns no match rather than reading out of bounds.

When `isEof` is set, search begins at `lines.length - pattern.length` so end-of-file hunks anchor at the end, falling back to a full search.

Chunks apply sequentially, each searching from the end of the previous match, so repeated context blocks resolve in order.

Line endings: detect the file's dominant ending on read and restore it on write, so patching a CRLF file on Windows does not rewrite every line.

Result text lists each file and its change, mirroring Codex's summary style:

```
Success. Updated the following files:
M src/server.ts
A src/exec-sessions.ts
D src/old-thing.ts
```

### `view_image`

Source: `view_image_spec.rs`.

```
required:             ["path"]
additionalProperties: false
```

| Param | Type | Notes |
|-------|------|-------|
| `path` | string | Image path, resolved via `resolveSafePath` |
| `detail` | enum `high` \| `original` | Optional |

Description: *"View a local image file from the filesystem when visual inspection is needed. Use this for images already available on disk."*

**Output divergence — deliberate.** Codex returns `{ image_url, detail }` where `image_url` is a data URL, because Codex re-injects it into its own model input. MCP has a native image content block, and ChatGPT Web renders it directly:

```json
{ "type": "image", "data": "<base64>", "mimeType": "image/png" }
```

We return the MCP form. No `outputSchema` is declared, since MCP's `outputSchema` describes `structuredContent`, which does not apply to image blocks.

MIME type comes from the file extension, confirmed against magic bytes (PNG `89 50 4E 47`, JPEG `FF D8 FF`, GIF `47 49 46 38`, WebP `RIFF....WEBP`, BMP `42 4D`). Mismatch or unknown type → `isError`.

A `viewImage.maxBytes` cap (default 5 MB) rejects oversized files with a clear message. Base64 inflates payloads by ~33%, and an unbounded read here would blow the response budget. `detail` is accepted and echoed but performs no resizing — that would need an image library, and no resizing is honest behavior for `original`.

### `update_plan`

Source: `plan_spec.rs`.

```
required:             ["plan"]
additionalProperties: false
```

| Param | Type | Notes |
|-------|------|-------|
| `explanation` | string | Optional |
| `plan` | array of `{ step, status }` | Required. Item requires both fields; `additionalProperties: false`. |

`status` enum: `pending` \| `in_progress` \| `completed`.

Description, verbatim from source:

```
Updates the task plan.
Provide an optional explanation and a list of plan items, each with a step and status.
At most one step can be in_progress at a time.
```

Codex declares no output schema; neither do we. Behavior: validate that at most one item is `in_progress` (reject otherwise), store into `SessionState.plan`, and return a rendered checklist plus `Plan updated`. Rendering the plan back is a small but real aid — it gives the model a stable, re-readable view of its own progress.

### `clock_curr_time`

Source: `current_time.rs` (namespace `clock`, tool `curr_time`).

No parameters. Description: *"Return the current time in UTC."*

```json
{
  "type": "object",
  "properties": {
    "current_time": {
      "type": "string",
      "description": "Current UTC time formatted as YYYY-MM-DD HH:MM:SS UTC."
    }
  }
}
```

Format exactly `YYYY-MM-DD HH:MM:SS UTC`, e.g. `2026-08-12 00:15:30 UTC`.

### `clock_sleep`

Source: `sleep.rs` (namespace `clock`, tool `sleep`).

```
required: ["duration_ms"]
```

`duration_ms`: number, valid range **1 to 43200000** (12 hours, from `MAX_SLEEP_DURATION_MS = 12 * 60 * 60 * 1000`). Out of range → `duration_ms must be between 1 and 43200000`.

Description: *"Pause execution for a specified duration. The sleep ends early when new input arrives for the active turn. Returns the elapsed wall-clock time."*

Early termination on new input is a Codex turn-lifecycle feature with no bridge equivalent; the sleep always runs its full duration. The description is adjusted to drop the early-exit sentence rather than promise behavior we do not implement.

A `sleep.maxDurationMs` config (default `300000`, 5 minutes) caps it well below Codex's 12 hours. A 12-hour sleep would hold an HTTP request open far past any tunnel or client timeout; the schema keeps Codex's range for familiarity while the runtime enforces something survivable, and exceeding it returns a clear error rather than hanging.

## Security: `exec_command` and the Allowlist

**This is the most consequential change in the spec.**

`run_command` takes `command` plus `args[]` and checks `command` against `allowedCommands`. `exec_command.cmd` is a free-form shell string. Dropping it in naively would silently void the allowlist — the single security boundary the README advertises. That must not happen quietly.

### Two Modes

A new config key `exec.mode` selects the policy:

| Mode | Behavior |
|------|----------|
| `"allowlist"` *(default)* | Validate every command position against the effective allowlist |
| `"unrestricted"` | No checks; `cmd` runs as written |

`"unrestricted"` exists because some users will point this at a scratch VM and want the full shell. It is opt-in, and the server logs a prominent warning at startup when it is active.

### Allowlist Mode

`exec_command` runs `cmd` through a validator before spawning:

1. **Split into command positions.** Tokenize respecting single and double quotes, then split on `;`, `&&`, `||`, `|`, `&`, and newlines. Each resulting segment starts a new command position.
2. **Skip environment assignments.** Leading `FOO=bar` tokens are stepped over; the command is the first token after them.
3. **Ignore redirection targets.** After `>`, `>>`, `<`, `2>`, and friends, the next token is a filename, not a command, and is not validated as one.
4. **Validate every command-position token** — not just the first. `git status | rm -rf /` must fail.
5. **Reject dynamic construction:** command substitution `$(...)`, backticks, and process substitution `<(...)`, `>(...)`. These smuggle a command past step 4.

Rejection names the offending token and lists the effective allowlist, matching `run_command`'s existing error style.

### Effective Allowlist

```
allowedCommands  ∪  exec.extraAllowedCommands
```

`allowedCommands` is **left untouched**, so `run_command` keeps its current behavior exactly. The union applies only to `exec_command`.

This split matters. `allowedCommands` defaults to build tooling — `bun`, `npm`, `npx`, `node`, `git`, `python`, `pip`, `cargo`, `make`. A shell restricted to those is close to useless: the model cannot even `ls` or pipe into `grep`, so it would abandon `exec_command` and fall back to shelling everything through `run_command`, defeating the point.

`exec.extraAllowedCommands` therefore defaults to read-only utilities that make shell mode genuinely usable:

```json
["ls", "cat", "grep", "find", "head", "tail", "wc", "echo", "pwd",
 "which", "rg", "sed", "awk", "sort", "uniq", "diff"]
```

Every entry reads or transforms text and none is a general execution vector on its own. `sed` and `awk` are the loosest of the set — `sed -i` writes files, `awk` has `system()` — but both are indispensable for real shell work, and per the framing below the guardrail was never the thing standing between an attacker and the filesystem.

### Honest Framing

**This is a guardrail, not a sandbox.** It must be documented as such in the README, in the same voice as the existing security section.

The default `allowedCommands` already contains `node`, `python`, and `bun`. Any of them executes arbitrary code in one line:

```
node -e "require('fs').rmSync('/', {recursive:true})"
```

So the allowlist has never been a security boundary against a determined attacker — it is a boundary against *mistakes*: a model reaching for `rm`, `curl | sh`, `shutdown`, or a package manager the user did not intend. The tokenizer extends that same protection to shell strings, no more.

Claiming more would be worse than claiming nothing, because a user who believes they have a sandbox will point the server at directories they should not. The README already says *"This server has no sandboxing beyond the above"* — that sentence stays true and stays prominent.

The path-traversal boundary is unchanged and remains real for filesystem tools. It does **not** extend to `exec_command`: a shell command can write anywhere the user can. `workdir` is validated through `resolveSafePath`, but that constrains where the command *starts*, not what it can reach. Documented explicitly.

## Relationship to the Existing Tools

Adding overlapping tools degrades selection unless the boundaries are stated. No tool is removed; four descriptions are sharpened.

| Task | Correct tool | Why |
|------|--------------|-----|
| Edit part of an existing file | `apply_patch` | Sends only the changed region |
| Create a new file, or fully replace one | `write_file` | Patch context is pointless when there is none |
| Read a file | `read_file` | Line numbers, pagination, no shell overhead |
| Search code | `grep` / `glob` | Structured, no shell quoting hazards |
| One-shot binary with fixed args | `run_command` | Tightest validation; no shell parsing |
| Pipes, redirects, chained or interactive commands | `exec_command` | Only tool that reaches a shell |
| Feed a running process | `write_stdin` | Requires a live `session_id` |

Description edits:

- `write_file` — add: *"For modifying part of an existing file, prefer `apply_patch`, which only sends the changed lines."*
- `run_command` — add: *"For shell features (pipes, redirects, `&&`) or interactive commands, use `exec_command` instead."*
- `read_file`, `grep` — add a note preferring them over shelling out to `cat` or `grep` via `exec_command`.

### The Two Tool Sets Are Complementary, Not Competing

Codex ships **no** `read_file`, `grep`, `glob`, `list_directory`, or `tree` — verified by searching `codex-rs/core/src/tools/`, where the only matches are MCP test fixtures. Codex routes all file reading and searching through the shell, because it runs in a sandboxed environment with a full PTY and a model tuned for that surface.

codexify operates under different constraints, and its structured primitives are the better fit for them:

- **Token efficiency.** `read_file` with `offset`/`limit` returns exactly the requested slice with line numbers. `cat file | sed -n '100,150p'` returns unnumbered text and costs an extra reasoning step to construct.
- **Safety by construction.** `grep` and `glob` take a pattern parameter and cannot be turned into a command. Their shell equivalents pass through a tokenizer and an allowlist that can only ever be approximate.
- **No quoting hazards.** A regex containing quotes, `$`, or backticks is a plain string argument to `grep`; through a shell it is a minefield, and on Windows a PowerShell one.
- **Cross-platform.** `tree` and `list_directory` behave identically on Windows and POSIX. Their shell equivalents do not.

So this is **addition, not replacement**. codexify contributes safe, structured, token-cheap primitives for reading and searching. Codex contributes the two capabilities that were genuinely missing: a session-capable shell and patch-based editing. All 11 existing tools stay exactly as they are, and the routing table above is what keeps the combined surface legible to the model.

## Configuration

New `codex.config.json` keys, all optional with defaults:

```json
{
  "exec": {
    "mode": "allowlist",
    "extraAllowedCommands": [
      "ls", "cat", "grep", "find", "head", "tail", "wc", "echo", "pwd",
      "which", "rg", "sed", "awk", "sort", "uniq", "diff"
    ],
    "shell": null,
    "defaultYieldMs": 10000,
    "maxOutputTokens": 10000,
    "maxSessions": 8
  },
  "viewImage": {
    "maxBytes": 5242880
  },
  "sleep": {
    "maxDurationMs": 300000
  }
}
```

`AppConfig` gains matching fields. Merge order is unchanged: defaults → config file → CLI flags. Notes:

- `exec.mode` is `"allowlist"` or `"unrestricted"`. An unrecognized value fails at startup rather than silently defaulting — a typo here must not quietly disable the guardrail.
- `exec.extraAllowedCommands` applies only to `exec_command`. `allowedCommands` is untouched and continues to govern `run_command` alone.
- `exec.shell` of `null` means platform default: `powershell -NoProfile -Command` on Windows, `/bin/sh -c` elsewhere.
- `exec.maxSessions` bounds concurrent `ExecSession` entries per MCP session; exceeding it returns an error naming the live sessions rather than spawning without limit.

## Error Handling

Follows the existing convention — `isError: true` plus a plain-language message, never a thrown exception across the MCP boundary.

| Scenario | Message |
|----------|---------|
| Command position not in allowlist | `Command not allowed: "<tok>". Allowed: ...` |
| Command substitution in allowlist mode | `Command substitution is not allowed. Set exec.mode to "unrestricted" to enable.` |
| `tty: true` requested | `tty: true is not supported; omit tty or set it to false to use plain pipes.` |
| Invalid `exec.mode` at startup | `Invalid exec.mode: "<v>". Expected "allowlist" or "unrestricted".` |
| Unknown `session_id` | `No such session: <id>. Live sessions: <ids>` |
| Session limit reached | `Too many active sessions (<n>). Live sessions: <ids>` |
| Patch missing begin/end marker | `Invalid patch: expected '*** Begin Patch' on the first line` |
| Patch context not found | `Failed to find context in <path>: <first context line>` |
| Patch targets a path outside work-dir | `Path must be within work directory` |
| `add` targets an existing file | `File already exists: <path>` |
| `update`/`delete` targets a missing file | `File not found: <path>` |
| Image exceeds `maxBytes` | `Image too large: <n> bytes (max <max>)` |
| Unrecognized image format | `Unsupported image format: <path>` |
| `duration_ms` out of range | `duration_ms must be between 1 and 43200000` |
| Two steps `in_progress` | `At most one step can be in_progress at a time` |

## Testing

Extends the existing `src/**/__tests__/` layout with `bun test`.

**`apply-patch.test.ts`** carries the most weight — it is the most intricate logic and the one where a silent bug corrupts user files:

- Parse: each hunk type; `Move to:`; `*** End of File`; both `@@` context forms; multi-file patches.
- Parse failures: missing begin marker, missing end marker, unknown marker, truncated hunk.
- Matching: all four `seek_sequence` passes, each verified to engage — exact, `trimEnd`, `trim`, and Unicode normalization (a patch with ASCII quotes applied to a file with typographic quotes).
- Guards: empty pattern; pattern longer than file.
- Sequential chunks against repeated context blocks.
- Atomicity: a two-file patch whose second file fails leaves the **first file untouched**.
- CRLF files keep CRLF.

**`exec-sessions.test.ts`** — fast command returns without `session_id`; slow command yields one; `write_stdin` drives an interactive process; empty `chars` polls; exit code surfaces on the final poll; unknown session errors; truncation sets `original_token_count`; killing a session terminates the process.

**`exec-security.test.ts`** — the allowlist table: bare allowed command passes; disallowed fails; `allowed | disallowed` fails; `FOO=bar allowed` passes; a command from `extraAllowedCommands` passes while the same command stays rejected by `run_command`; `$(...)` and backticks fail in `allowlist` mode and pass in `unrestricted` mode; redirection targets are not treated as commands; quoted separators inside strings do not split; invalid `exec.mode` fails at config load.

**`view-image.test.ts`** — valid PNG returns an image block with correct mimeType; extension/magic-byte mismatch errors; oversized file errors; traversal blocked.

**`clock-plan.test.ts`** — time format matches `YYYY-MM-DD HH:MM:SS UTC`; sleep range validation at both bounds; sleep cap enforced; plan stores and renders; two `in_progress` items rejected.

**`registry.test.ts`** — extend to assert 18 tools and no duplicate names.

## Deliverables

New:

```
src/exec-sessions.ts          # session manager (no MCP knowledge)
src/apply-patch.ts            # parser + applier (pure, plus one write step)
src/tools/exec-command.ts
src/tools/write-stdin.ts
src/tools/apply-patch.ts
src/tools/view-image.ts
src/tools/update-plan.ts
src/tools/clock-curr-time.ts
src/tools/clock-sleep.ts
src/**/__tests__/…            # test files listed above
```

Modified:

```
src/types.ts                  # ToolContent union, handler 3rd param, SessionState, AppConfig keys
src/server.ts                 # per-session SessionState; kill exec sessions on close
src/registry.ts               # register 7 new tools
src/config.ts                 # exec / viewImage / sleep defaults
codex.config.json             # new keys
src/tools/write-file.ts       # description only
src/tools/run-command.ts      # description only
src/tools/read-file.ts        # description only
src/tools/grep.ts             # description only
README.md                     # tool table, config, security framing
package.json                  # version 0.4.0
```

No new runtime dependencies. Bun built-ins cover process spawning, file I/O, and base64.

## Implementation Phasing

All 7 tools ship in this effort. The order below sequences the work so each phase lands something testable and the risky parts come after the foundations are proven.

**Phase 1 — Foundations.** `src/types.ts` (`ToolContent` union, handler third parameter, `SessionState`, `AppConfig` keys), `src/config.ts` defaults and `exec.mode` validation, `src/server.ts` session wiring and process cleanup on close. No new tools yet; the existing 18-tool-minus-7 surface must still pass `bun test` and `bunx tsc --noEmit`.

**Phase 2 — Low risk, high confidence.** `clock_curr_time`, `clock_sleep`, `update_plan`. Small, pure, and they exercise the new `SessionState` parameter end to end before anything complex depends on it.

**Phase 3 — `apply_patch`.** The parser and applier in `src/apply-patch.ts`, then the tool wrapper. Heaviest test burden. Independent of the exec work, so it can proceed in parallel if desired.

**Phase 4 — Unified exec.** `src/exec-sessions.ts`, the security tokenizer, then `exec_command` and `write_stdin`. Last because it carries the security surface and benefits from the session lifecycle already being exercised by phase 2.

**Phase 5 — `view_image`.** Needs the `ToolContent` union from phase 1 and nothing else.

**Phase 6 — Integration.** Sharpen the four existing tool descriptions, update `README.md` (tool table, config keys, guardrail-not-sandbox framing), bump to v0.4.0, verify the registry reports 18 tools.

## Future Work

Implementable locally, deliberately not in this spec. Recorded so the decision is not relitigated:

- `skills.list`, `skills.read` — readable from disk, but needs a skills directory convention and paging first.
- `memories.add_ad_hoc_note`, `memories.list`, `memories.read`, `memories.search` — an append-only markdown store with search. Genuinely useful, but orthogonal to the editing and execution gaps this spec closes.

Each would get its own spec.
