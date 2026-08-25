# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.6.0] - 2026-08-25

### Added

- Multi-project `set_project_root` now accepts HTTPS and SSH GitHub repository
  URLs, HTTPS branch URLs (`/tree/<branch>`), and HTTPS pull-request URLs
  (`/pull/<number>`). It reuses an unambiguous local checkout when available,
  otherwise clones into `projectCloneDir` (or `--project-clone-dir`, defaulting to
  the access root). Branch and PR selections fetch the requested ref and use an
  isolated detached worktree when the source checkout is on another commit.

### Changed

- Quickstart no longer offers to enable, generate, or rotate the advanced
  `conversationAuthToken` gate. Conversation authorization remains disabled unless
  configured manually; rerunning quickstart preserves an existing valid token and
  prints its required ChatGPT instruction without surfacing the feature to new
  installs.

### Security

- GitHub project cloning and target fetching are restricted to normalized
  repository, branch, and pull-request URLs on `github.com`; embedded credentials,
  unsupported subpages, and insecure/arbitrary transports are rejected. The clone
  directory is revalidated beneath the access root, concurrent repository
  resolution is serialized, interactive credential prompts are disabled, cloned
  remotes are verified, and destination collisions are refused. Existing source
  checkouts are never switched to satisfy a branch or PR URL, and a session already
  bound to another project is rejected before any clone or fetch side effect.

## [1.5.0] - 2026-08-25

### Added

- Optional per-conversation connector authorization through
  `conversationAuthToken`. Quickstart can generate and persist a high-entropy
  token, protect the config on Unix, and print a one-line instruction for an
  individual chat or ChatGPT Project instructions. The conditional `authenticate`
  tool verifies the token once, then restores the grant from stable ChatGPT
  conversation metadata across connector reconnects and server restarts; generic
  MCP clients use transport-session authorization.

### Fixed

- Codex CLI enrichment now imports plugin-provided Streamable HTTP MCP servers,
  including environment-backed bearer authentication, static and environment
  headers, tool filters, and startup/tool timeouts. Transport-specific fields
  remain fail-closed, and literal bearer tokens are still rejected.
- `import_host_file` now participates in the same fail-closed, serialized review
  checkpoint protocol as every other project-writing tool, so imports cannot race
  review capture or proceed after a checkpoint-capture failure.
- The conversation authorization check no longer holds its in-memory lock while
  probing the durable marker on disk, so a cache miss for one conversation cannot
  serialize every other conversation's authorization behind blocking filesystem
  reads.

### Security

- Conversation authorization blocks every non-authentication tool and withholds
  the project-aware initialization brief until verification. Durable markers store
  only a hashed conversation identity and grant, in a namespace derived from the
  canonical work directory and current token, so token rotation invalidates older
  grants without copying the token into the cache. Audit command previews also
  redact the configured conversation token. The token remains plaintext in
  `codex.config.json` by design and must be kept private and out of version control.
- Generic MCP transport-session project bindings now revalidate the complete
  direct-checkout or managed-worktree relationship on every project tool call.
  A moved, replaced, or internally inconsistent active root cannot escape its
  selected source or recorded managed-worktree boundary.

## [1.4.0] - 2026-08-25

### Fixed

- Audited the ported tool-call handlers against upstream Codex (codex-rs) and
  closed the genuine behavioral divergences:
  - `write_stdin`: a lone `\u0003` (Ctrl-C) now interrupts the exec session
    (SIGINT to the process group on unix, kill on windows) instead of writing
    the raw `0x03` byte into the pipe, matching Codex unified exec.
  - `apply_patch`: an `Add` hunk over an existing path now overwrites it rather
    than being rejected, matching Codex's engine, which does not existence-check
    Adds.
  - `apply_patch`: a patch whose hunks resolve to the same target path twice is
    now rejected, so two operations never silently race on one file.
  - `apply_patch` parser: an `Update` hunk with no leading `@@` context marker is
    now accepted as a single context-less chunk instead of being rejected.
  - `exec_command` / `write_stdin`: `original_token_count` is now always reported
    (not only when truncation occurs), matching upstream; truncation is tracked
    separately for audit.
  - `exec_command`: the initial yield is floored at the 10s default on windows to
    match Codex's first-yield behavior.
  - `view_image`: an optional `detail` hint is now accepted and ignored so a
    Codex client sending it is not schema-rejected.

## [1.3.0] - 2026-08-25

### Added

