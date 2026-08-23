# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[1.0.1]: https://github.com/devnoname120/codexify/releases/tag/v1.0.1
[1.0.0]: https://github.com/devnoname120/codexify/releases/tag/v1.0.0
