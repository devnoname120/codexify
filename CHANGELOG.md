# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.1.0] - 2026-08-30

### Added

- A GitHub Pages landing page at `codexify.dev` with installation commands,
  feature and architecture documentation, and automatic light/dark theming based
  on the browser's preferred color scheme.

### Changed

- The macOS/Linux and Windows installers now explicitly tell users to restart
  their terminal after installation and then run `codexify quickstart` to finish
  connector setup.

### Fixed

- Payload logging now checks tracing filters using event metadata, so a filtered
  payload level reliably falls back to the ordinary tool-completion event.

## [1.0.0] - 2026-08-29

### Added

- Bridged MCP `resource_link` results now work end-to-end through Codexify in direct, gateway, and catalog modes. Upstream resource URIs are replaced with short-lived opaque capabilities; downstream `resources/read` is proxied to the originating MCP with cancellation, timeout, size, TTL, and reference bounds, and returned content URIs are rewritten before reaching the connector.

- OpenAI Codex plugin skills now participate in the normal Codexify skill catalogue using Codex-compatible plugin activation, active-version selection, manifest-declared skill roots and namespaces, Agent Plugin direct-child discovery, legacy recursive and migrated-command discovery, and `[[skills.config]]` enablement rules. Claude Code plugin-skill discovery remains supported under the same `skills.includePlugins` switch.

- MCP `self_update` tool for verified in-place updates of standard installations. It downloads and validates the latest release before scheduling an OS-managed detached worker that stops the background service, atomically swaps the executable with rollback protection, and restarts the service. Progress is written to the rotating service log.

- Checksummed Linux, macOS, and Windows installation scripts that download the
  latest GitHub release, replace the executable under `~/.codexify/bin`, and add
  that directory to the user's shell or Windows `PATH`. The macOS installer also
  removes the executable's quarantine attribute. Installers register and start
  the native per-user background service unless `CODEXIFY_SKIP_SERVICE=1` is set.
- Install-time migration of legacy `~/.codex-free` state into `~/.codexify`.
  The old `codex.config.json` is rebased onto Codexify defaults so only values
  that differed from Codex Free defaults are carried forward, `review` settings
  become `diff`, and state-path references are rewritten. Existing Codexify
  config values win conflicts, conflicting legacy state files are retained, and
  pre-rename conversation, authorization, review-checkpoint, and worktree state
  remains addressable across the renamed hash and metadata namespaces.
- Native user-service management through systemd on Linux, launchd on macOS, and
  Task Scheduler on Windows. `codexify service install|enable|disable|remove` owns
  lifecycle state, while `codexify service logs [-f]` reads bounded rotating logs.
  The service supervisor waits for first-run configuration, restarts failed server
  processes with bounded backoff, and launches them with an absolute config path.
- Top-level `workDir` configuration for unattended launches. Quickstart stores the
  canonical absolute project path and restarts an installed service automatically.
- Multi-project `set_project_root` now accepts HTTPS GitHub commit URLs
  (`/commit/<sha>`). Full 40-character commit IDs are fetched and selected exactly,
  using a detached clone or managed worktree without moving an existing source
  checkout.
- Multi-project `set_project_root` now accepts provider-agnostic HTTPS and SSH Git
  repository URLs ending in `.git`, including GitLab and SCP-style SSH clone URLs.
  Conventional service remotes such as `git@host:group/repo.git` are matched to
  their HTTPS equivalents, while arbitrary SSH users remain distinct. Unsafe local,
  insecure, and credential-bearing HTTPS transports remain rejected.
- `output.maxToolOutputTokens`, defaulting to 10,000 approximate tokens, as a
  connector-wide ceiling for textual model-visible tool results.
- Configurable all-tool payload tracing through `toolLogging` and
  `--log-tool-payloads[=<MODE>]`. Native, direct MCP, gateway MCP, and catalog MCP
  calls now emit paired start/completion events with monotonic call IDs, selectable
  severity, resolved raw upstream server/tool names, mandatory secret and checksum
  redaction, MCP image content-block and resource-capability elision, and
  independently work-bounded UTF-8 request/response previews. Audit JSONL records
  use the same resolved identity fields.

### Changed

- **Breaking connector-schema rename:** the Codexify diff-display tool is now
  `show_diff` instead of `show_changes`, and its incremental baseline value is
  `last_diff` instead of `last_review`. Refresh the ChatGPT connector after
  upgrading so the new tool schema is registered. The public configuration block
  is now `diff`; the former `review` key remains accepted as a compatibility alias.
  New persistent checkpoints live under `refs/codexify/diff/` with a
  `last-diff` ref, while existing `refs/codexify/review/.../last-review` state is
  copied lazily and retained for rollback compatibility. The MCP App now emits a
  diff-named resource URI and result-metadata key, while historical review-named
  cards and widget state remain readable.

- `exec_command` and `write_stdin` now clamp caller-requested output budgets to
  server policy. `grep` caps match count, context and individual long lines while
  keeping the actual match visible, and `run_command` returns bounded partial
  output on timeout.
- The `show_changes` MCP App now renders GitHub-style wrapped diffs with old/new
  line-number gutters, full-width addition and deletion colors, blue hunk headers,
  bundled syntax highlighting, bounded intraline highlighting, and compact
  binary-change summaries. Redundant review/checkpoint chrome and per-line `+`/`-`
  markers were removed, the app no longer requests an additional host border, and
  only the review panel is opaque so its surrounding iframe canvas can composite
  transparently with the host conversation.
  Its current resource URI is versioned at `v3`; the v2 and unversioned URIs remain
  readable.

### Security

- Tool `content` and `structuredContent` are finalized through a common output
  policy before entering model context. One-shot command stdout and stderr are
  drained through bounded head/tail buffers, while component-only `_meta` remains
  outside the model-visible limit.

[1.1.0]: https://github.com/devnoname120/codexify/releases/tag/v1.1.0
[1.0.0]: https://github.com/devnoname120/codexify/releases/tag/v1.0.0
