# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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

### Fixed

- Resident `exec_command` processes now belong to the stable ChatGPT
  conversation identity instead of the replaceable MCP transport, so a later
  `write_stdin` call in the same chat can resume or poll the process after a
  connector reconnect. Generic MCP clients retain transport-owned process
  cleanup; conversation-owned processes remain bounded by the configured idle
  timeout and are killed when the server stops.

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

[Unreleased]: https://github.com/devnoname120/codexify/compare/v1.2.0...HEAD
[1.2.0]: https://github.com/devnoname120/codexify/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/devnoname120/codexify/releases/tag/v1.1.0
[1.0.1]: https://github.com/devnoname120/codexify/releases/tag/v1.0.1
[1.0.0]: https://github.com/devnoname120/codexify/releases/tag/v1.0.0
