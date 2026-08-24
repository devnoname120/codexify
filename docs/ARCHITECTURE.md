# codexify — Architecture & Design

A Rust port of the `codexify` MCP bridge. codexify is a local [Model Context
Protocol](https://modelcontextprotocol.io) server that exposes Codex-style agent
tools over **Streamable HTTP**, scoped either to one configured working directory
or to a project root selected independently by each ChatGPT conversation, and can
additionally **aggregate local or remote MCP servers**, **surface local skills**, and
materialize ChatGPT-native files inside the active project.
Clients without ChatGPT conversation metadata use an MCP-transport-session fallback.

This document explains how it is put together and why. For usage, see
[README.md](../README.md).

---

## 1. Overview

```
ChatGPT / MCP client
        │
        ▼
 OpenAI Secure MCP Tunnel
        ▲
        │ outbound HTTPS polling / responses
        │
 official tunnel-client-runtime
        │ loopback HTTP  POST/GET/DELETE /mcp
        ▼
┌──────────────────────────────────────────────┐
│ codexify  (axum + tokio + rmcp)                │
│                                               │
│  /health   /mcp (StreamableHttpService)       │
│                                               │
│  ServerHandler ── tools + resources           │
│        │                                      │
│        ▼                                      │
│  registry: Vec<Box<dyn Tool>>                 │
│    • 26 native tools                          │
│    • + list_projects + set_project_root       │
│      in multi-project mode                    │
│    • bridged tools  ← upstream MCP servers    │
│    • gateway tools  ← upstream MCP servers    │
│                                               │
│  shared ProjectBindingStore                   │
│    • openai/session hash → project root       │
│    • persistent atomic binding records        │
│                                               │
│  shared ConversationExecSessionStore          │
│    • openai/session hash → resident commands  │
│    • in-memory ownership + idle cleanup       │
│                                               │
│  shared ReviewCheckpointManager               │
│    • project-open + last-review snapshots     │
│    • scoped Git refs + MCP Apps resource      │
│                                               │
│  optional shared AuditLogger                  │
│    • redacted JSONL tool lifecycle records    │
│    • no raw arguments or returned output      │
│                                               │
│  per-transport SessionState                   │
│    • fallback root for generic MCP clients    │
│    • exec sessions + plan + review fallback   │
└──────────────────────────────────────────────┘
        │ reads/writes           │ stdio / Streamable HTTP
        ▼                        ▼
   active project root     upstream MCP servers (idasql, remote-docs, …)
```

Four surfaces reach the model:

- **Tools** — `tools/list` + `tools/call` (native, bridged, gateway).
- **Skills** — a catalogue in the server `instructions` plus the `skills_list` /
  `skills_read` tools, discovered from disk.
- **Instructions** — the agent brief + environment + memory + project doc,
  rebuilt from the active project config.
- **MCP App** — the self-contained review resource linked from `show_changes`;
  unsupported clients ignore the UI metadata and keep the ordinary tool result.

---

## 2. Request lifecycle

1. A client opens an MCP session with `POST /mcp` (`initialize`). rmcp's
   `StreamableHttpService` manages the session and calls the **service factory**
   once per session, producing a fresh `CodexHandler`.
2. `CodexHandler::get_info` returns the negotiated protocol version, capabilities
   (`tools`), server identity, and the `instructions` string. Single-project mode
   builds the full project-aware brief immediately. Multi-project mode emits only
   the root-selection protocol and a project-neutral environment because ChatGPT's
   conversation identity arrives in request `_meta` on tool calls, after
   initialization.
3. `tools/list` → `CodexHandler::list_tools` maps the shared tool registry into
   rmcp `Tool` definitions, including optional titles, behavioral annotations,
   OpenAI file-parameter metadata, and MCP Apps resource metadata.
   `resources/list` / `resources/read` expose the embedded review HTML.
4. `tools/call` → `CodexHandler::call_tool` reads `openai/session` from rmcp's
   `RequestContext::meta`. rmcp moves wire-level request `_meta` into that context
   before dispatch, so the typed tool parameters are not the authoritative source.
5. In multi-project mode, `list_projects` may run before selection. It rebuilds a
   read-only catalogue from the user-level native Codex `[projects]` table and the
   static `projectCatalog.entries` overlay, canonicalizes and filters candidates
   against the access root, and returns relative selectors without reading project
   content or creating a binding.
6. `set_project_root` canonicalizes an existing directory below the configured
   access root. With `openai/session`, it writes an immutable conversation binding
   through the shared `ProjectBindingStore`; without it, the root is stored in the
   current `SessionState`. Re-selecting the same canonical root is idempotent and
   selecting a different root is rejected.
7. Other project-scoped calls resolve the durable conversation binding first, or
   the transport-session fallback when no conversation identity exists, then
   receive an effective clone of `AppConfig` whose `work_dir` is that root. Before
   the first such call, the review manager captures the scoped project-open snapshot.
   Non-Git projects report review as unavailable; a Git snapshot failure blocks
   mutating tools before dispatch.
8. Tool dispatch supplies a request context containing the stable conversation
   identity and shared review manager; tools that do not need it use the default
   context-free implementation. The server fills in default `structuredContent`
   when appropriate. Dispatch also emits diagnostic tracing and, when audit
   logging is enabled, paired JSONL `tool_start` / `tool_finish` records: identity
   and project values are hashed, and scalar argument values and returned payloads
   are replaced by schema-bounded shape and size accounting (unknown argument keys
   and dynamic-map keys are not recorded).
9. `exec_command` and `write_stdin` opt into resident-process routing. With a
   ChatGPT conversation identity, dispatch substitutes the shared conversation's
   exec state while retaining the transport's other mutable state. Without one,
   the ordinary transport-owned state is used.
10. When a transport session ends, rmcp drops the `CodexHandler`: its
    generic-client exec state loses its last owner and kills resident process
    trees. Conversation-owned process state remains in the server for later
    connector calls, subject to idle cleanup; server shutdown drops the shared
    store and kills anything still running. Project bindings and ChatGPT review
    refs are independently durable on disk across server restarts, but process
    handles are not.

Cross-cutting HTTP concerns live in the axum layer: a `/health` route, a
bearer-auth middleware that bypasses `/health`, and—in externally exposed
mode—a `tower-http` CORS layer exposing `mcp-session-id`. Native tunnel mode
does not install that permissive CORS layer.

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
    fn title(&self) -> Option<String>;                  // host-facing card title
    fn annotations(&self) -> Option<ToolAnnotations>;  // MCP behavior hints
    fn meta(&self) -> Option<MetaObject>;               // MCP Apps and extensions
    fn input_schema(&self) -> Value;
    fn output_schema(&self) -> Option<Value>;
    fn fills_structured_content(&self) -> bool;         // opt out of default-fill
    fn requires_project_root(&self) -> bool;            // true by default
    fn uses_exec_session_state(&self) -> bool;          // false by default
    fn may_modify_project(&self) -> bool;               // fail closed on review errors
    async fn call(&self, args: Value, cfg: &AppConfig, session: &SessionState) -> ToolResult;
    async fn call_with_context(&self, args: Value, cfg: &AppConfig, session: &SessionState, context: &ToolRequestContext) -> ToolResult;
}
```

### `ToolResult` (`types.rs`)
`{ content: Vec<ToolContent>, is_error: bool, structured_content: Option<Value>, audit: ToolAuditMetadata }`.
The server converts it to rmcp's `CallToolResult`. Tools with an `outputSchema`
whose text *is* the structured form rely on the server's default-fill
(`{ "content": <joined text> }`); tools that build their own structured content —
or bridge it from upstream — return `fills_structured_content() == false`.
`ToolAuditMetadata` is not sent over MCP; bounded-output and resident-process tools
use it to report truncation, original token count, exec-session ID and PID without
putting operational fields into their public output schema.

### `ProjectBindingStore` (`project_bindings.rs`)
Shared, durable conversation-to-project bindings. `ConversationIdentity` hashes
ChatGPT's `openai/session` value before it reaches a filename; no raw conversation
identifier is written to disk. Records are namespaced by canonical access-root
hash, written atomically under a per-record lock, and validated again on every
load so a deleted project or changed symlink fails closed. A new store instance
can recover the same binding after a server restart.

### `ConversationExecSessionStore` and `SessionState` (`exec_sessions.rs`)
`SessionState` is the per-MCP-transport view: it owns the optional fallback
project root for generic clients, the current plan, a transport-owned exec
state, and the generic-client review fallback. The exec state is
reference-counted and kills running process trees when its last owner disappears.

`ConversationExecSessionStore` is shared by all handlers in the server process.
For the two unified-exec tools, dispatch uses the hashed `openai/session` identity
to substitute conversation-owned exec state into a temporary `SessionState`
view. That state survives replacement transports, is isolated from other
conversations, and is removed after its sessions finish or expire. It is not
written to disk and therefore does not survive server restart.

### `ReviewCheckpointManager` (`review.rs`)
Shared project-review state. ChatGPT owners are keyed by the existing hashed
conversation identity and persist two refs under `refs/codexify/review/`; generic
clients use the transport state above. Snapshots seed a private index with only
tracked entries beneath the logical project path, refresh that scope from the
working tree, and create root commits without touching the user's index. Comparisons
repeat the same literal pathspec, return project-relative file records and patch
headers, bound file and patch results, and advance `last-review` through `git update-ref`
compare-and-swap. The same per-owner/project lock spans mutating tool calls through
completion so reviews cannot capture an in-process write halfway through.

### `AppConfig` (`types.rs`)
The fully-resolved config handed to every tool. `config.rs` parses
`codex.config.json` with camelCase field names for backward compatibility, imports
user-level Codex MCP definitions through `codex_mcp.rs`, opportunistically adds
plugin-provided entries from the Codex CLI's effective catalogue, then applies
explicit `mcpServers` entries as field overlays. Optional sub-configs (`projectDoc`,
`projectCatalog`, `output`, `review`, `artifactIngress`, `memory`, `skills`, `ignore`, `audit`) fall
back to per-module defaults. In multi-project mode, dispatch clones this config per
call and substitutes the conversation's selected root—or the transport fallback—for
`work_dir`; the static server policy, catalogue overlay, and bridge configuration
remain shared. Native Codex project entries are intentionally re-read when the
catalogue tool is called rather than copied into `AppConfig` at startup.

### `quickstart` CLI (`quickstart.rs`)
The `quickstart` subcommand runs before server configuration is loaded. It uses a
testable line-oriented wizard for ordinary prompts and terminal-hidden input for
the runtime API key. The wizard canonicalizes the project directory, validates
the tunnel credentials with the same helpers as normal startup, merges only the
managed fields into the existing JSON object, and stores the key outside the
project behind an absolute `file:` reference. Config and credential replacement
use temporary files in the destination directory. Once setup is complete, the
same process can pass the generated paths back through `load_config` and enter the
ordinary supervised server lifecycle; there is no separate quickstart runtime.

---

## 4. MCP server layer (`server.rs`, `auth.rs`)

- **Transport**: `rmcp::transport::streamable_http_server::StreamableHttpService`,
  configured with `json_response = true`. Externally exposed mode preserves the
  legacy behavior: bind `0.0.0.0`, allow arbitrary Host values unless
  `allowedHosts` is configured, and install permissive CORS for MCP clients.
  Native tunnel mode instead binds `127.0.0.1`, forces Host validation to
  loopback authorities, omits permissive CORS, and requires a random
  process-private bearer token generated at startup.
- **Session model**: the factory runs per MCP transport session. `SessionState`
  owns the generic-client root fallback, current plan, and generic-client
  resident commands. ChatGPT identity is taken from
  `RequestContext::meta["openai/session"]`: the persistent
  `ProjectBindingStore` resolves its project, while the in-memory
  `ConversationExecSessionStore` resolves resident commands across replacement
  transports. Upstream MCP connections, both stores, and the tool registry are
  shared (`Arc`) across transports.
- **`get_info`** advertises server name `codexify` (wire-compatible identity),
  version, tools/resources capabilities, the `io.modelcontextprotocol/ui` extension,
  and the `instructions`. The review resource is embedded in the binary and has no
  external network or asset dependency.
- **Tool descriptors** preserve generic titles, MCP annotations and `_meta`
  extensions. `import_host_file` uses this path for
  `_meta["openai/fileParams"]`; the server has no tool-name special case.
- **Errors**: a tool that fails returns `Ok(CallToolResult::error(...))`
  (`isError: true`) so the caller sees the message; only an unknown tool name is
  an error *result* as well. Protocol errors are avoided.

### 4.1 Native OpenAI tunnel (`openai_tunnel.rs`)

The native tunnel is a supervised sidecar, not a second MCP implementation.
Codexify continues to serve its existing Streamable HTTP endpoint, while the
official OpenAI `tunnel-client-runtime` forwards tunnel commands to
`http://127.0.0.1:<port>/mcp`.

