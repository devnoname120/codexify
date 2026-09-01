# codexify — Architecture & Design

A Rust port of the `codexify` MCP bridge. codexify is a local [Model Context
Protocol](https://modelcontextprotocol.io) server that exposes Codex-style agent
tools over **Streamable HTTP**, scoped either to one configured working directory
or to a project root selected independently by each ChatGPT conversation, and can
additionally **aggregate local or remote MCP servers**, **surface local skills**,
materialize ChatGPT-native files inside the active project, and return project files
to ChatGPT as downloadable MCP resources.
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
│    • 28 native tools                          │
│    • + setup authorization wire tool          │
│    • + list_projects + set_project_root       │
│      in multi-project mode                    │
│    • 4 catalog tools ← private upstream index │
│    • direct tools    ← upstream MCP servers   │
│    • gateway tools   ← upstream MCP servers   │
│                                               │
│  shared ProjectBindingStore                   │
│    • openai/session hash → project root       │
│    • persistent atomic binding records        │
│                                               │
│  shared ConversationAuthorizationStore        │
│    • openai/session hash → authorized          │
│    • token-scoped persistent markers          │
│                                               │
│  shared ConversationExecSessionStore          │
│    • openai/session hash → resident commands  │
│    • in-memory ownership + idle cleanup       │
│                                               │
│  shared DiffCheckpointManager                 │
│    • project-open + last-diff snapshots       │
│    • scoped Git refs + MCP Apps resource      │
│                                               │
│  shared ArtifactEgressStore                   │
│    • durable opaque native capabilities       │
│    • global per-user disk snapshots + LRU     │
│    • safe latest-source fallback              │
│                                               │
│  optional shared AuditLogger                  │
│    • redacted JSONL tool lifecycle records    │
│    • no raw arguments or returned output      │
│                                               │
│  per-transport SessionState                   │
│    • fallback root for generic MCP clients    │
│    • auth + exec + plan + diff fallback       │
└──────────────────────────────────────────────┘
        │ reads/writes           │ stdio / Streamable HTTP
        ▼                        ▼
   active project root     upstream MCP servers (idasql, remote-docs, …)
```

Five integration surfaces are exposed to the client:

- **Tools** — `tools/list` + `tools/call` (native, fixed catalog discovery/call,
  direct compatibility proxies, and gateway compatibility dispatchers).
- **Skills** — a catalogue in the server `instructions` plus the `skills_list` /
  `skills_read` tools, discovered from disk.
- **Instructions** — the agent brief + environment + memory + project doc,
  rebuilt from the active project config.
- **Conversation authorization** — an optional authentication-token gate whose
  durable grant is keyed by ChatGPT's stable conversation metadata rather than
  by the replaceable MCP transport.
- **Resources / MCP Apps** — the self-contained diff resource linked from
  `show_diff`, the setup status/dashboard resource linked from `setup`, and the
  restart-tolerant updater resource linked from `self_update`. Their private
  payloads and operational timing arrive through component-only result metadata,
  alongside opaque exported-file resources linked from `export_host_file`
  and opaque capabilities replacing resource links returned by bridged MCP tools.
  `resources/read` resolves durable native records to retained immutable disk
  snapshots or a safely revalidated current source path, while bridged references
  proxy the read to the originating upstream peer. Unsupported clients ignore diff
  UI metadata and keep the concise ordinary text result; a client must support
  resource links to retrieve file bytes.

---

## 2. Request lifecycle

1. A client opens an MCP session with `POST /mcp` (`initialize`). rmcp's
   `StreamableHttpService` manages the session and calls the **service factory**
   once per session, producing a fresh `CodexHandler`.
2. `CodexHandler::get_info` returns the negotiated protocol version, capabilities
   (`tools`), server identity, and the `instructions` string. With
   `conversationAuthToken`, initialization exposes only the model-facing gate protocol;
   project context is loaded later through `get_agent_brief`. Otherwise,
   single-project mode builds the full project-aware brief immediately, while
   multi-project mode emits only the root-selection protocol and a project-neutral
   environment because ChatGPT's conversation identity arrives in request `_meta`
   on tool calls, after initialization.
3. `tools/list` → `CodexHandler::list_tools` maps the shared tool registry into
   rmcp `Tool` definitions, including mandatory titles, complete behavioral annotations,
   icons, input/output schemas, OpenAI file-parameter metadata, and MCP Apps
   resource metadata. Transitive definitions held by the private MCP catalog are
   deliberately absent from this registry. `resources/list` exposes the embedded
   diff, setup, and updater HTML resources. `resources/read` serves those static
   resources, resolves opaque exported-file capabilities returned by
   `export_host_file`, and proxies opaque capabilities created from bridged
   upstream `resource_link` results; those dynamic references are intentionally
   not enumerable. For MCP 2026-07-28 and newer, every Codexify-owned cacheable
   list/read response carries `ttlMs: 0` and `cacheScope: "private"`; older peers
   keep the historical field-omitting wire shape. Bridged resource reads retain
   any stricter capability-bounded upstream TTL because the protocol boundary only
   fills missing cache hints.
4. `tools/call` → `CodexHandler::call_tool` reads `openai/session` from rmcp's
   `RequestContext::meta`. rmcp moves wire-level request `_meta` into that context
   before dispatch, so the typed tool parameters are not the authoritative source.
5. When conversation authorization is configured, the intentionally innocuous
   `setup` wire tool compares the authentication token submitted as `ref` without
   echoing it and records only the authorization decision. Its same response also
   carries the cached connector-version echo, running server version, and a
   bounded `gh`-first/GitHub-API-fallback release check for the setup MCP App.
   Every other model-visible tool fails
   before project resolution or dispatch until the hashed ChatGPT conversation is
   authorized. The marker survives server restarts and is namespaced by the
   canonical work directory and current token, so rotation invalidates prior
   grants. Clients without `openai/session` receive a transport-local fallback
   grant.
6. In multi-project mode, `list_projects` may run before selection. It rebuilds a
   read-only catalogue from the user-level native Codex `[projects]` table and the
   static `projectCatalog.entries` overlay, canonicalizes and filters candidates
   against the access root, and returns relative selectors without reading project
   content or creating a binding.
7. `set_project_root` canonicalizes an existing directory below the configured
   access root, parses a provider-agnostic HTTPS/SSH Git repository URL ending in
   `.git`, or parses a supported GitHub repository, branch, pull-request, or commit
   URL.
   URL resolution first looks for a matching local Git top level; when none exists,
   it clones beneath `projectCloneDir` and verifies the resulting remote. A branch
   URL fetches `refs/heads/<branch>` and a PR URL fetches
   `refs/pull/<number>/head`; a commit URL fetches its full object ID. If an existing
   source checkout is not already at the fetched commit, the binding path creates a
   detached managed worktree at that commit rather than moving the source checkout;
   worktree mode `Never` rejects that case. With `openai/session`, the immutable
   selection is written through the shared `ProjectBindingStore`; without it, it is
   stored in the current `SessionState`. Re-selecting the same canonical root or
   exact URL selection is idempotent, selecting a different one is rejected before
   cloning or fetching, and a clone destination collision never overwrites existing
   data.
8. Other project-scoped calls resolve the durable conversation binding first, or
   the transport-session fallback when no conversation identity exists, then
   receive an effective clone of `AppConfig` whose `work_dir` is that root. Before
   the first such call, the diff manager captures the scoped project-open snapshot.
   Non-Git projects report diff capture as unavailable; a Git snapshot failure blocks
   mutating tools before dispatch.
9. Tool dispatch first validates arguments against the cached JSON Schema compiled
   when the registry was built. Native fixed-shape argument objects are closed;
   validation failures are returned before project resolution or side effects, and
   diagnostics mask values covered by `writeOnly`. Dispatch then resolves a
   `ToolCallIdentity`. Native tools retain their
   downstream name; direct, gateway, and catalog MCP proxies map the call to the
   raw configured upstream server/tool before execution. Dispatch then supplies a
   request context containing the stable conversation identity, any
   feature-detected ChatGPT connector identifier, shared
   authorization store, and shared diff manager; tools that do not need it use
   the default context-free implementation. The server applies the model-output
   policy to textual `content` and explicit `structuredContent`, then fills default
   structured text mirrors within the same ceiling. A successful result is checked
   against the tool's cached output validator before it leaves the server. Component-only
   result `_meta` is deliberately excluded. Every tracing lifecycle event includes the resolved
   identity. Dispatch assigns one server-wide call ID before observers run. When
   top-level `debug` is enabled it also appends bounded tool name/version/duration
   metadata for MCP Apps without exposing arguments or result payloads.
   Optional `toolLogging` tracing emits one correlated start and completion record
   for every tool class, with independently selectable request
   and response payloads, configurable severity, lazy redaction, and work-bounded
   UTF-8 previews. Separately, audit logging emits JSONL `tool_start` / `tool_finish`
   records with the same call ID and resolved MCP identity: conversation and project identities
   are hashed, and scalar argument values and returned payloads are replaced by
   schema-bounded shape and size accounting (unknown argument keys and dynamic-map
   keys are not recorded).
10. `exec_command` and `write_stdin` opt into resident-process routing. With a
   ChatGPT conversation identity, dispatch substitutes the shared conversation's
   exec state while retaining the transport's other mutable state. Without one,
   the ordinary transport-owned state is used.
11. When a transport session ends, rmcp drops the `CodexHandler`: its
    generic-client exec state loses its last owner and kills resident process
    trees. Conversation-owned process state remains in the server for later
    connector calls, subject to idle cleanup; server shutdown drops the shared
    store and kills anything still running. Project bindings, conversation
    authorizations, and ChatGPT diff refs are independently durable on disk across server
    restarts, but process handles are not.

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
    fn title(&self) -> String;                          // mandatory host-facing title
    fn behavior(&self) -> ToolBehavior;                // four MCP hints + justification
    fn meta(&self) -> Option<MetaObject>;               // MCP Apps and extensions
    fn input_schema(&self) -> Value;
    fn requires_closed_input_schema(&self) -> bool;     // true for native tools
    fn call_identity(&self, args: &Value) -> ToolCallIdentity; // resolved native/MCP target
    fn output_schema(&self) -> Option<Value>;
    fn requires_closed_output_schema(&self) -> bool;    // true for native tools
    fn fills_structured_content(&self) -> bool;         // opt out of default-fill
    fn permits_missing_structured_content(&self) -> bool; // direct upstream compatibility
    fn validate_arguments(&self, args: &Value) -> Result<(), String>;
    fn validate_structured_output(&self, value: Option<&Value>) -> Result<(), String>;
    fn manages_model_output_budget(&self) -> bool;      // false by default
    fn requires_project_root(&self) -> bool;            // true by default
    fn uses_exec_session_state(&self) -> bool;          // false by default
    fn may_modify_project(&self) -> bool;               // fail closed on diff-capture errors
    async fn call(&self, args: Value, cfg: &AppConfig, session: &SessionState) -> ToolResult;
    async fn call_with_context(&self, args: Value, cfg: &AppConfig, session: &SessionState, context: &ToolRequestContext) -> ToolResult;
}
```

`ToolBehavior` makes `readOnlyHint`, `destructiveHint`, `idempotentHint`, and
`openWorldHint` explicit for every tool and keeps a non-empty classification rationale
beside the implementation. `ValidatedTool` compiles Draft 2020-12 input/output
validators once, rejects malformed native descriptors at startup, and enforces the
same contract at the dispatch boundary. Invalid third-party direct descriptors are
skipped with a warning rather than preventing valid native tools or sibling MCP
sources from starting. Native argument structures may derive `Deserialize` and
`JsonSchema`; `schema_for` and `parse_tool_args` keep the wire schema and parser on
the same Rust type where a tool does not require a host-specific dynamic schema.

### `ToolResult` (`types.rs`)
`{ content: Vec<ToolContent>, is_error: bool, structured_content: Option<Value>, meta: Option<MetaObject>, audit: ToolAuditMetadata }`.
The server converts it to rmcp's `CallToolResult`. Tools with an exact `outputSchema`
whose text *is* the structured form rely on the server's default-fill
(`{ "content": <joined text> }`); tools that build their own structured content —
or bridge it from upstream — return `fills_structured_content() == false`.
Before conversion, the server applies `output.maxToolOutputTokens` to every
non-self-managed textual result. Default text mirrors are fitted after JSON
escaping. Oversized explicit structured data becomes an error response because
arbitrary partial JSON cannot be guaranteed to satisfy the advertised schema.
After budgeting and default-fill, successful structured content is validated; a
handler/schema regression becomes an explicit tool error rather than a malformed MCP
success response.
`exec_command` and `write_stdin` self-manage the policy around their nested output
receipt. Result `_meta`, images and resource links are not text-truncated.
`ToolAuditMetadata` is not sent over MCP; bounded-output and resident-process tools
use it to report truncation, original token count, exec-session ID and PID without
putting operational fields into their public output schema.

### `ProjectBindingStore` (`project_bindings.rs`)
Shared, durable conversation-to-project bindings. `ConversationIdentity` hashes
ChatGPT's `openai/session` value before it reaches a filename; no raw conversation
identifier is written to disk. Records are namespaced by canonical access-root
hash, written atomically under a per-record lock, and validated again on every
load so a deleted project or changed symlink fails closed. A new store instance
can recover the same binding after a server restart. URL-created bindings also
persist the canonical Git selection URL, allowing normalized equivalents of the
same repository—and equivalent encodings of an exact GitHub branch, PR, or commit
selection—to remain idempotent without another remote inspection.

### Git project resolution (`project_clone.rs`)
`ProjectReference` distinguishes ordinary filesystem selectors from HTTPS/SSH Git
repository URLs and GitHub's richer branch, pull-request, or commit URL forms.
Provider-agnostic selections require a repository-root URL ending in `.git`, which
keeps arbitrary provider web pages out of the clone surface; existing matching
remotes may omit that suffix. HTTPS, `ssh://`, and SCP-style SSH forms are accepted,
while embedded HTTPS credentials, local/file transports, HTTP, `git://`, queries,
fragments, and other transports are rejected. Generic repository identity is
host/path based and preserves the provider path's case. HTTPS is considered
equivalent to default-port SSH only for the conventional shared `git` account used
by hosting services; arbitrary SSH users, custom ports, and non-service SSH forms
remain distinct to avoid equating repositories whose server-side path semantics may
differ. GitHub repository roots retain their existing shorthand without `.git`,
normalize owner/repository identity case-insensitively, and additionally accept `/tree/<branch>`,
`/pull/<number>`, and `/commit/<40-character-hex-ID>` target URLs. Exact checkout
identity retains the case-sensitive branch name, PR number, or lowercase commit ID.

Resolution revalidates `projectCloneDir` beneath the canonical access root, then
checks the normal `<clone-dir>/<repository-name>` destination, live catalogue
candidates, and immediate clone-directory children by inspecting each Git top
level's remotes. One match is reused; multiple matches require an explicit path.
When cloning is necessary, a repository-keyed cross-process lock serializes
requests, `git clone` runs non-interactively under a bounded timeout into a private
temporary directory, the remote is verified, and publication refuses an existing
destination. Binding locks are acquired and checked before this resolver runs, so
an already-bound conversation/session cannot produce a rejected clone side effect.
For an explicit checkout target, resolution fetches the exact GitHub ref or commit
ID into a private deterministic ref and resolves its commit. Fresh branch clones
check out the named branch; fresh PR and commit clones detach at the selected
commit. Existing matching source checkouts are only fetched, never checked out or
reset.

### `ConversationAuthorizationStore` (`conversation_auth.rs`)
The configured authentication token is validated at startup and compared in
constant time. A successful ChatGPT call writes a private authorization marker
whose filename uses the existing hashed `ConversationIdentity`; marker contents
contain only the grant. Markers are grouped under a digest of the canonical work
directory and current token, which avoids plaintext token storage and makes
rotation a new authorization namespace. The in-memory set avoids disk reads after
recovery. Generic clients have no durable conversation identity, so their grant
is an atomic flag shared only by the current `SessionState` views.

### Worktree isolation (`worktrees.rs`)
In multi-project mode, `set_project_root` can bind a conversation to a **detached
managed Git worktree** instead of the source checkout, so two chats editing the
same repository never share a working tree. `worktrees.mode` selects the policy:
`Never` binds the source root directly, `Always` requires a worktree, and `Auto`
(the default) uses an unassigned source checkout directly but creates a worktree
when that checkout is already assigned. An explicit branch, PR, or commit selection
also forces a worktree in `Auto` when the source is on another commit; `Never`
rejects that mismatch rather than mutating the source. Managed worktrees are
created with `git worktree add --detach` under `worktrees.root` (default: Codex's
configured worktree location), one per conversation-and-repository, at either the
ordinary starting commit or the exact fetched target commit.

The binding therefore carries two roots: `source_project_root`, the operator-
authorized directory beneath the access root, and `project_root`, the managed
checkout that becomes the effective `work_dir`. The managed worktree lives
*outside* the access root by design, so `effective_config` re-validates the pair
on every dispatch — `source_project_root` must still canonicalize to a path under
the access root (fails closed if a symlink or the directory moved), while
`project_root` is only required to still exist as a directory. Attacker-controlled
input never selects `project_root` independently: it is either the validated
source root or a path deterministically derived from `worktrees.root` plus the
repository's Git-relative workspace path.

Stale managed worktrees from ended conversations are swept on startup when
`worktrees.autoCleanupEnabled` is set, retaining the most recent `keepCount`
(default 15) and skipping any root still referenced by a live binding. On Windows
the extended-length `\\?\` prefix that `fs::canonicalize` returns is stripped
before the root is handed to `git worktree add`, which otherwise cannot create the
worktree's leading directories. Per-worktree Codex environment setup is a security
boundary of its own: the environment file is neither copied into the worktree nor
its setup script executed unless `worktrees.allowSetupScript` is explicitly `true`
(default `false`), because both the file and its script path are selectable through
the source repository's local Git config and the script runs outside the
`exec` policy.

### `ConversationExecSessionStore` and `SessionState` (`exec_sessions.rs`)
`SessionState` is the per-MCP-transport view: it owns the optional fallback
project root and authorization flag for generic clients, the current plan, a
transport-owned exec state, and the generic-client diff fallback. The exec state is
reference-counted and kills running process trees when its last owner disappears.

`ConversationExecSessionStore` is shared by all handlers in the server process.
For the two unified-exec tools, dispatch uses the hashed `openai/session` identity
to substitute conversation-owned exec state into a temporary `SessionState`
view. That state survives replacement transports, is isolated from other
conversations, and is removed after its sessions finish or expire. It is not
written to disk and therefore does not survive server restart.

### `DiffCheckpointManager` (`diff.rs`)
Shared project-diff state. ChatGPT owners are keyed by the existing hashed
conversation identity and persist two refs under `refs/codexify/diff/`; generic
clients use the transport state above. Snapshots seed a private index with only
tracked entries beneath the logical project path, refresh that scope from the
working tree, and create root commits without touching the user's index. Comparisons
repeat the same literal pathspec, return project-relative file records and patch
headers, and bound file and patch results. By default `show_diff` records the
emitted snapshot as the next incremental baseline through `git update-ref`
compare-and-swap. That cursor is connector-private bookkeeping, so the tool remains
annotated read-only: it does not modify project files, Git history, user-owned data,
or external systems. The same per-owner/project lock spans mutating tool calls through
completion so a diff cannot capture an in-process write halfway through. Existing
`refs/codexify/review/.../project-open` and `.../last-review` refs are copied lazily
into the diff namespace and retained for rollback compatibility.

### `AppConfig` (`types.rs`)
The fully-resolved config handed to every tool. `config.rs` selects one JSON file
from explicit `--config`, `CODEXIFY_CONFIG`, the user-level
`~/.codexify/codexify.config.json`, or built-in defaults, in that order. It parses
camelCase fields and resolves the active project from CLI `--work-dir` or the
config's absolute `workDir`. It imports user-level Codex MCP definitions through
`codex_mcp.rs`, opportunistically adds
plugin-provided entries from the Codex CLI's effective catalogue, then applies
explicit `mcpServers` entries as field overlays. Optional sub-configs (`projectDoc`,
`projectCatalog`, `output`, `diff`, `artifactIngress`, `artifactEgress`, `worktrees`,
`memory`, `skills`, `ignore`, `audit`) fall back to per-module defaults, as does `codexMcp`
(Codex MCP import and CLI enrichment). The former top-level `review` key remains a
deserialization alias for `diff`, but resolved runtime configuration uses only the
diff-named field. In multi-project mode, dispatch clones this
config per call and substitutes the conversation's selected root—or the transport
fallback—for `work_dir`; the static server policy, catalogue overlay, and bridge
configuration remain shared. Native Codex project entries are intentionally re-read
when the catalogue tool is called rather than copied into `AppConfig` at startup.
`conversationAuthToken` is a top-level optional authentication secret with no CLI
override; its presence also controls registry inclusion of the authorization gate
and authorization-only initialization instructions. Only the ChatGPT-facing MCP
surface disguises that gate as `setup(ref)`: ChatGPT can otherwise mistake a
token-looking connector call for a dangerous secret leak and refuse it. The token
itself is a 64-character lowercase hexadecimal string shaped like SHA-256, not a
digest of a different configured secret, and `ref` carries that exact token. These
wire names are a compatibility workaround for ChatGPT's account-level connector
OAuth and safety behavior; the implementation, persistence, and operator-facing
documentation continue to treat the value as an authentication token and the
result as a conversation authorization grant.

### `quickstart` CLI (`quickstart.rs`)
The `quickstart` subcommand runs before server configuration is loaded. It uses a
testable line-oriented wizard for ordinary prompts and terminal-hidden input for
the runtime API key. Without an explicit CLI or environment override, it writes
`~/.codexify/codexify.config.json` and its generated launch command relies on normal
user-config discovery rather than adding `--config`. The wizard canonicalizes the
project directory, stores it as absolute `workDir`, validates the tunnel credentials
with the same helpers as normal startup, merges only the managed fields into the
existing JSON object, and stores the key outside the project behind an absolute
`file:` reference. Config and
credential replacement use temporary files in the destination directory. Once
setup is complete, quickstart either restarts an installed native service with the
selected config or passes the selected work directory through `load_config` before
entering the foreground server lifecycle.
The wizard does not expose the advanced `conversationAuthToken` policy as an
onboarding choice. If an existing config already contains a valid token, it
preserves the token, protects the config as a private file on Unix, and prints the
one-line instruction needed by an individual chat or ChatGPT Project.

### `doctor` CLI (`doctor.rs`)

`doctor` is a read-only preflight/support surface built on the same configuration
resolver as server startup, with announcements suppressed so `--json` can emit one
stable document. The report distinguishes pass, warning, failure, and skipped
checks and exits nonzero only after the complete report has been written when a
failure is present.

The command owns a platform-aware executable resolver for absolute/relative
command paths and PATH lookup (including PATHEXT on Windows). It follows Codex's
diagnostics for Git and ripgrep, additionally warns when `gh` is unavailable,
validates the resolved `exec_command` shell, classifies optional versus required
Codex CLI enrichment, and checks enabled local stdio MCP commands without starting
them. MCP resolution honors each server's cwd and PATH override.

The remaining checks reuse narrow read-only helpers rather than lifecycle paths:
`service.rs` reports native-manager state, `self_update.rs` inspects the bounded
update lock, and `openai_tunnel.rs` validates credential references and installed
runtime integrity without downloading or starting the tunnel. A local HTTP probe is
made only for a running native service; it is fixed to IPv4 loopback, disables
redirects, uses a short timeout, bounds the response to 64 KiB while reading it,
and requires the server's `{ "status": "ok" }` health JSON.

### Native user service (`service.rs`)

The public `service install|enable|disable|remove|logs` commands manage one
per-user native definition: an XDG-aware systemd user unit on Linux, a launchd agent on
macOS, or an at-logon Scheduled Task on Windows. Installation stores the current
executable path and the selected config path as absolute arguments. The hidden
`service run` command is the native manager's entry point. The updater also uses
the hidden `service wait-ready` command to poll the loopback health endpoint under
a deadline after a restart.

The service runner does not construct a second server configuration. It waits if
the selected file does not exist, then starts the ordinary Codexify executable
with `--config <absolute-path>`. Child stdout and stderr are serialized into a
private 10 MiB log with five numbered generations. Child failures use bounded
exponential restart backoff; systemd, launchd, and Task Scheduler separately
restart the runner if the runner itself fails. On shutdown, the runner terminates
the server process group/tree before exiting. Windows children are assigned to a
kill-on-close Job Object so Task Scheduler termination cannot orphan the server
or its descendants.

macOS lifecycle changes are state-aware. `bootout --wait` is itself bounded so a
wedged launchd operation cannot hang an update, exit status 37 is treated as a
transitional `EALREADY` result and retried within a fixed window, and enable/start
re-evaluates whether the agent is loaded when a concurrent bootout or bootstrap
completes between commands.

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
  owns the generic-client root and authorization fallbacks, current plan, and generic-client
  resident commands. ChatGPT identity is taken from
  `RequestContext::meta["openai/session"]`: the persistent
  `ProjectBindingStore` resolves its project, the persistent
  `ConversationAuthorizationStore` resolves the optional token grant, and the in-memory
  `ConversationExecSessionStore` resolves resident commands across replacement
  transports. `ArtifactEgressStore` resolves native bearer capabilities through a
  global per-user durable record/snapshot store, while `BridgedResourceStore`
  preserves only short-lived upstream-resource routing capabilities across the
  same replacement transport. Upstream MCP connections, these stores, and the
  tool registry are shared (`Arc`) across transports.
- **`get_info`** advertises server name `codexify` (wire-compatible identity),
  version, tools/resources capabilities, the `io.modelcontextprotocol/ui` extension,
  and the `instructions`. The diff resource is embedded in the binary and has no
  external network or asset dependency.
- **Tool descriptors and results** preserve generic titles, MCP annotations and
  `_meta` extensions. `import_host_file` uses the descriptor path for
  `_meta["openai/fileParams"]`; `export_host_file` returns a standard
  `resource_link`. Bridged `resource_link` blocks are converted to the same
  downstream content type after their upstream URI is replaced by an opaque
  `codexify://upstream-resource/...` capability. `resources/read` resolves native
  durable records to an immutable snapshot or a safely revalidated source file,
  and forwards bridged capabilities to their originating peer. The conversion
  layer has no tool-name special case.
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

The Codexify `/health` endpoint returns success only after this startup sequence
has completed. It returns `503 Service Unavailable` while the native tunnel is
starting or is known to be unhealthy, and returns to success after supervision
restores the tunnel. Non-tunnel mode is ready as soon as the HTTP listener starts.

Codexify watches the HTTP server, tunnel child, `SIGINT`, and `SIGTERM`
concurrently. Failure of either process shuts down the other. Normal shutdown
sends `SIGTERM` on Unix, waits under a deadline, then force-kills if necessary;
Windows uses the child-process kill path. The MCP cancellation token and Axum
graceful-shutdown signal are triggered together; lingering HTTP connections are
aborted after a bounded grace period. Runtime logs and health URL files live in
a private per-run temporary directory and are removed after shutdown.

---

## 5. Native tools (32 advertised default, 34 multi-project)

| Group | Tools |
|-------|-------|
| File / code | `read_file`, `write_file`, `import_host_file`, `export_host_file`, `apply_patch`, `glob`, `grep`, `list_directory`, `tree`, `view_image` |
| Commands | `exec_command` / `write_stdin` (one-shot or resident shell sessions) |
| Git / diff | `git_status`, `show_diff`, `git_push`, `git_commit`, `git_log` |
| Environment / project | `get_environment`, `get_project_doc`, `get_agent_brief` |
| Diagnostics / updates | `self_update`; app-only `doctor`, `self_update_status` |
| Task state | `update_plan`, `remember`, `update_memory_note`, `forget_memory_note`, `recall` |
| Skills | `skills_list`, `skills_read` |
| Timing | `clock_curr_time`, `clock_sleep` |
| Project selection (multi-project only) | `list_projects`, `set_project_root` |

The default registry has 30 model-visible tools plus the app-only `doctor` and
`self_update_status` tools. Multi-project mode prepends `list_projects` and
`set_project_root`. Conversation authorization prepends the ChatGPT-facing
`setup` wire tool, raising both advertised and model-visible counts by one.
`artifactIngress.enabled = false` independently removes `import_host_file`, while
`artifactEgress.enabled = false` independently removes `export_host_file`; each
reduces the applicable count by one. Project-catalogue discovery and
selection, clocks, the four transitive-MCP catalog tools, and direct/gateway
compatibility tools are project-independent; every other native tool is blocked
until a conversation binding or transport fallback is available.

Each lives in `src/tools/<name>.rs`; the registry (`registry.rs`) lists them in
the original order and rejects duplicate names.

---

## 6. Infrastructure modules

| Module | Responsibility |
|--------|----------------|
| `safe_path.rs` | Lexical path-traversal guard (no `canonicalize`; component-wise containment) used by the ordinary filesystem tools. Native file ingress and egress use their own capability-confined boundaries below. |
| `artifact_ingress/` | OpenAI native-file validation and streaming plus capability-confined, atomic no-overwrite workspace publication. It never accepts a local source path, and constrains the download URL and every redirect hop to the configurable `artifactIngress.allowedHosts` allowlist (default `"*"`, which still rejects loopback, private, link-local, unique-local, CGNAT, `localhost`, and metadata addresses). |
| `artifact_egress.rs` | Capability-confined regular-file snapshotting plus a process-wide bounded store of immutable bytes. It returns random opaque `codexify://artifact/...` resource capabilities, enforces per-file/total-byte/reference/TTL limits, and serves blobs only through `resources/read`; absolute paths, traversal, symlink escapes, and delayed path rereads are excluded. |
| `bridged_resources.rs` | Process-wide opaque capability registry for `resource_link` blocks returned by upstream MCP tools. It binds a random `codexify://upstream-resource/...` token to the originating peer and URI without caching the payload, forwards `resources/read` with cancellation/timeout handling, applies artifact-egress file/reference/TTL limits, and rewrites returned content URIs so upstream capabilities never become downstream routing tokens. |
| `logging.rs` | Tracing initialization with default filters for normal, `-v`, and `-vv` operation (an explicit `RUST_LOG` remains authoritative), plus a non-overridable filter that suppresses RMCP framework events, preventing native-file bearer URLs from appearing in logs before tool dispatch or malformed-session errors. |
| `tool_logging.rs` | Opt-in configurable-level invocation tracing for every native and bridged tool, monotonic call pairing, resolved raw MCP identities, compact MCP-shaped response rendering, MCP image content-block and resource-capability elision, a serializer that stops at the byte budget, and UTF-8-safe prefix markers. |
| `redaction.rs` | Shared value/JSON/argv redaction for audit command previews and tool payload tracing. It collects configured and environment-backed connector/MCP/tunnel secrets, removes secret/checksum-labelled fields and native-file capability URLs, applies schema sensitivity and credential-syntax heuristics, and supplies lazy serialization wrappers so payload redaction does not require cloning the full value. |
| `output_budget.rs` | Line/byte windowing and list caps, each cut announced with the continuation argument. |
| `audit.rs` | Private append-only JSONL tool lifecycle records, resolved raw MCP identities, stable hashed conversation/project identities, redacted argument summaries, output accounting, and opt-in bounded command previews using the shared redactor. |
| `conversation_auth.rs` | Authentication-token generation and validation, constant-time comparison, copyable ChatGPT instruction rendering with the innocuous wire vocabulary, durable per-conversation authorization markers, and transport-session fallback. |
| `ignore_rules.rs` | One `.gitignore`-accurate matcher (the `ignore` crate) shared by glob/grep/tree/list_directory. |
| `exec_policy.rs` | Shell-string allowlist guard for `exec_command` (a guardrail, not a sandbox). |
| `project_bindings.rs` | Canonical project-root validation plus durable ChatGPT conversation bindings keyed by a hash of `openai/session`, namespaced by access root, locked per record, and atomically written. |
| `project_clone.rs` | Strict provider-agnostic HTTPS/SSH Git repository URL parsing plus GitHub branch/PR/commit target parsing, conservative normalized remote matching, existing-checkout discovery, exact GitHub target-ref or object-ID fetching, bounded non-interactive cloning below `projectCloneDir`, cross-process repository locks, collision refusal, and post-clone verification. |
| `worktrees.rs` | Per-conversation managed Git worktree lifecycle: create a detached checkout under `worktrees.root` via `git worktree add`, optionally at an exact fetched commit, dual source/worktree root tracking, startup sweep bounded by `keepCount`, Windows `\\?\`-prefix handling, and the opt-in `allowSetupScript` gate for per-worktree environment setup. |
| `project_catalog.rs` | Live, read-only project discovery from native Codex plus explicit metadata; canonical access-root filtering, deduplication, deterministic query ranking, sanitized MCP warnings, and local diagnostics. |
| `exec_sessions.rs` | Generic-client transport fallback plus conversation-owned unified-exec sessions and transport-local diff state: trusted configured-shell resolution, Codex-compatible model shell-type selection by basename, PowerShell exit-code wrapping, background stdout/stderr drain tasks, process-group kill, idle cleanup, and output truncation (UTF-16 units to match the TS). |
| `diff.rs` | Project-scoped Git snapshots, persistent conversation refs, transport-local fallbacks, incremental comparisons whose emitted snapshot can advance the private diff cursor through compare-and-swap, legacy review-ref migration, diff parsing and component-payload budgets. |
| `diff_ui.rs` | Embedded MCP Apps resource, component-only diff-result metadata, legacy review-card compatibility, and persisted private interaction state for the interactive `show_diff` card. |
| `setup_ui.rs` / `setup_ui.html` | Compact setup/status MCP App with cached-bypass update checks, background structured doctor diagnostics, conversational Refresh/Autofix handoffs, and debug timing display. |
| `self_update.rs` | Verified release resolution, checksum validation, bounded executable/changelog extraction, private durable updater records, and generated rollback-capable OS worker scripts. |
| `self_update_ui.rs` | Embedded updater MCP App, component-only changelog payload, restart-tolerant polling, absolute timeout state, and app-only status-tool integration. |
| `widget_debug.rs` | Small component-only timing metadata shared by all tool results when top-level debug mode is enabled. |
| `apply_patch.rs` | The Codex patch format: verify the whole patch, then apply file operations sequentially with Codex-compatible partial-failure behavior, fuzzy context matching and CRLF preservation. |
| `tool.rs` | Mandatory titles and `ToolBehavior`, typed-schema helpers, startup descriptor checks, cached dialect-aware JSON Schema validators, masked input diagnostics, and successful structured-output validation. |
| `memory.rs` | Working memory outside the repo, keyed by a hash of the normalized active root, with `O_EXCL` locking and atomic writes. In multi-project mode, a configured `memory.dir` is a base containing one hashed child per project. |
| `quickstart.rs` | Interactive first-install wizard for project scope, native tunnel credentials, JSON config merging, preservation of preconfigured advanced conversation authorization, and the ChatGPT developer-mode connector handoff. |
| `doctor.rs` | Read-only local diagnostic orchestration, deterministic human/JSON reports, Codex-aligned Git/ripgrep probes, GitHub CLI/shell/Codex CLI/MCP command resolution, bounded GitHub release-freshness and loopback health probes, and service/update/tunnel prerequisite checks. |
| `service.rs` | Per-user systemd, launchd, and Windows Task Scheduler definitions; state-aware launchd transitions; bounded child restart and readiness supervision; private rotating stdout/stderr logs; and `service logs [-f]`. |
| `openai_tunnel.rs` | Verified installation and lifecycle supervision for OpenAI's outbound Secure MCP Tunnel runtime. |
| `process_env.rs` | Child-process environment boundaries: isolate the tunnel runtime and remove tunnel credentials from model-controlled and upstream subprocesses. |
| `project_doc.rs` | `AGENTS.md` discovery from project root down to the work dir under a byte budget. Multi-project mode treats the selected directory as the exact project root and never walks into the common access-root parent. |
| `skills.rs` | `SKILL.md` discovery (see §8). |
| `codex_config.rs` | Shared secret-safe resolver and TOML reader for `$CODEX_HOME/config.toml` or `~/.codex/config.toml`. |
| `codex_mcp.rs` | Read-only import of local stdio and remote Streamable HTTP MCP definitions from the shared native Codex configuration reader, plus bounded `codex mcp list/get --json` enrichment for plugin-provided servers, with secret-safe diagnostics. |
| `mcp_catalog.rs` | Private transitive MCP source/tool catalogue, collision-safe model identifiers, weighted BM25 index, exact metadata/schema lookup, and the fixed source/search/get/call tools. |
| `instructions.rs` | Assembles the agent brief + environment + saved state + skills + project doc. Authorization-enabled initialization emits only the intentionally innocuous model-facing gate protocol; otherwise multi-project initialization emits a project-neutral selector brief because conversation metadata is available only on subsequent tool calls. `get_agent_brief` builds the full brief after authorization and project resolution. |
| `environment.rs` | OS / shell / policy description, shared by `get_environment` and the instructions. |

---

## 7. MCP bridging (aggregator)

codexify can act as an MCP **client** to local stdio and remote Streamable HTTP
servers and materialize their complete filtered tool catalogues at startup.
`bridge.rs` owns transport connection/lifecycle and compatibility proxies;
`mcp_catalog.rs` owns the private catalogue, index, and fixed progressive-
disclosure tools. `server.rs::start_http_server` connects upstreams before it
constructs the downstream registry.

All exposure modes share one `BridgedResourceStore`. `bridge.rs` maps upstream
`resource_link` content through that store before returning a tool result, so
direct proxies, gateway dispatch and `mcp_call_tool` all produce the same opaque
downstream capability. `server.rs::read_resource` recognizes that capability and
routes the read back over the already-connected upstream `Peer`; the original
resource routing URI is replaced in the downstream link and in returned
`ResourceContents.uri` fields.

### 7.1 Configuration provenance and effective exposure

`config.rs` first imports compatible `[mcp_servers.<name>]` entries from Codex's
user-level config. Unless `codexMcp.useCli = false`, it then runs
`codex mcp list --json` and fetches additional servers with `codex mcp get` so
plugin-provided transport settings, enablement, and tool filters are retained.
`CODEX_CLI_PATH` or `codexMcp.cliPath` selects the executable; otherwise `codex`
is resolved from `PATH`. Missing/incompatible CLI discovery warns in automatic
mode and is fatal only when `--codex-cli` explicitly requires it.

Every `McpServerSpec` retains internal provenance after explicit
`codexify.config.json.mcpServers` overlays are applied:

| Provenance | Default exposure |
|------------|------------------|
| `Explicit` (standalone local entry) | `Direct` — backward-compatible flattening |
| `CodexConfig` | `Catalog` — private catalogue |
| `CodexCli` (including plugin-only servers) | `Catalog` — private catalogue |

The typed `mode` override (`direct`, `gateway`, or `catalog`) always wins. A
same-name explicit overlay preserves imported provenance unless it sets `mode`;
this allows bridge-only fields to be added without accidentally restoring a
large flattened capability set. Exposure is never inferred from tool count.

For each sorted, non-disabled effective server, `connect_one`:

1. Selects stdio from `command`, or Streamable HTTP from `url`.
2. For stdio, launches through `TokioChildProcess`. For HTTP, resolves bearer and
   environment-backed headers and builds a reqwest client with redirects disabled,
   preventing caller-supplied authorization/custom headers from being replayed to
   another target.
3. Runs the MCP handshake and complete paginated `list_all_tools()` under
   `startupTimeoutSec` (20 s by default).
4. Applies `tools` as an allow-list over raw names, then `disabledTools` as a
   deny-list.
5. Materializes the filtered definitions according to effective exposure.

Failures are reported rather than fatal. The banner distinguishes `direct (N
tool(s))`, `catalog (N private tool(s))`, `gateway (N functions via <tool>)`,
`disabled`, and `FAILED: <reason>`. `RunningService` handles remain in
`Bridge.services` for the entire downstream server lifetime; dropping one closes
the HTTP session or terminates its stdio child.

### 7.2 Catalog mode and ranked progressive disclosure

Each catalog-mode server becomes a private `CatalogSource` containing:

- its raw configured name, provenance, transport, implementation identity,
  initialization instructions, upstream `Peer`, and optional call timeout;
- every filtered raw `rmcp::model::Tool`, including title, description,
  input/output schemas, annotations, icons, and `_meta`;
- separate collision-disambiguated model-facing source/tool IDs. Raw names are
  retained and are the only names used for upstream dispatch.

All catalog sources share one immutable `Arc<Catalog>` and one set of four
downstream `Tool` objects. If no source uses catalog mode, these tools are absent:

| Fixed tool | Internal operation |
|------------|--------------------|
| `mcp_list_sources` | Returns source IDs plus raw names, provenance, transport, tool count, implementation metadata, and instructions; optionally token-filters sources |
| `mcp_search_tools` | Executes ranked full-text search across all documents or one source ID and returns compact matches |
| `mcp_get_tool` | Serializes the exact selected raw tool definition, augmented with its model-facing ID and source metadata |
| `mcp_call_tool` | Resolves source/tool IDs to stored raw names and invokes the selected upstream peer |

The local search index is weighted BM25 (`k1 = 1.2`, `b = 0.75`). A document
includes source ID/name/provenance/transport, upstream implementation metadata and
instructions, tool ID/raw name/title/description, and recursively useful
input/output-schema property names, descriptions, required names, and enum values.
Tool names receive the highest term weights, descriptions and schema text lower
weights, and exact normalized name/title matches receive deterministic boosts.
Search returns summaries; full schemas are loaded only through `mcp_get_tool`.

The fixed tool descriptions include a bounded source manifest, making available
systems visible in the downstream connector catalogue without placing every
transitive schema there. All four tools return project-independent exposure and
read-only discovery annotations except `mcp_call_tool`, which advertises
conservative potentially-destructive/open-world hints.

That conservative dispatcher annotation is an unavoidable semantic loss. The
downstream host approves one generic tool before its runtime `source`/`tool`
selection is known, so ChatGPT cannot enforce the selected upstream tool's
individual read-only/destructive/open-world hints. `mcp_get_tool` exposes those
exact annotations to the model, but only direct mode preserves them as host-level
per-tool capabilities. The dispatcher deliberately has no fixed output schema,
because selected upstream output schemas differ.

`mcp_call_tool`, direct proxies, and the gateway all use `forward_tool_call`.
Results retain text, images, structured content, `isError`, and result `_meta`;
unsupported content variants use the existing JSON-text fallback. RMCP request
handles preserve configured timeouts and emit cancellation. Downstream request
cancellation is also forwarded to the upstream request and returns promptly.

The catalogue is a startup snapshot. Dynamic `tools/list_changed` notifications
are not used to mutate the downstream fixed surface; restart the process to
rematerialize an upstream catalogue.

### 7.3 Direct compatibility mode (`"mode": "direct"`)

Each selected upstream tool becomes a `BridgedTool` named `<server>__<tool>`
(sanitized to `[A-Za-z0-9_]`, so `remote-exec` becomes
`remote_exec__exec`). Calls forward by stored raw server/tool name. Direct
descriptors retain the upstream title, description, input/output schemas, icons,
`_meta`, and every supplied annotation field. Missing safety hints are filled with
their MCP defaults so the downstream descriptor is explicit; call results use the
common passthrough path. A
name colliding with an existing downstream tool is skipped with a warning.

This is the default only for standalone explicit `mcpServers` entries. It is the
strongest compatibility mode and the only mode that preserves per-tool host
approval semantics, at the cost of placing every selected transitive definition
in downstream `tools/list` and therefore the ChatGPT connector capability
catalogue.

### 7.4 Gateway compatibility mode (`"mode": "gateway"`)

The whole server becomes one `GatewayTool` named from the sanitized server name,
with input `{ function: <enum>, arguments: object }`. It validates the raw
function name, forwards through the common call path, and writes an auto-generated
skill containing every raw function and full input schema (see §8.3). Its compact
description also contains a bounded function summary.

An 84-tool upstream therefore contributes one tool plus one skill. Gateway mode
is retained unchanged for compatibility, but unlike catalog mode it has no ranked
search or exact metadata lookup and shares the generic-dispatch approval
limitation.

### 7.5 Transports and unsupported capabilities

The upstream client supports the two transports exposed by current Codex:

- **stdio**, inferred from `command`;
- **Streamable HTTP**, inferred from `url`, with `http`, `streamable-http`, and
  `streamable_http` accepted as aliases.

HTTP configuration supports `bearerTokenEnvVar`, static `httpHeaders`,
environment-backed `envHttpHeaders`, `startupTimeoutSec`, and `toolTimeoutSec`.
Environment-backed headers override static headers. Legacy SSE and WebSocket types
are rejected explicitly.

OAuth login/token persistence, `http_headers_helper`, remote Codex execution
environments, MCP resources/templates/prompts, and dynamic capability forwarding
remain outside this tool-transport bridge. Catalog mode exposes upstream
initialization instructions as source metadata but does not inject them into the
downstream server's own initialization instructions.

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

### 8.4 Setup status MCP App

When conversation authorization is enabled, `setup` accepts the historical
`ref` argument plus an optional `connectorVersion` echo. The advertised tool
description embeds the running package version. A freshly refreshed connector
copies that marker into the call; a cached schema from before an upgrade cannot
supply the newer field, so the running server classifies the schema as unknown
and recommends Refresh without rejecting the backward-compatible call.

After recording the authorization grant, `setup` performs one bounded release
check. It invokes `gh api --hostname github.com` with a 2-second timeout and falls
back to an unauthenticated rustls GitHub API request with a 2-second timeout.
Successful results are cached for 5 minutes and failures for 30 seconds. The
structured result includes current/latest versions, source, connector-schema
classification, and the normal next agent step. The compact setup component
renders the version and schema as line-oriented rows but deliberately omits that
model-only continuation text.

The app-only `check_for_updates` MCP tool runs the same bounded release inspection
with a forced cache bypass, then replaces the shared cache entry. This lets the
persistent **Check for updates** action refresh only the Codexify row without
repeating setup authorization. A positively identified newer release adds the
row-local **Upgrade** action, which delegates to the existing verified
`self_update` flow and its restart-safe progress component.

After the component initializes and has rendered setup state, it asynchronously
invokes the app-only `doctor` tool. `doctor` reuses `doctor.rs` against the
already-resolved running `AppConfig`, rather than locating and spawning another
executable, and returns the complete `DoctorReport` as structured content beside
the deterministic human report. Healthy automatic results remain hidden;
warning-only results produce a compact summary, while failures expand structured
checks with pass/warning/failure/skipped color semantics. **Autofix** sends only
warning/failure findings to ChatGPT through `ui/message` and asks the agent to
diagnose, repair, verify, and rerun doctor.

A stale or unknown connector schema adds a row-local **Refresh** action. The
component does not inspect undocumented connector metadata or navigate itself;
instead it sends a constrained `ui/message` prompt telling ChatGPT to derive
`#settings/Plugins/plugin_asdk_app_<slug>` from any visible
`plugin://dev-<slug>@...` mention, otherwise use `#settings/Plugins`, never guess a
slug, and tell the user to scroll below the connector's tool list and click
**Refresh**.

### 8.5 Detached self-update

`self_update.rs` implements the global `self_update` MCP tool. The tool requires
`confirm=true`, verifies that the running process belongs to the standard
`~/.codexify/bin/codexify[.exe]` installation, and acquires the single
`~/.codexify/update/update.lock` before contacting GitHub. It resolves the latest
semantic-versioned release, downloads `checksums.txt` and the exact platform
archive through a bounded rustls client, verifies SHA-256, accepts exactly one
bounded regular executable plus an optional bounded `CHANGELOG.md` from the
archive, writes the executable beside the installed binary, and validates it with
`--help`. The selected changelog sections satisfy `(current, target]` and remain
in component-only result metadata. No service interruption occurs during that
preflight.

Before scheduling the worker, Codexify creates
`~/.codexify/update/status/<update-id>.json` with mode `0600` beneath a private
directory. Same-directory temporary files and atomic replacement preserve a
complete JSON record across process restart. Records contain source/target
versions, an update timestamp, bounded controlled failure information, and one of
`scheduled`, `installing`, `validating`, `restarting`, `succeeded`, `failed`, or
`rolled_back`. Opportunistic oldest-first cleanup retains at most 32 records while
never deleting the active update.

The service supervisor marks only its ordinary server child with
`CODEXIFY_SERVICE_SUPERVISED=1`; model-launched subprocesses have that marker
removed. A supervised update hands the final transaction to an OS-managed job
outside the service's process tree: a transient systemd user unit, a submitted
launchd job, or an on-demand Windows scheduled task. The worker delays 10 seconds
so the MCP result and UI resource can be delivered, advances the durable record at
transaction boundaries, stops the service, retains the old executable as a
rollback copy, atomically installs and probes the replacement, restarts the
service, and removes its task, staging files, and lock. Its terminal state reflects
whether the replacement succeeded, was restored, or left the service unavailable.

`self_update_status` is a project-independent read-only tool whose standard MCP
Apps metadata uses `visibility: ["app"]`; OpenAI compatibility metadata marks it
private and widget-accessible. It accepts only the opaque 96-bit update ID and
returns the durable record plus the version of the process answering the poll.
The `self_update` tool links `ui://codexify/self-update/v1/mcp-app.html`, which
polls immediately, uses one-second normal and two-second reconnect intervals, and
stores its absolute 60-second deadline and terminal state in private widget state.
A timeout is rendered as unverified completion rather than failure and exposes a
manual retry.

Unix foreground processes can schedule the same detached replacement without a
service restart and continue running their already-open executable image; Windows
requires service supervision because the running executable is locked. Update
events share the rotating service log.

---

## 9. Configuration reference

The server selects configuration from `--config`, then `CODEXIFY_CONFIG`, then
`~/.codexify/codexify.config.json`. If no selected file exists, built-in defaults
are used. Relative explicit paths resolve from the startup directory, and the
startup banner prints the exact source. `quickstart` writes the user-level path by
default. All fields are optional, but server startup requires either CLI
`--work-dir` or an absolute config `workDir`; native service launches use the
latter.

```jsonc
{
  "workDir": "/absolute/path/to/project", // required when --work-dir is omitted
  "debug": false,                         // component-only tool timing footers
  "port": 3000,
  "apiKey": "…",                      // or --api-key; bearer token
  "conversationAuthToken": "0123456789abcdef…", // exactly 64 lowercase hex characters
  "multiProject": false,               // or --multi-project; work-dir becomes access root
  "projectCloneDir": ".",             // existing path below access root; or --project-clone-dir
  "allowedHosts": [],                  // DNS-rebinding allowlist; empty = any host
  "openaiTunnel": {
    "tunnelId": "tunnel_0123456789abcdef0123456789abcdef",
    "apiKeyRef": "env:CONTROL_PLANE_API_KEY",
    "clientPath": "…",                // optional; otherwise verified managed install
    "organizationId": "org_…"          // optional
  },
  "tree":   { "defaultDepth": 3, "ignore": ["node_modules", ".git", …] },
  "command":{ "defaultTimeout": 30000, "maxTimeout": 120000 },   // ms
  "exec":   { "mode": "unrestricted",     // or "allowlist"
              "extraAllowedCommands": [], "maxSessions": 8,
              "defaultShell": "…" },
  "ignore": { "useGitignore": true, "useDefaultPatterns": true, "customPatterns": [] },
  "output": { "maxToolOutputTokens": 10000, "maxFileLines": 1000, "maxFileBytes": 131072, "maxEntries": 500, "maxTreeNodes": 1000 },
  "diff": { "maxPatchBytes": 4194304 },
  "toolLogging": { "mode": "off",       // off | requests | responses | all
                   "level": "info",      // trace | debug | info | warn | error
                   "maxRequestBytes": 2048, "maxResponseBytes": 4096,
                   "redactEnv": [] },
  "audit": { "logFile": null, "includeCommandPreview": false,
             "commandPreviewMaxBytes": 512, "redactEnv": [] },
  "artifactIngress": { "enabled": true, "maxFileBytes": 104857600,
                       "requestTimeoutMs": 120000, "idleTimeoutMs": 30000,
                       "maxRedirects": 3, "maxConcurrentDownloads": 2,
                       "allowedHosts": ["*"] },
  "artifactEgress": { "enabled": true, "maxFileBytes": 104857600,
                      "snapshotMaxFileBytes": 104857600,
                      "maxSnapshotBytes": 5368709120,
                      "fallbackToSource": true, "maxReferences": 64,
                      "referenceTtlMs": 300000 },
  "worktrees": { "mode": "auto",        // auto | always | never; or --worktree-mode
                 "root": "…",            // default: $CODEX_HOME/worktrees; or --worktree-root
                 "upstreamRefreshMode": "never",   // never | best-effort
                 "autoCleanupEnabled": true, "keepCount": 15,
                 "allowSetupScript": false },      // opt-in; runs arbitrary setup outside exec policy
  "projectDoc": { "maxBytes": 32768, "fallbackFilenames": [], "rootMarkers": [".git"] },
  "memory": { "enabled": true, "dir": "…", "maxBytes": 16384 },
  "skills": { "enabled": true, "dirs": ["…"], "includePlugins": true },
  "codexMcp": { "enabled": true,        // import MCP servers from Codex's config.toml
                "useCli": true,          // also run `codex mcp list/get --json` for plugin-provided servers
                "cliPath": "…" },        // default: CODEX_CLI_PATH, then `codex` on PATH
  "projectCatalog": {                    // multi-project discovery; independent of codexMcp
    "codexConfig": { "enabled": true, "trustedOnly": true },  // read native Codex [projects] table
    "entries": [ { "path": "…", "name": "…",     // path is absolute or relative to the access root
                   "aliases": ["…"], "description": "…" } ]
  },

  "mcpServers": {
    "local-exec": {
      "command": "D:\\mcphub\\mcp-server-windows-x86_64.exe",
      "args": [], "env": {},
      "type": "stdio",
      "disabled": false,
      "tools": ["exec", "machine_list"],   // optional allow-list of upstream names
      "mode": "gateway"                    // direct | gateway | catalog
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
Config: C:\Users\alice\.codexify\codexify.config.json (user config)
Tools loaded (37): 33 native + 4 upstream-facing MCP tools
Upstream MCP servers:
  idalib      -> catalog (66 private tool(s))
  remote-exec -> catalog (84 private tool(s))
Auth: disabled (no --api-key)
Conversation authorization: enabled (one token check per chat)
Tool payload logging: disabled
Audit log: /private/path/tools.jsonl
Audit command previews: disabled
```

- `Config:` names both the selected file and its source. Explicit relative paths
  resolve from the launch directory; implicit discovery selects the stable
  user-level file.
- The `Upstream MCP servers:` block reports each server's outcome.
- In native tunnel mode, the banner also reports loopback-only exposure, the
  internal-auth boundary, managed runtime version or operator-supplied client,
  and local `/readyz` and `/metrics` URLs. The tunnel ID, runtime key, and
  internal bearer are never printed.
- Multi-project startup also prints `Project access root:`, `Project mode:
  persistent ChatGPT conversation binding`, and the conversation-binding state
  directory; its native count is 34 without conversation authorization and 35
  with it because the two selectors and optional `setup` tool are present.
- Conversation authorization adds one native tool and prints only whether the
  gate is enabled; neither the token nor its derived namespace is printed.
- Top-level `debug` does not alter tracing or payload logging. It adds only the
  tool name, running version, and measured server duration to component-only
  result metadata; widgets may separately measure their own round-trip duration.
- Tool completion diagnostics always include the resolved raw MCP server/tool when
  a direct proxy, gateway dispatcher, or catalog dispatcher selected one.
- `-v` and `-vv` increase Codexify diagnostics without dumping raw tool payloads;
  `RUST_LOG` overrides those defaults. `toolLogging` / `--log-tool-payloads` is the
  separate payload opt-in; the banner prints its mode, severity, and byte limits.
  If the selected payload severity is filtered out, dispatch retains the ordinary
  info-level completion record rather than suppressing all call visibility. When audit
  logging is configured, the banner prints its destination and whether command
  previews are enabled.

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

- Unit tests inside modules plus integration tests under `tests/`
  (`tempfile`-isolated), including the suite ported from the TS Bun project.
- Memory / skills tests pin `memory.dir` / `skills.dirs` to temp dirs so they never
  touch the real home; plugin discovery is suppressed when `skills.dirs` is set.
- `tests/project_selection.rs` covers pre-selection blocking, immutable canonical
  bindings, concurrent session isolation, traversal and symlink escapes,
  project-keyed persistent state, deferred project instructions, and CLI/config
  activation.
- `tests/diff_checkpoints.rs` uses real repositories to cover monorepo scoping,
  byte-for-byte real-index preservation, persistent and transport owners, live ref
  reset, mutation/diff serialization, legacy-ref migration, incremental baselines, unborn repositories,
  malformed Git state, renames, deletions, binaries, relative patches, and patch-budget omission.
- `artifact_ingress` unit tests use scripted response bodies and capability-rooted
  temporary directories to cover provider-host validation, redirects, declared
  and streamed size limits, idle and caller cancellation, exact hashes, partial
  cleanup, symlink escapes, no-overwrite publication, and concurrent writers.
- `artifact_egress` unit tests cover opaque resource generation, exact immutable
  byte snapshots after source replacement, MIME and SHA-256 receipts, traversal,
  symlink escape and oversize rejection, caller cancellation boundaries, cache-byte
  and reference eviction, and reference expiry.
- `bridged_resources` unit tests cover downstream URI rewriting and actual-content
  size enforcement; `bridge.rs` integration fixtures verify resource-link/read
  round-trips in direct, gateway and catalog modes plus expiry and downstream
  cancellation.
- `tests/review_fixes.rs` locks the behavioral-fidelity fixes found by the
  adversarial review of the port. The bridge/gateway/skills code was reviewed the
  same way; the confirmed low-severity findings (name-collision dedup, YAML-safe
  generated frontmatter, non-object `arguments` rejection, version ordering) are
  fixed with regression tests in `bridge.rs` / `skills.rs`.
- `examples/mock_mcp.rs` is a minimal stdio MCP server used to exercise the bridge
  end-to-end.
- `bridge.rs` starts loopback Streamable HTTP MCP servers to verify bearer/custom
  headers, many-tool private catalogues, source discovery, ranked schema-aware
  search, exact metadata retrieval, collision-safe IDs, raw dispatch, filters,
  result passthrough, direct/gateway compatibility, transitive resource egress,
  deadlines, and caller cancellation.

Run: `cargo test`. Build a standalone binary: `cargo build --release`.

---

## 13. Troubleshooting

| Symptom | Cause / fix |
|---------|-------------|
| An upstream is unavailable; banner shows `-> FAILED` | For stdio, verify that `command` is runnable on the machine where Codexify runs. For Streamable HTTP, verify the URL, TLS trust, bearer/header environment variables, and upstream authentication. |
| Banner shows a server you didn't configure (e.g. `idasql -> disabled`) | Codexify loaded a *different* `codexify.config.json` than you edited. Check the `Config:` line and edit that file, set `CODEXIFY_CONFIG`, or pass `--config`. |
| codexify exposes a newer tool set but the client still shows the old one | The client caches the tool manifest — **remove and re-add the connector** so it re-fetches `tools/list`. |
| A direct-mode upstream floods the connector catalogue or some tools disappear | Use `"mode": "catalog"` for ranked progressive disclosure, or keep direct mode and curate raw names with `"tools": [...]`. Gateway mode remains available for compatibility with its generated-skill workflow. |
| Upstream uses `type: "sse"` or `"websocket"` | Current Codex transport parity is stdio plus Streamable HTTP. Point the entry at a Streamable HTTP endpoint and use `url` (or an HTTP type alias). |
| Native tunnel never becomes ready | Check the banner's loopback `/readyz` and `/metrics` URLs and the redacted startup error. Codexify requires runtime readiness plus one successful control-plane poll; the runtime key needs the applicable Tunnels **Read** + **Use** permissions. The runtime-only binary has no `/ui` or `/api/status` surface. |
| Native tunnel key is rejected before startup | `apiKeyRef` must be `env:NAME` or `file:/path`. The referenced value must exist; on Unix, key files must not grant group/other access. |
| `import_host_file` is missing | `artifactIngress.enabled` is false, or the connector cached an older manifest. Enable it and remove/re-add the connector so ChatGPT refreshes `tools/list`. |
| Native file import reports an untrusted URL | The supplied value was not a ChatGPT-native file parameter or its temporary provider URL no longer matches the supported OpenAI file-service boundary. Reattach or regenerate the file instead of passing a URL manually. |
| `export_host_file` is missing | `artifactEgress.enabled` is false, or the connector cached an older manifest. Enable it and remove/re-add the connector so ChatGPT refreshes `tools/list`. |
| An exported-file resource is unknown or unavailable | Native exported-file records do not expire on `referenceTtlMs` and survive Codexify restarts. The resource is unavailable only when its durable record is missing/invalid, or when no retained snapshot exists and the recorded source fallback is disabled, missing, unsafe, or over `maxFileBytes`. Re-export to create a new capability. Bridged upstream resources remain process-local and expire under `referenceTtlMs`/`maxReferences`. |
