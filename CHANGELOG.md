# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- The setup MCP App now uses compact status rows, keeps explicit **Check for
  updates** and **Doctor** actions available, runs structured doctor diagnostics
  asynchronously, surfaces warnings/failures with **Autofix**, and delegates stale
  connector refresh instructions to a ChatGPT follow-up message. Manual release
  checks bypass the latest-version cache, while the obsolete
  `chatgptConnectorSettingsUrl` setting and connector-ID metadata probing were
  removed.

## [1.2.1] - 2026-09-01

### Changed

- GitHub Actions now warms target-specific release dependency caches on `main`,
  restores them for tagged builds, runs release validation in parallel with the
  platform build matrix, and avoids retaining release-tag caches that cannot
  benefit subsequent releases.

### Fixed

- The landing page now automatically selects the Windows install command on
  Windows browsers while preserving the macOS/Linux default for those platforms
  and unknown clients; the operating-system tabs remain manually switchable.

## [1.2.0] - 2026-08-31

### Added

- `codexify doctor` for side-effect-free local diagnostics with deterministic
  human and JSON reports and failure-aware exit status. It validates effective
  configuration, Codex-aligned Git and ripgrep availability, GitHub CLI (`gh`),
  the configured exec shell, Codex CLI enrichment, enabled stdio MCP commands,
  latest-release freshness, self-update locks, native-service state and loopback
  health, and OpenAI tunnel credentials/runtime integrity without starting MCP
  children or downloading the managed tunnel runtime.
- The per-conversation `setup` tool now returns a cached `gh`-first latest-release
  check and connector-schema version comparison in its original response. Its MCP
  App shows Update and Doctor buttons, warns when ChatGPT should refresh its cached
  tools, and opens a connector-settings link when a connector ID or configured
  `chatgptConnectorSettingsUrl` is available. The doctor action is app-only.
- Top-level `debug` configuration now adds bounded component-only tool execution
  timings. The setup, diff, and updater widgets render server timings, and
  widget-originated calls also report their observed round-trip duration.
- `self_update` now attaches an MCP App that renders every checksum-bound
  changelog section in the upgrade interval and monitors the detached update
  across service restart. A private atomic record under
  `~/.codexify/update/status/` distinguishes scheduled, installation, validation,
  restart, success, failure, and rollback states. The component polls through a
  dedicated tool advertised with app-only visibility, requires the target process
  version before declaring supervised success, and treats its 60-second timeout as
  unverified completion rather than failure.
- Release archives now include `CHANGELOG.md`, and the detached worker waits 10
  seconds before service interruption so ChatGPT can receive and initialize the
  updater resource.
- Native exported-file capabilities now survive MCP reconnects and Codexify
  restarts. Eligible files are retained as immutable per-user disk snapshots with
  a configurable global least-recently-used budget, while snapshots that are too
  large or have been evicted can safely resolve the latest file at their recorded
  project-relative source path.

### Changed

- Command execution now uses `exec_command`/`write_stdin` exclusively. The
  redundant `run_command` tool and top-level `allowedCommands` setting were
  removed; `exec.mode` now defaults to `"unrestricted"` and
  `exec.extraAllowedCommands` defaults to an empty list. Opt-in allowlist mode
  remains available through `exec.extraAllowedCommands`.
- `view_image` now follows Codex's `high`/`original` detail contract and image
  preparation limits, with `high` as the default and original-resolution detail
  using Codex's larger image budget.
- `clock_sleep` now uses integer millisecond durations and ends early when the
  active MCP request is cancelled, mirroring Codex's interruptible-sleep behavior
  within Codexify's existing five-minute tunnel-safe cap.
- Scheduled self-updates now explicitly direct users to refresh the ChatGPT
  connector after Codexify restarts so the updated tool schema is loaded.

### Fixed

- macOS self-update now waits for launchd teardown, retries bounded
  `EALREADY` lifecycle transitions, recovers when a bootout or bootstrap finishes
  between commands, and verifies server plus native-tunnel readiness before
  declaring the restarted update successful.
- The Codexify landing page keeps its GitHub button visible on narrow mobile
  layouts.

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

[Unreleased]: https://github.com/devnoname120/codexify/compare/v1.2.0...HEAD
[1.2.0]: https://github.com/devnoname120/codexify/releases/tag/v1.2.0
[1.1.0]: https://github.com/devnoname120/codexify/releases/tag/v1.1.0
[1.0.0]: https://github.com/devnoname120/codexify/releases/tag/v1.0.0