Startup is ordered and fail-closed:

1. Generate a high-entropy internal bearer token and bind the authenticated MCP
   listener to loopback.
2. Resolve an explicit official client binary or install the pinned runtime-only
   release under `~/.codexify/openai-tunnel/`.
3. Verify the official release archive against the per-platform SHA-256 embedded
   in the Codexify build, install the exact expected executable atomically with
   private permissions, and persist a local integrity manifest. Re-check the
   executable hash and compatibility on later starts.
4. Resolve the configured runtime-key reference, launch the client with a clean
   allowlisted environment, and inject both the runtime key and internal MCP
   bearer under child-only synthetic variable names. Static MCP and discovery
   headers carry the internal bearer to the loopback endpoint. Model-controlled
   and upstream MCP subprocesses explicitly remove the original key variable.
5. Require the runtime-only surfaces it actually exports: `/readyz` must return
   success and the labeled
   `commands_poll_last_successful_timestamp_seconds` metric must be non-zero.

Codexify watches the HTTP server, tunnel child, `SIGINT`, and `SIGTERM`
concurrently. Failure of either process shuts down the other. Normal shutdown
sends `SIGTERM` on Unix, waits under a deadline, then force-kills if necessary;
Windows uses the child-process kill path. The MCP cancellation token and Axum
graceful-shutdown signal are triggered together; lingering HTTP connections are
aborted after a bounded grace period. Runtime logs and health URL files live in
a private per-run temporary directory and are removed after shutdown.