- Optional Git worktree isolation for concurrent conversations. In multi-project
  mode, `set_project_root` can bind each conversation to a detached managed
  worktree (`worktrees.mode`) created under a configurable worktrees root, so
  simultaneous chats never edit the same checkout. Stale managed worktrees are
  swept on startup when `worktrees.autoCleanupEnabled` is set, bounded by
  `worktrees.keepCount`.
- Optional Codex CLI enrichment for MCP discovery. By default Codexify uses
  `codex mcp list/get --json` when the executable is available, adding MCP
  servers contributed by enabled Codex plugins while retaining direct
  `config.toml` parsing as the standalone fallback. Missing or incompatible CLI
  discovery warns in automatic mode; `--codex-cli` makes it a startup error.
  `codexMcp.useCli` disables enrichment and `codexMcp.cliPath` selects an
  explicit executable.
- A read-only multi-provider project catalogue for multi-project mode. The new
  pre-binding `list_projects` MCP tool discovers trusted paths from native Codex's
  user-level `[projects]` table, merges optional names, aliases, descriptions, and
  explicit entries from `projectCatalog`, filters every candidate through the
  existing canonical access-root boundary, and returns selectors accepted directly
  by `set_project_root`. Native discovery is live, independent from `codexMcp`, and
  requires no `codex` executable.
- `codexify projects list` for local catalogue diagnostics, with query and JSON
  output plus an explicit `--show-skipped` mode. Rejected absolute paths remain
  hidden from MCP output and normal CLI output.
- Interactive `codexify quickstart` onboarding for new installations. The
  wizard selects the project scope, guides tunnel creation and ChatGPT developer
  mode setup with direct links and concrete connector values, validates the
  tunnel ID and hidden runtime key, preserves unrelated JSON configuration, and
  stores the key in a dedicated per-tunnel file outside the project. On Unix, the
  credential directory and file are restricted to the current user. The wizard
  can start the normal supervised server immediately so ChatGPT can scan the live
  tunnel.
- Project-scoped review checkpoints with immutable project-open and incremental
  last-review baselines. `show_changes` reports structured file statistics,
  renames, binaries and a bounded complete patch, and can advance the incremental
  baseline with compare-and-swap semantics.
- A self-contained MCP Apps review resource linked from `show_changes`. Compatible
  ChatGPT developer connectors render file summaries and patches interactively;
  ordinary MCP clients continue to receive the same text and structured result.
- Native ChatGPT file ingress through `import_host_file`. The tool declares the
  OpenAI native-file parameter contract, streams one attachment or generated file
  into a new active-project path, and returns the byte count and SHA-256 receipt.
- `artifactIngress` configuration for enablement, per-file size, whole-request and
  idle timeouts, redirect limits, process-wide concurrent import limits, and a
  configurable `allowedHosts` download allowlist.
- Upstream MCP Streamable HTTP support alongside stdio. Codexify now imports
  compatible `url` entries from Codex configuration and accepts remote entries in
  `codex.config.json`, including bearer-token environment variables, static and
  environment-backed HTTP headers, startup timeouts, per-tool cancellable
  timeouts, the existing tool filters, and gateway mode.

### Fixed

- Resident `exec_command` processes now belong to the stable ChatGPT
  conversation identity instead of the replaceable MCP transport, so a later
  `write_stdin` call in the same chat can resume or poll the process after a
  connector reconnect. Generic MCP clients retain transport-owned process
  cleanup; conversation-owned processes remain bounded by the configured idle
  timeout and are killed when the server stops.
