# codexify — Architecture & Design

A Rust port of the `codexify` MCP bridge. codexify is a local [Model Context
Protocol](https://modelcontextprotocol.io) server that exposes Codex-style agent
tools over **Streamable HTTP**, scoped to a chosen working directory, and can
additionally **aggregate other local MCP servers** and **surface local skills**.

This document explains how it is put together and why. For usage, see
[README.md](../README.md).

---

## 1. Overview

```
ChatGPT / MCP client
        │  HTTPS
        ▼
   public tunnel (ngrok / cloudflared)
        │  HTTP  POST/GET/DELETE /mcp
        ▼
┌──────────────────────────────────────────────┐
│ codexify  (axum + tokio + rmcp)                │
│                                               │
│  /health   /mcp (StreamableHttpService)       │
│                                               │
│  ServerHandler ── list_tools / call_tool      │
│        │                                      │
│        ▼                                      │
│  registry: Vec<Box<dyn Tool>>                 │
│    • 25 native tools                          │
│    • bridged tools  ← upstream MCP servers    │
│    • gateway tools  ← upstream MCP servers    │
│                                               │
│  per-session SessionState (exec + plan)       │
└──────────────────────────────────────────────┘
        │ reads/writes           │ stdio child processes
        ▼                        ▼
   work directory         upstream MCP servers (idasql, remote-exec, …)
```

Three surfaces reach the model:

- **Tools** — `tools/list` + `tools/call` (native, bridged, gateway).
- **Skills** — a catalogue in the server `instructions` plus the `skills_list` /
  `skills_read` tools, discovered from disk.
- **Instructions** — the agent brief + environment + memory + project doc,
  rebuilt per MCP session.

---

## 2. Request lifecycle

1. A client opens an MCP session with `POST /mcp` (`initialize`). rmcp's
   `StreamableHttpService` manages the session and calls the **service factory**
   once per session, producing a fresh `CodexHandler`.
2. `CodexHandler::get_info` returns the negotiated protocol version, capabilities
   (`tools`), server identity, and the `instructions` string (built per session,
   so a resumed conversation opens with the saved plan and notes in front of it).
3. `tools/list` → `CodexHandler::list_tools` maps the shared tool registry into
   rmcp `Tool` definitions.
4. `tools/call` → `CodexHandler::call_tool` finds the tool by name, runs it, then
   fills in the default `structuredContent` when appropriate.
5. When the session ends, rmcp drops the `CodexHandler`; its `SessionState`
   `Drop` kills any resident `exec_command` shells.

Cross-cutting HTTP concerns live in the axum layer: a `/health` route, a
`tower-http` CORS layer (exposing `mcp-session-id`), and a bearer-auth middleware
that bypasses `/health`.

---

## 3. Core abstractions

### `Tool` trait (`tool.rs`)
Object-safe (`async_trait`) so the registry is `Vec<Box<dyn Tool>>` dispatched by
name. Every tool — native, bridged, or gateway — implements it:

```rust
trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> String;
    fn describe(&self, cfg: &AppConfig) -> String;      // config-aware override
    fn input_schema(&self) -> Value;
    fn output_schema(&self) -> Option<Value>;
    fn fills_structured_content(&self) -> bool;         // opt out of default-fill
    async fn call(&self, args: Value, cfg: &AppConfig, session: &SessionState) -> ToolResult;
}
```

### `ToolResult` (`types.rs`)
`{ content: Vec<ToolContent>, is_error: bool, structured_content: Option<Value> }`.
The server converts it to rmcp's `CallToolResult`. Tools with an `outputSchema`
whose text *is* the structured form rely on the server's default-fill
(`{ "content": <joined text> }`); tools that build their own structured content —
or bridge it from upstream — return `fills_structured_content() == false`.

### `SessionState` (`exec_sessions.rs`)
Per-MCP-session mutable state: the map of resident `exec_command` shells and the
current plan. Created fresh per session by the factory; `Drop` disposes shells.

### `AppConfig` (`types.rs`)
The fully-resolved config handed to every tool. Parsed from `codex.config.json`
with camelCase field names for backward compatibility. Optional sub-configs
(`projectDoc`, `output`, `memory`, `skills`, `ignore`) fall back to per-module
defaults.

---

## 4. MCP server layer (`server.rs`, `auth.rs`)

- **Transport**: `rmcp::transport::streamable_http_server::StreamableHttpService`,
  configured with `json_response = true` and DNS-rebinding host checks disabled by
  default (so it works behind a tunnel presenting an arbitrary hostname; set
  `allowedHosts` to re-enable).
- **Session model**: the factory runs per session, so each conversation gets its
  own `SessionState`. Upstream MCP connections and the tool registry are shared
  (`Arc`) across sessions.
- **`get_info`** advertises server name `codexify` (wire-compatible identity),
  version, `tools` capability, and the `instructions`.
- **Errors**: a tool that fails returns `Ok(CallToolResult::error(...))`
  (`isError: true`) so the caller sees the message; only an unknown tool name is
  an error *result* as well. Protocol errors are avoided.

---

## 5. Native tools (25)

| Group | Tools |
|-------|-------|
| File / code | `read_file`, `write_file`, `apply_patch`, `glob`, `grep`, `list_directory`, `tree`, `view_image` |
| Commands | `run_command` (allowlisted argv), `exec_command` / `write_stdin` (resident shell sessions) |
| Git | `git_status`, `git_push`, `git_commit`, `git_log` |
| Environment / project | `get_environment`, `get_project_doc`, `get_agent_brief` |
| Task state | `update_plan`, `remember`, `recall` |
| Skills | `skills_list`, `skills_read` |
| Timing | `clock_curr_time`, `clock_sleep` |

Each lives in `src/tools/<name>.rs`; the registry (`registry.rs`) lists them in
the original order and rejects duplicate names.

---

## 6. Infrastructure modules

| Module | Responsibility |
|--------|----------------|
| `safe_path.rs` | Lexical path-traversal guard (no `canonicalize`; component-wise containment). The security boundary for every filesystem tool. |
| `output_budget.rs` | Line/byte windowing and list caps, each cut announced with the continuation argument. |
| `ignore_rules.rs` | One `.gitignore`-accurate matcher (the `ignore` crate) shared by glob/grep/tree/list_directory. |
| `exec_policy.rs` | Shell-string allowlist guard for `exec_command` (a guardrail, not a sandbox). |
| `exec_sessions.rs` | Unified-exec sessions: shell resolution, PowerShell exit-code wrapping, background stdout/stderr drain tasks, process-group kill, output truncation (UTF-16 units to match the TS). |
| `apply_patch.rs` | The Codex patch format: parse then apply, atomically, with fuzzy context matching and CRLF preservation. |
| `memory.rs` | Working memory outside the repo, keyed by a hash of the normalized work dir, with `O_EXCL` locking and atomic writes. |
| `project_doc.rs` | `AGENTS.md` discovery from project root down to the work dir under a byte budget. |
| `skills.rs` | `SKILL.md` discovery (see §8). |
| `instructions.rs` | Assembles the agent brief + environment + saved state + skills + project doc, per session. |
| `environment.rs` | OS / shell / policy description, shared by `get_environment` and the instructions. |

---

## 7. MCP bridging (aggregator)

codexify can act as an MCP **client** to other local MCP servers, discover their
tools at startup, and re-expose them. Implemented in `bridge.rs`; wired in
`server.rs::start_http_server` before the HTTP server starts.

### Discovery
For each entry in `mcpServers` (sorted, non-disabled), `connect_one`:
1. Launches the `command` as a stdio child process (`TokioChildProcess`).
2. Runs the MCP handshake (`().serve(transport)`), then `list_all_tools()`,
   under a 20 s timeout.
3. Applies the optional `tools` allow-list.

Failures are **reported, not fatal** — each server appears in the startup banner
as `-> N tool(s)`, `-> FAILED: <reason>`, `-> disabled`, or
`-> gateway (N functions via <tool>)`. The `RunningService` handles are kept in
`Bridge.services` for the whole server lifetime (dropping one kills its child).

### Direct mode (default)
Each upstream tool becomes its own `BridgedTool`, named `<server>__<tool>`
(sanitised to `[A-Za-z0-9_]`, so `remote-exec` → `remote_exec__exec`). `call`
forwards `tools/call` to the upstream peer by the tool's **original** name and
passes the result through verbatim (text, images, structured content, error
flag). A name colliding with an existing tool is skipped with a warning.

### Gateway mode (`"mode": "gateway"`)
For servers with many tools (where a client such as ChatGPT won't reliably
surface a large set), the whole server collapses into **one** dispatcher tool:

- One `GatewayTool` named `<server>` with input `{ function: <enum>, arguments: object }`.
  Its `call` validates `function` against the enum, then forwards
  `call_tool(function, arguments)` to the upstream.
- Its description carries a compact one-line-per-function list (kept small to stay
  under per-tool size limits).
- An **auto-generated skill** documents every function and its full argument
  schema (see §8.3).

So an 84-tool upstream shows up as **1 tool + 1 skill** instead of 84 tools.

### Why bridging opts out of default-fill
Bridged/gateway results are passed through verbatim; `fills_structured_content()`
returns `false` so the server never synthesises a `{content}` structured result
that would not match the upstream's own schema.

### Transports
Only **stdio** (command-launched) upstreams are bridged. `type: "sse"` / `"http"`
or a bare `url` are recognised and reported as *not supported yet* rather than
failing the whole config.

---

## 8. Skills discovery (`skills.rs`)

A skill is a directory holding a `SKILL.md` whose YAML frontmatter carries a
`name` and `description`. codexify discovers three kinds, all merged (deduped by
lowercased name, repo > user precedence) and surfaced through the instructions
catalogue and `skills_list` / `skills_read`.

### 8.1 Standalone skills
`.agents/skills`, `.codex/skills`, and `.claude/skills` — in each project
directory (root → work dir) and under the home directory. Scope `repo` / `user`.

### 8.2 Plugin skills
Installed Claude Code plugins under
`~/.claude/plugins/cache/<marketplace>/<plugin>/<version>/skills/*`. The highest
installed version per plugin is used; each skill is namespaced `<plugin>:<skill>`
(e.g. `idasql:decompiler`). Scope `plugin`. Enabled by default; suppressed when an
explicit `skills.dirs` override is set (which is also how the test suite isolates
from the real home). Toggle with `skills.includePlugins`.

### 8.3 Generated gateway skills
For each gateway-mode MCP server, codexify writes a `SKILL.md` to a per-port temp
directory (`<temp>/codexify-gateway-skills/<port>/<server>/SKILL.md`, rebuilt fresh
each start) documenting every function and its argument schema. That directory is
added to the skill roots, so the generated skill is discovered like any other and
read through `skills_read`. Scope `plugin`.

---

## 9. Configuration reference

`codex.config.json` (loaded from the current directory, or `--config`; the
startup banner prints the exact file with `Config:`). All fields optional.

```jsonc
{
  "port": 3000,
  "apiKey": "…",                      // or --api-key; bearer token
  "allowedCommands": ["git", "node", …],   // run_command allowlist
  "allowedHosts": [],                  // DNS-rebinding allowlist; empty = any host
  "tree":   { "defaultDepth": 3, "ignore": ["node_modules", ".git", …] },
  "command":{ "defaultTimeout": 30000, "maxTimeout": 120000 },   // ms
  "exec":   { "mode": "allowlist"|"unrestricted",
              "extraAllowedCommands": ["ls", "cat", …], "maxSessions": 8,
              "defaultShell": "…" },
  "ignore": { "useGitignore": true, "useDefaultPatterns": true, "customPatterns": [] },
  "output": { "maxFileLines": 1000, "maxFileBytes": 131072, "maxEntries": 500, "maxTreeNodes": 1000 },
  "projectDoc": { "maxBytes": 32768, "fallbackFilenames": [], "rootMarkers": [".git"] },
  "memory": { "enabled": true, "dir": "…", "maxBytes": 16384 },
  "skills": { "enabled": true, "dirs": ["…"], "includePlugins": true },

  "mcpServers": {
    "remote-exec": {
      "command": "D:\\mcphub\\mcp-server-windows-x86_64.exe",  // stdio only
      "args": [], "env": {},
      "type": "stdio",                 // "sse"/"http" recognised but not bridged
      "disabled": false,
      "tools": ["exec", "machine_list"],   // optional allow-list of upstream names
      "mode": "gateway"                // or omit for "direct"
    }
  }
}
```

---

## 10. Startup & diagnostics

The banner is designed so failures are never silent:

```
Config: D:\codex-bridge\codex.config.json          ← which file actually loaded
Tools loaded (26): 25 native + 1 bridged from upstream MCP servers
Upstream MCP servers:
  remote-exec -> gateway (84 functions via `remote_exec`)
Auth: disabled (no --api-key)
```

- `Config:` reveals the common mistake of editing a different file than the one
  loaded (config is resolved relative to the launch directory unless `--config`).
- The `Upstream MCP servers:` block reports each server's outcome.

---

## 11. Notes on the JS → Rust port

Faithful to the TypeScript original; unavoidable differences, each documented in
the README:

- `grep` uses the Rust `regex` crate (no lookaround / backreferences).
- Filename sort uses byte/Unicode ordering, not JS `localeCompare`.
- `write_file`'s byte count is UTF-8 bytes.
- `exec_command` runs with plain pipes, not a PTY.
- `glob` walks the tree itself (no symlink-dir following); its `dot: false`
  handling is approximate for mixed literal-dot / wildcard patterns.
- A trailing-slash ignore pattern hides the directory entry itself.

`exec_command` output truncation and token counting deliberately use UTF-16 code
units to match the TS `text.length` / `text.slice`.

---

## 12. Testing

- **334 tests** — unit tests inside modules plus integration tests under `tests/`
  (`tempfile`-isolated), ported from the TS Bun suite.
- Memory / skills tests pin `memory.dir` / `skills.dirs` to temp dirs so they never
  touch the real home; plugin discovery is suppressed when `skills.dirs` is set.
- `tests/review_fixes.rs` locks the behavioral-fidelity fixes found by the
  adversarial review of the port. The bridge/gateway/skills code was reviewed the
  same way; the confirmed low-severity findings (name-collision dedup, YAML-safe
  generated frontmatter, non-object `arguments` rejection, version ordering) are
  fixed with regression tests in `bridge.rs` / `skills.rs`.
- `examples/mock_mcp.rs` is a minimal stdio MCP server used to exercise the bridge
  end-to-end.

Run: `cargo test`. Build a standalone binary: `cargo build --release`.

---

## 13. Troubleshooting

| Symptom | Cause / fix |
|---------|-------------|
| Bridged tools don't appear; banner shows `-> FAILED` | The `command` path isn't a runnable stdio binary **on the machine where codexify runs**. Fix the path or run the server locally. |
| Banner shows a server you didn't configure (e.g. `idasql -> disabled`) | codexify loaded a *different* `codex.config.json` than you edited. Check the `Config:` line and edit that file, or pass `--config`. |
| codexify exposes the tools (`Tools loaded (109)`) but the client shows only 25 | The client caches the tool manifest — **remove and re-add the connector** so it re-fetches `tools/list`. There is no tool-count cap at 109 (the hard API cap is 128). |
| A client won't surface a large bridged set at all | Use `"mode": "gateway"` to collapse the server into one tool + a skill, or `"tools": [...]` to expose a curated few. |
| Upstream `type: "sse"`/`"http"` | Not bridged yet (stdio only); reported as unsupported instead of breaking the config. |