---

## 5. Native tools (27 default, 29 multi-project)

| Group | Tools |
|-------|-------|
| File / code | `read_file`, `write_file`, `import_host_file`, `apply_patch`, `glob`, `grep`, `list_directory`, `tree`, `view_image` |
| Commands | `run_command` (allowlisted argv), `exec_command` / `write_stdin` (resident shell sessions) |
| Git / review | `git_status`, `show_changes`, `git_push`, `git_commit`, `git_log` |
| Environment / project | `get_environment`, `get_project_doc`, `get_agent_brief` |
| Task state | `update_plan`, `remember`, `recall` |
| Skills | `skills_list`, `skills_read` |
| Timing | `clock_curr_time`, `clock_sleep` |
| Project selection (multi-project only) | `list_projects`, `set_project_root` |

Multi-project mode prepends `list_projects` and `set_project_root`. Both are
omitted entirely in the default registry, preserving the 27-tool default surface
and behaviour. `artifactIngress.enabled = false` independently removes
`import_host_file`, reducing either count by one. Catalogue discovery, selection,
clocks, and bridged/gateway tools are project-independent; every other native tool
is blocked until a conversation binding or transport fallback is available.

Each lives in `src/tools/<name>.rs`; the registry (`registry.rs`) lists them in
the original order and rejects duplicate names.