- Managed worktrees now work on Windows: the extended-length `\\?\` prefix that
  `fs::canonicalize` returns is stripped from the worktree root before it is
  handed to `git worktree add`, which otherwise fails to create the worktree's
  leading directories.

### Security

- Review snapshots use a private temporary Git index and an explicit literal
  pathspec for the selected project. They preserve the real index byte-for-byte, never
  include sibling monorepo changes, return project-relative patch paths, and persist
  only hashed conversation identifiers in the dedicated `refs/codexify/review/`
  namespace. Mutating calls and reviews for one conversation/project scope are
  serialized through tool completion.
- Native-file downloads are HTTPS-only and constrained by the configurable
  `artifactIngress.allowedHosts` allowlist, with every redirect hop revalidated
  and ambient proxy credentials disabled. The default `"*"` wildcard accepts any
  public host but always rejects internal and reserved targets — loopback,
  private, link-local, unique-local, carrier-grade-NAT, `localhost`, and the
  cloud metadata address — so an injected or compromised URL cannot reach
  internal services. Hosts named explicitly in the allowlist are trusted as
  given. Signed URLs and file IDs are never returned or logged; RMCP framework
  events are excluded from the tracing layer even when `RUST_LOG` requests them.
- Imported files are written through a capability-confined project directory to a
  private partial and atomically published without overwrite only after size,
  synchronization, and SHA-256 processing complete. Traversal, moved or replaced
  project roots, parent symlink escapes, existing destinations, partial visibility,
  and concurrent replacement races fail closed.
- Per-worktree Codex environment setup scripts are now opt-in through
  `worktrees.allowSetupScript` (default `false`). The script runs an arbitrary
  command outside the `allowedCommands`/exec policy, and both the environment
  file and its script path are selectable through the source repository's local
  Git config, so an untrusted project could otherwise plant a script that runs
  on the next conversation binding. When the flag is off, the environment is
  neither copied into the worktree nor executed.
- Remote bearer tokens and environment-backed headers are resolved only when the
  upstream connection is created and are never included in discovery reports.
  Configured tool-call timeouts use RMCP cancellation rather than abandoning an
  in-flight request. Legacy SSE/WebSocket transports, literal Codex
  `bearer_token` values, mixed stdio/HTTP settings, and ambiguous duplicate
  Authorization configuration are rejected explicitly.
- The upstream Streamable HTTP client's redirect policy is now set to `none` in
  Codexify's own code (`build_upstream_client`) rather than inherited from
  RMCP's default client, so the guarantee that caller-supplied
  `Authorization`/custom headers are never replayed to a redirect target cannot
  silently regress under a dependency bump; a regression test asserts it.

### Changed

- Consolidated on a single `reqwest` 0.13 (the version RMCP's Streamable HTTP
  client transport uses), removing the second `reqwest` 0.12 that was compiled
  in alongside it. The whole process now shares one TLS stack — rustls with the
  ring crypto provider — installed once before any HTTP client is built, so
  there is a single set of trust roots and no aws-lc-rs backend.

## [1.2.0] - 2026-08-24

### Added

- Idle timeout for resident `exec_command` sessions. A background reaper closes a
  session that has neither produced output nor received stdin for
  `exec.idleTimeoutMs` (default 5 minutes; `0` disables it), so a forgotten or
  wedged interactive process no longer lingers holding a process slot.
- Automatic supervision of the native OpenAI Secure MCP Tunnel. The HTTP server
  stays up while the outbound tunnel-client process is restarted on exit, backed
  off linearly, and bounded by a circuit breaker (giving up after five restarts
  within a rolling 60-second window) so a flapping tunnel cannot loop forever.
- Continuous health monitoring of the running tunnel: its loopback readiness and
  control-plane poll metric are probed every 10 seconds, and three consecutive
  failures — distinguishing an unreachable probe from a reachable-but-unhealthy
  tunnel — trigger the same supervised restart path.

### Fixed

- Bound the in-memory output buffer of resident `exec_command` sessions. A shell
  that streamed output faster than it was yielded (a runaway `yes`, a chatty
  build) previously grew an unbounded buffer and could exhaust memory; output is
  now capped in RAM, keeping the head and tail and eliding the middle with a
  byte count, independent of the per-call token truncation.

### Changed

- The release workflow now publishes a `checksums.txt` covering every release
  archive and fails if any published asset is missing a checksum, giving
  downloaders something to verify against.
- The release workflow asserts the pushed tag matches the `Cargo.toml` version
  before building, so a mistagged release can no longer ship a binary whose
  `--version` disagrees with its tag.

## [1.1.0] - 2026-08-24

### Added

- Automatically import local stdio MCP servers from Codex's user-level
  `$CODEX_HOME/config.toml` (or `~/.codex/config.toml`), including launch
  environment, working directory, enablement and tool filters. Explicit
  `codex.config.json` entries overlay imported fields, and discovery can be
  disabled with `codexMcp.enabled`.
- Native OpenAI Secure MCP Tunnel support through the official
  `tunnel-client-runtime`. Codexify can supervise the outbound tunnel directly,
  verify its runtime-only readiness and labeled control-plane polling metric,
  and stop the runtime with the MCP server.
- Verified managed installation of a pinned runtime-only tunnel client, with
  per-platform archive hashes embedded in Codexify, a local integrity manifest,
  private permissions, and compatibility checks. An explicit official client
  binary can be selected instead.
- `openaiTunnel` configuration and matching CLI flags for tunnel ID, runtime
  key reference, client path, and organization ID.
- **Persistent per-conversation project-root selection.** Start with
  `--multi-project` (or `multiProject: true`) to make `--work-dir` an access root.
  ChatGPT conversations bind once with `set_project_root`; the binding is keyed
  from `_meta["openai/session"]`, stored under `~/.codexify/` without persisting
  the raw identifier, and restored across MCP reconnects and server restarts.
  Project tools, command working directories, git operations, `AGENTS.md`, repo
  skills, plans, and notes then use that conversation's independently selected
  root. Clients without ChatGPT conversation metadata retain the transport-session
  fallback. Canonical containment checks reject traversal and symlink escapes,
  and project-aware tools remain unavailable until selection.

### Security

- Native tunnel mode binds the MCP listener to loopback, forces DNS-rebinding
  Host validation to loopback authorities, disables permissive browser CORS,
  and authenticates the local MCP hop with a random per-process bearer token.
- Tunnel runtime keys must be referenced through `env:NAME` or `file:/path`;
  literal keys are rejected, Unix key files must have private permissions, the
  resolved key is exposed only to the tunnel child under a synthetic variable,
  and model-controlled or bridged subprocesses remove the source key variable.
- The tunnel runtime starts with an allowlisted environment rather than
  inheriting ambient tunnel-client configuration, proxy, header, or trust-store
  overrides. HTTP and tunnel shutdown paths are coupled and time-bounded.

## [1.0.1] - 2026-08-24

### Changed

- Release profile uses `lto = "thin"` and default codegen units (was full LTO +
  `codegen-units = 1`), roughly halving release build time; binaries are now
  stripped (`strip = true`).
- CI release workflow builds the `darwin-x64` binary by cross-compiling on the
  Apple-Silicon `macos-14` runner instead of the slow, frequently-queued
  `macos-13` (Intel) pool, and the build job now has a 30-minute timeout so a
  stalled runner fails fast.

## [1.0.0] - 2026-08-19

The first Rust release. The server was rewritten from Bun + TypeScript to Rust
(**tokio + axum + [`rmcp`](https://crates.io/crates/rmcp)**), keeping the tool
schemas, the agent brief, and the on-disk formats compatible with the 0.x line.
The binary is now `codexify`.

### Added

- **MCP aggregator.** Bridge other local MCP servers through an `mcpServers`
  section in `codex.config.json`: Codexify launches each as a stdio child,
  discovers its tools at startup, and re-exposes them as `<server>__<tool>`
  alongside the native tools. Startup banner reports every configured server so a
  bad path or failed handshake is never silent.
- **Gateway mode** (`mode: "gateway"`) collapses a many-tool upstream into a
  single dispatcher tool plus an auto-generated skill, so clients that don't
  surface large tool sets (e.g. ChatGPT) see one tool instead of dozens.
- **`allowedHosts`** config array for DNS-rebinding protection on the `Host`
  header (empty by default, so tunnels keep working).
- **`/health`** endpoint (unauthenticated) reporting the loaded tool count.
- **Claude Code plugin skills.** Skill discovery now also finds skills bundled
  with installed Claude Code plugins under `~/.claude/plugins/cache/...`,
  namespaced `<plugin>:<skill>`; toggle with `skills.includePlugins`.
- `.claude/skills` added to the standalone skill search roots (repo and user).
- Prebuilt release binaries for `windows-x64`, `linux-x64`, `linux-arm64`,
  `darwin-x64` and `darwin-arm64`.

### Changed

- Rewrote the server in Rust; the compiled binary no longer requires a runtime
  and has no AVX2/baseline caveat.
- File-walking tools (`glob`, `grep`, `tree`, `list_directory`) share
  `.gitignore`-accurate matching via the Rust [`ignore`](https://crates.io/crates/ignore)
  crate.

### Notes on the port

Behaviour matches the TypeScript original, with a few unavoidable differences:
`grep` uses the Rust `regex` crate (no lookaround or backreferences);
filename sort uses byte/Unicode ordering rather than `localeCompare`;
`write_file` reports UTF-8 byte counts; `exec_command` uses plain pipes, not a
PTY. See the README's "Notes on the port" for the full list.

[Unreleased]: https://github.com/devnoname120/codexify/compare/v1.6.0...HEAD
[1.6.0]: https://github.com/devnoname120/codexify/compare/v1.5.0...v1.6.0
[1.5.0]: https://github.com/devnoname120/codexify/compare/v1.4.0...v1.5.0
[1.4.0]: https://github.com/devnoname120/codexify/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/devnoname120/codexify/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/devnoname120/codexify/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/devnoname120/codexify/releases/tag/v1.1.0
[1.0.1]: https://github.com/devnoname120/codexify/releases/tag/v1.0.1
[1.0.0]: https://github.com/devnoname120/codexify/releases/tag/v1.0.0