---

## 6. Infrastructure modules

| Module | Responsibility |
|--------|----------------|
| `safe_path.rs` | Lexical path-traversal guard (no `canonicalize`; component-wise containment). The security boundary for every filesystem tool. |
| `artifact_ingress/` | OpenAI native-file validation and streaming plus capability-confined, atomic no-overwrite workspace publication. It never accepts a local source path, and constrains the download URL and every redirect hop to the configurable `artifactIngress.allowedHosts` allowlist (default `"*"`, which still rejects loopback, private, link-local, unique-local, CGNAT, `localhost`, and metadata addresses). |
| `logging.rs` | Tracing initialization plus a non-overridable filter that suppresses RMCP framework events, preventing native-file bearer URLs from appearing in logs before tool dispatch or malformed-session errors. |
| `output_budget.rs` | Line/byte windowing and list caps, each cut announced with the continuation argument. |
| `audit.rs` | Private append-only JSONL tool lifecycle records, stable hashed identities, redacted argument summaries, output accounting, and opt-in bounded command previews. |
| `logging.rs` | Default tracing filters for normal, `-v`, and `-vv` operation; an explicit `RUST_LOG` remains authoritative. |
| `ignore_rules.rs` | One `.gitignore`-accurate matcher (the `ignore` crate) shared by glob/grep/tree/list_directory. |
| `exec_policy.rs` | Shell-string allowlist guard for `exec_command` (a guardrail, not a sandbox). |
| `project_bindings.rs` | Canonical project-root validation plus durable ChatGPT conversation bindings keyed by a hash of `openai/session`, namespaced by access root, locked per record, and atomically written. |
| `project_catalog.rs` | Live, read-only project discovery from native Codex plus explicit metadata; canonical access-root filtering, deduplication, deterministic query ranking, sanitized MCP warnings, and local diagnostics. |
| `exec_sessions.rs` | Generic-client transport fallback plus conversation-owned unified-exec sessions and transport-local review state: shell resolution, PowerShell exit-code wrapping, background stdout/stderr drain tasks, process-group kill, idle cleanup, and output truncation (UTF-16 units to match the TS). |
| `review.rs` | Project-scoped Git snapshots, persistent conversation refs, transport-local fallbacks, incremental compare-and-swap checkpoints, diff parsing and result budgets. |
| `review_ui.rs` | Embedded MCP Apps resource and compatibility metadata for the interactive `show_changes` review card. |
| `apply_patch.rs` | The Codex patch format: parse then apply, atomically, with fuzzy context matching and CRLF preservation. |
| `memory.rs` | Working memory outside the repo, keyed by a hash of the normalized active root, with `O_EXCL` locking and atomic writes. In multi-project mode, a configured `memory.dir` is a base containing one hashed child per project. |
| `quickstart.rs` | Interactive first-install wizard for project scope, native tunnel credentials, JSON config merging, and the ChatGPT developer-mode connector handoff. |
| `openai_tunnel.rs` | Verified installation and lifecycle supervision for OpenAI's outbound Secure MCP Tunnel runtime. |
| `process_env.rs` | Child-process environment boundaries: isolate the tunnel runtime and remove tunnel credentials from model-controlled and upstream subprocesses. |
| `project_doc.rs` | `AGENTS.md` discovery from project root down to the work dir under a byte budget. Multi-project mode treats the selected directory as the exact project root and never walks into the common access-root parent. |
| `skills.rs` | `SKILL.md` discovery (see §8). |
| `codex_config.rs` | Shared secret-safe resolver and TOML reader for `$CODEX_HOME/config.toml` or `~/.codex/config.toml`. |
| `codex_mcp.rs` | Read-only import of local stdio and remote Streamable HTTP MCP definitions from the shared native Codex configuration reader, plus bounded `codex mcp list/get --json` enrichment for plugin-provided servers, with secret-safe diagnostics. |
| `instructions.rs` | Assembles the agent brief + environment + saved state + skills + project doc. Multi-project initialization emits a project-neutral selector brief because conversation metadata is available only on subsequent tool calls; `get_agent_brief` builds the full brief after restoring or creating a binding. |
| `environment.rs` | OS / shell / policy description, shared by `get_environment` and the instructions. |

---

## 7. MCP bridging (aggregator)

codexify can act as an MCP **client** to local stdio and remote Streamable HTTP
servers, discover their tools at startup, and re-expose them. Implemented in `bridge.rs`; wired in
`server.rs::start_http_server` before the HTTP server starts.

### Discovery
Before bridging, `config.rs` imports compatible `[mcp_servers.<name>]` entries
from Codex's user-level config. Unless `codexMcp.useCli = false`, it then runs
`codex mcp list --json` and fetches each additional server with `codex mcp get`
so plugin-provided enablement and tool filters are preserved. `CODEX_CLI_PATH`
or `codexMcp.cliPath` selects the executable; otherwise `codex` is resolved from
`PATH`. Missing or incompatible CLI discovery is a warning in the default auto
mode and a startup error when `--codex-cli` is supplied. Explicit
`codex.config.json.mcpServers` fields overlay the combined imports with the same
name; `codexMcp.enabled = false` disables automatic Codex import unless
`--codex-cli` explicitly requires it. Non-local Codex execution environments and
`http_headers_helper` are reported and skipped because Codexify cannot delegate
transport execution to a Codex executor or helper process.

For each resulting server entry (sorted, non-disabled), `connect_one`:
1. Selects stdio from `command`, or Streamable HTTP from `url`.
2. For stdio, launches the child through `TokioChildProcess`; for HTTP, resolves
   the bearer-token environment variable and environment-backed headers, then
   builds `StreamableHttpClientTransport` with RMCP's redirect-disabled reqwest client.
3. Runs the MCP handshake (`().serve(transport)`), then `list_all_tools()`, under
   `startupTimeoutSec` (20 s by default).
4. Applies the optional `tools` allow-list, then `disabledTools` deny-list.

Failures are **reported, not fatal** — each server appears in the startup banner
as `-> N tool(s)`, `-> FAILED: <reason>`, `-> disabled`, or
`-> gateway (N functions via <tool>)`. The `RunningService` handles are kept in
`Bridge.services` for the whole server lifetime (dropping one closes the HTTP
session or kills its child).

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
The upstream client supports the two transports exposed by current Codex:

- **stdio**, inferred from `command`;
- **Streamable HTTP**, inferred from `url`, with `http`, `streamable-http`, and
  `streamable_http` accepted as explicit aliases.

HTTP configuration supports `bearerTokenEnvVar`, static `httpHeaders`,
environment-backed `envHttpHeaders`, `startupTimeoutSec`, and `toolTimeoutSec`.
Environment-backed headers override static headers. Tool timeouts use RMCP's
cancellable request handle so timeout cleanup sends MCP cancellation and removes
request bookkeeping. Legacy SSE and WebSocket types are rejected explicitly.

OAuth login/token persistence, `http_headers_helper`, remote Codex execution
environments, MCP resources/templates/prompts, upstream initialization
instructions, and dynamic capability forwarding are not part of this
tool-transport bridge.

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
  "multiProject": false,               // or --multi-project; work-dir becomes access root
  "allowedCommands": ["git", "node", …],   // run_command allowlist
  "allowedHosts": [],                  // DNS-rebinding allowlist; empty = any host
  "openaiTunnel": {
    "tunnelId": "tunnel_0123456789abcdef0123456789abcdef",
    "apiKeyRef": "env:CONTROL_PLANE_API_KEY",
    "clientPath": "…",                // optional; otherwise verified managed install
    "organizationId": "org_…"          // optional
  },
  "tree":   { "defaultDepth": 3, "ignore": ["node_modules", ".git", …] },
  "command":{ "defaultTimeout": 30000, "maxTimeout": 120000 },   // ms
  "exec":   { "mode": "allowlist"|"unrestricted",
              "extraAllowedCommands": ["ls", "cat", …], "maxSessions": 8,
              "defaultShell": "…" },
  "ignore": { "useGitignore": true, "useDefaultPatterns": true, "customPatterns": [] },
  "output": { "maxFileLines": 1000, "maxFileBytes": 131072, "maxEntries": 500, "maxTreeNodes": 1000 },
  "review": { "maxPatchBytes": 524288 },
  "audit": { "logFile": null, "includeCommandPreview": false,
             "commandPreviewMaxBytes": 512, "redactEnv": [] },
  "artifactIngress": { "enabled": true, "maxFileBytes": 104857600,
                       "requestTimeoutMs": 120000, "idleTimeoutMs": 30000,
                       "maxRedirects": 3, "maxConcurrentDownloads": 2,
                       "allowedHosts": ["*"] },
  "projectDoc": { "maxBytes": 32768, "fallbackFilenames": [], "rootMarkers": [".git"] },
  "memory": { "enabled": true, "dir": "…", "maxBytes": 16384 },
  "skills": { "enabled": true, "dirs": ["…"], "includePlugins": true },

  "mcpServers": {
    "local-exec": {
      "command": "D:\\mcphub\\mcp-server-windows-x86_64.exe",
      "args": [], "env": {},
      "type": "stdio",
      "disabled": false,
      "tools": ["exec", "machine_list"],   // optional allow-list of upstream names
      "mode": "gateway"                // or omit for "direct"
    },
    "remote-docs": {
      "url": "https://mcp.example.com/mcp",
      "bearerTokenEnvVar": "REMOTE_MCP_TOKEN",
      "httpHeaders": { "X-Client": "codexify" },
      "envHttpHeaders": { "X-Tenant": "REMOTE_MCP_TENANT" },
      "startupTimeoutSec": 20,
      "toolTimeoutSec": 60
    }
  }
}
```

---

## 10. Startup & diagnostics

The banner is designed so failures are never silent:

```
Config: D:\codex-bridge\codex.config.json          ← which file actually loaded
Tools loaded (27): 26 native + 1 bridged from upstream MCP servers
Upstream MCP servers:
  remote-exec -> gateway (84 functions via `remote_exec`)
Auth: disabled (no --api-key)
Audit log: /private/path/tools.jsonl
Audit command previews: disabled
```

- `Config:` reveals the common mistake of editing a different file than the one
  loaded (config is resolved relative to the launch directory unless `--config`).
- The `Upstream MCP servers:` block reports each server's outcome.
- In native tunnel mode, the banner also reports loopback-only exposure, the
  internal-auth boundary, managed runtime version or operator-supplied client,
  and local `/readyz` and `/metrics` URLs. The tunnel ID, runtime key, and
  internal bearer are never printed.
- Multi-project startup also prints `Project access root:`, `Project mode:
  persistent ChatGPT conversation binding`, and the conversation-binding state
  directory; its native count is 29 because the selectors are present.
- `-v` and `-vv` increase Codexify diagnostics without dumping raw tool payloads;
  `RUST_LOG` overrides those defaults. When audit logging is configured, the banner
  prints its destination and whether command previews are enabled.

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

- **519 tests** — unit tests inside modules plus integration tests under `tests/`
  (`tempfile`-isolated), ported from the TS Bun suite.
- Memory / skills tests pin `memory.dir` / `skills.dirs` to temp dirs so they never
  touch the real home; plugin discovery is suppressed when `skills.dirs` is set.
- `tests/project_selection.rs` covers pre-selection blocking, immutable canonical
  bindings, concurrent session isolation, traversal and symlink escapes,
  project-keyed persistent state, deferred project instructions, and CLI/config
  activation.
- `tests/review_checkpoints.rs` uses real repositories to cover monorepo scoping,
  byte-for-byte real-index preservation, persistent and transport owners, live ref
  reset, mutation/review serialization, incremental baselines, unborn repositories,
  malformed Git state, renames, deletions, binaries, relative patches, and patch-budget omission.
- `artifact_ingress` unit tests use scripted response bodies and capability-rooted
  temporary directories to cover provider-host validation, redirects, declared
  and streamed size limits, idle and caller cancellation, exact hashes, partial
  cleanup, symlink escapes, no-overwrite publication, and concurrent writers.
- `tests/review_fixes.rs` locks the behavioral-fidelity fixes found by the
  adversarial review of the port. The bridge/gateway/skills code was reviewed the
  same way; the confirmed low-severity findings (name-collision dedup, YAML-safe
  generated frontmatter, non-object `arguments` rejection, version ordering) are
  fixed with regression tests in `bridge.rs` / `skills.rs`.
- `examples/mock_mcp.rs` is a minimal stdio MCP server used to exercise the bridge
  end-to-end.
- `bridge.rs` starts a loopback Streamable HTTP MCP server to verify bearer and
  custom headers, remote discovery/calls, and cancellable tool deadlines.

Run: `cargo test`. Build a standalone binary: `cargo build --release`.

---

## 13. Troubleshooting

| Symptom | Cause / fix |
|---------|-------------|
| Bridged tools don't appear; banner shows `-> FAILED` | For stdio, verify that `command` is runnable on the machine where Codexify runs. For Streamable HTTP, verify the URL, TLS trust, bearer/header environment variables, and upstream authentication. |
| Banner shows a server you didn't configure (e.g. `idasql -> disabled`) | codexify loaded a *different* `codex.config.json` than you edited. Check the `Config:` line and edit that file, or pass `--config`. |
| codexify exposes a newer tool set but the client still shows the old one | The client caches the tool manifest — **remove and re-add the connector** so it re-fetches `tools/list`. |
| A client won't surface a large bridged set at all | Use `"mode": "gateway"` to collapse the server into one tool + a skill, or `"tools": [...]` to expose a curated few. |
| Upstream uses `type: "sse"` or `"websocket"` | Current Codex transport parity is stdio plus Streamable HTTP. Point the entry at a Streamable HTTP endpoint and use `url` (or an HTTP type alias). |
| Native tunnel never becomes ready | Check the banner's loopback `/readyz` and `/metrics` URLs and the redacted startup error. Codexify requires runtime readiness plus one successful control-plane poll; the runtime key needs the applicable Tunnels **Read** + **Use** permissions. The runtime-only binary has no `/ui` or `/api/status` surface. |
| Native tunnel key is rejected before startup | `apiKeyRef` must be `env:NAME` or `file:/path`. The referenced value must exist; on Unix, key files must not grant group/other access. |
| `import_host_file` is missing | `artifactIngress.enabled` is false, or the connector cached an older manifest. Enable it and remove/re-add the connector so ChatGPT refreshes `tools/list`. |
| Native file import reports an untrusted URL | The supplied value was not a ChatGPT-native file parameter or its temporary provider URL no longer matches the supported OpenAI file-service boundary. Reattach or regenerate the file instead of passing a URL manually. |
