# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.6.5] - 2026-09-26

### Changed

- `chat_await` now defaults to 55 seconds instead of 115 seconds. Its existing
  `agentChat.maxWaitMs` setting remains configurable from 1000 to 300000 ms,
  including waits before workspace selection. Bundled configuration files and
  documentation use the new default; existing explicit overrides are preserved.

## [1.6.4] - 2026-09-26

### Fixed

- Standalone owner-chat access tokens persist across service restarts in the
  existing private `owner-chat.json` file, so saved chat URLs keep working with
  a fixed listener port. Concurrent first starts share one token, changing the
  listener port preserves it, and invalid or unsafe credential files are rejected
  rather than silently replaced. Token rotation is now an explicit action.

## [1.6.3] - 2026-09-25

### Changed

- Temporarily publish releases without waiting for Apple notarization while the
  team's enrollment rejection (status 7000) is unresolved. The notarization steps
  remain commented in CI for restoration; Developer ID signature verification,
  archive checksums, and build/test checks remain required.

### Fixed

- Duplicate-agent warnings show their timestamp at the bottom right inside the
  yellow banner, using the same local-time formatting as normal chat bubbles in
  both embedded and standalone agent chat. Missing historical timestamps remain
  omitted, and date labels update at local midnight.
- Duplicate-agent warnings explain the competing-agent problem and the request
  for the duplicate to stop so the other agent can continue without interference.

## [1.6.2] - 2026-09-25

### Added

- Agent chat shows a filled red speech bubble and composer cue while awaiting a
  reply. An unanswered timeout preserves the cue for 20 seconds without flicker
  across chat-only calls; replies and non-chat agent tool calls restore
  activity-based presence.

### Fixed

- Setup and live status checks use the complete configuration-aware connector
  schema marker, eliminating false refresh warnings when agent tickets or
  multi-project selection is enabled. Genuine connector and conversation schema
  mismatches remain distinct.
- The setup card recognizes versioned Markdown-chat markers alongside ticket and
  workspace suffixes when explaining schema changes.

## [1.6.1] - 2026-09-24

### Changed

- Agent chat changes to away after three minutes and offline after five minutes.
  The same five-minute interval applies to offline notifications and agent-ticket
  recovery. An expired chain accepts a missing or stale string ticket while an
  in-flight call retains its exclusive reservation.

### Fixed

- Ticket rejections add a yellow warning between chat messages on every rejected
  call, including in chats that previously showed a warning. Existing warnings
  also render outside agent message bubbles.
- Audit JSONL records ticket reservation decisions and successor handoffs,
  including rejections before tool dispatch, without storing ticket values.

## [1.6.0] - 2026-09-23

### Added

- `experimental.agentTickets` (disabled by default) prevents stale agent branches
  from dispatching tools through a short single-use ticket chain. Widget-only
  helpers remain exempt; stable conversation tickets survive reconnects and
  restarts. A server-written chat warning identifies a blocked possible duplicate;
  cancellation observed at the response handoff check preserves the old ticket.
  The first call after ten minutes offline can reclaim the chain, but never steals
  an in-flight reservation. Enabling it requires serial tool calls and a connector
  schema refresh.
- The `experimental` section groups ticketing, `forceReadOnlyToolAnnotations`, and
  automatic Claude skill/plugin discovery (`claudeSkills`), all disabled by default.
  The former top-level read-only override remains a compatibility fallback; an
  explicit nested value takes precedence.
- `agentChat.port` pins the standalone owner-chat listener to localhost port
  `3120` by default; set it to another port or to `null` for an OS-assigned port.
- Setup supports GitHub URL entry, explicit project switching, and listing/reusing
  existing project worktrees with paths, names, and recorded last-use dates.
  Workspace-change notices require the agent to reload its project brief.
- Per-tunnel schema reload records cover anonymous discovery and survive
  restarts. The refresh action opens a dimmed instructions popover with a
  connector-settings deep link and the existing text fragment.

### Changed

- Workspace-aware chat widgets use a new schema. After upgrading, refresh the
  connector tools and reload older chat cards before sending messages or downloading files.
- `chat_await` defaults to a 115-second wait rather than 270 seconds.
- The agent brief is normally loaded once per conversation/workspace and reloaded
  after relevant instruction changes or loss of its context, not for every task.

### Fixed

- Skill discovery honors `policy.allow_implicit_invocation` from `agents/openai.yaml`.
  Explicit-only skills stay available through `skills_list` and `skills_read` without
  entering the automatic brief; full trigger descriptions are preserved.
- Opt-in Claude plugin discovery uses registered installation paths and applicable
  project scopes instead of offering stale cached plugins or guessing their version.
- MCP catalogue schemas avoid host-incompatible regular expressions while keeping
  argument validation in the tool handlers.
- Switching workspaces reconnects the existing chat panel to the selected
  transcript without mixing history, receipts, or late responses. Unconfirmed
  sends retain their original destination rather than moving to another project.
- The chat composer accounts for its borders when sizing short drafts, avoiding
  clipped text and an unnecessary scrollbar while preserving scrolling for long drafts.
- The standalone owner-chat access key remains in the URL fragment across page
  load, conversation selection, browser navigation, and reload.
- Chat tool-call markers remain above user messages sent after those calls;
  per-message counters preserve the chronology across retries and reloads.
- Greetings leave the project picker open instead of selecting scratch to unlock
  Markdown chat; `chat_await` can wait for the user's initial workspace choice.
- The standalone owner-chat sidebar reads the API's camelCase activity fields,
  uses smaller presence icons, and stays aligned with the selected chat header.
- `service install` writes the systemd unit's `WorkingDirectory=` value without
  quotes, so `systemd` accepts the unit and `service enable` can start it.

## [1.5.3] - 2026-09-16

### Added

- `openaiTunnels` accepts 1–8 tunnel ID/API-key-reference pairs so one Codexify
  service can connect to several ChatGPT accounts. Each tunnel client is
  supervised independently; a failing account does not take healthy tunnels
  offline. Existing single `openaiTunnel` configurations remain valid.

### Fixed

- The standalone owner chat now puts the selected conversation in the URL so
  reload restores it and browser Back/Forward navigates between chats. The
  private access token remains in session storage after the initial link loads.

## [1.5.2] - 2026-09-16

### Added

- `codexify chat` opens a private, localhost-only owner view of persisted agent
  chats outside ChatGPT. A conversation list sorts by the latest chat entry and
  shows the same online/away/offline activity states, per-browser unread dots,
  and locally derived titles. The selected chat reuses the embedded widget's
  history, composer, receipts, Markdown, downloads, and live tool counters; both
  views use the same server-side channel.

### Security

- The owner view uses a separate loopback listener and an ephemeral access
  token; it is not routed through the MCP tunnel. This does not isolate chats
  from connector users with unrestricted tool execution under the same OS
  account.

## [1.5.1] - 2026-09-16

### Added

- When `agentChat.notifications` is configured, the server attempts one offline
  notification after a conversation has gone 10 minutes without an agent tool
  call, matching the chat indicator's offline threshold. The claim is persisted
  before provider delivery to prevent duplicate alerts; the next agent call
  re-arms it. The chat card does not need to be open, and a failed delivery is
  not retried automatically.

## [1.5.0] - 2026-09-16

### Added

- Agent chat shows a live total of model-visible Codexify tool calls in its
  header and a compact interval count between agent messages. Counts persist
  across reloads and server restarts; app-only widget polling is excluded.
- User and agent chat bubbles show small local times at the bottom right, with
  user times before the ticks and `YYYY-MM-DD` prefixes for earlier local days.
  Timestamps persist across reloads and retries; midnight changes update locally
  without another server response. Undated historical user text is not backfilled.
- Markdown chat can notify multiple services through the local Apprise library,
  using `agentChat.notifications.urls`, an optional Python interpreter path,
  and a bounded timeout. All services, including ntfy, use this single backend;
  the former native ntfy implementation and `markdownChat.ntfy` block are removed.
  CI checks the real ntfy/Pushover/webhook adapters and the
  subprocess path on Linux, macOS, and Windows without contacting real recipients.
- Opt-in `agentChat` communication gives each conversation its own `CHAT.md`
  outside the repository by default. `chat_read`, `chat_write`, and `chat_await`
  support complete unread Markdown, persistent cursors, native directory watches
  with polling fallback, a configurable 270-second default wait, and optional
  Apprise notifications with service credentials stored directly in local configuration.
- Every advertised tool gains an optional `new_chat_message_from_user` output
  when Markdown chat is enabled. User messages bypass ordinary output truncation,
  remain pending until a chat tool acknowledges them, and do not enter payload
  logs. The brief directs communication and waiting through the chat tools.
- `setup` opens one persistent conversation-specific chat panel with an
  arrow/Return send action and Shift+Return newlines; chat tools create no cards.
  App-only tools append user messages and read history without starting a ChatGPT turn.
  One grey tick means saved, two grey ticks mean returned in an agent-facing
  response, and two blue ticks mean acknowledged through a chat tool.
  A conversation-scoped indicator shows online below four minutes since the last
  agent tool invocation, last seen until ten minutes, and offline thereafter.
  Widget interactions do not count as agent activity. Cards preserve drafts,
  offer earlier history, and retry sends without
  duplicating messages. Inactive cards stop polling.

### Changed

- Renamed the public configuration block from `markdownChat` to `agentChat`.
  Versioned config migration rewrites the old key instead of retaining a runtime
  alias, including conversion of the former native ntfy shape.
- Configuration files now carry `schemaVersion`. On successful startup,
  unversioned historical configs are validated, backed up byte-for-byte beside
  the original, and atomically rewritten to the current schema. The migration
  covers the v1.0 `review` name and artifact-cache field, removed command-policy
  fields, and post-v1.3 chat configuration shapes; newer schemas are rejected.

### Fixed

- Legacy migration preserves `workDir`, accepts an explicit `--work-dir` for old
  command-line-only roots, and validates every candidate with the current server
  startup loader before replacing configs or removing legacy state. Regression
  tests exercise the generated file on Linux, macOS, and Windows.
- Chat history has more reading space: up to 480 px on desktop and 420 px on
  narrow screens, with the composer outside the scrolling message area.
- Markdown chat renders tables, nested formatting, reference links, and balanced
  URLs with an embedded Markdown parser. Exported and project-relative file links
  use a private workspace-scoped resolver and host-mediated downloads, with an
  explicit fallback when the host does not support widget downloads.
- Release installers now validate an existing selected config with the same
  startup rules as the server before installing the background service. Invalid
  configs, including migrated configs missing `workDir`, now defer service setup
  instead of installing a service that immediately exits.
- Release staging now retries draft-release readback to tolerate GitHub's brief
  post-creation consistency delay without requiring a failed-job rerun.
- Setup status detects Markdown-chat schema toggles without a package-version
  change, preserves each conversation's baseline, and distinguishes an observed
  connector reload from an older conversation. The widget explains the toggle
  and directs the user to refresh or start a new conversation as appropriate.
- `grep` accepts an explicit file as well as a directory, including the current
  conversation's read-only Markdown chat history.

## [1.4.0] - 2026-09-10

### Added

- `codexify service status [--json]` reports the native background service's
  installation, running/enabled state, definition path, and platform details
  without starting or changing it. Distinct exit codes identify running, stopped,
  absent, and query-failure states; no valid server configuration is required.
- `codexify config` can print the selected JSON document, resolve its path, get,
  set, or unset escaped dotted settings, and edit a validated staging copy via
  `VISUAL`, `EDITOR`, `nano`/`vi`, or Notepad. Mutations use atomic replacement,
  preserve unrelated fields and existing permissions, and refuse symlink targets.
- `codexify service start`, `stop`, and `restart` control the running service
  without changing whether it starts at login. Existing `enable` and `disable`
  retain their enablement semantics on systemd, launchd, and Task Scheduler.
- macOS release binaries now use the stable Developer ID identity `dev.codexify`
  with hardened runtime and timestamping. Releases are staged as drafts and
  published only after both architecture-specific Apple notarizations are
  independently accepted and reverified.

### Changed

- Human-facing Doctor, project-catalogue, and service-log output now use adaptive
  terminal colors while preserving plain redirected output and machine-readable
  JSON. `service logs` promotes the tool name to the start of tool events, removes
  legacy stored ANSI sequences, and pretty-prints request and response JSON.
- Quickstart now asks for single- or multi-project mode before the directory,
  defaults new setups to multi-project mode, uses mode-specific path wording,
  prompts for tunnel credentials directly, and suggests `Codexify` for both the
  tunnel and connector. Its interactive output and command errors use adaptive
  terminal colors. Existing config values are preserved rather than renamed.
- When no service is installed, quickstart now offers background-service
  installation first and keeps the foreground-server path as an explicit
  fallback.
- The macOS/Linux and Windows installers now defer service installation when the
  selected config file does not exist. Quickstart creates the config and installs
  the service afterward. Their final restart-and-quickstart instructions are
  separated by a blank line and highlighted in green, with bold ANSI emphasis on
  capable POSIX terminals.
- Apple notarization submission is asynchronous: tag workflows exit after
  uploading signed drafts, a bounded Cloudflare relay wakes a short macOS
  finalizer, and no runner waits for Apple's processing. Installers and self-update
  remain restricted to the latest published stable release and reject unpublished
  metadata explicitly.

### Fixed

- Windows service tasks now launch through a hidden PowerShell host, and the
  supervised server child uses `CREATE_NO_WINDOW`, preventing empty console
  windows while preserving the interactive-logon tunnel and network context.

- Set diff code text to 12 px on desktop and 10 px on mobile while
  retaining larger file labels and controls. Indented identifiers now wrap within
  the available width instead of leaving a whitespace-only first visual line;
  source whitespace and syntax/intraline highlighting remain intact. The diff
  resource advances to v5, with v4 and v3 URLs still readable.

## [1.3.0] - 2026-09-07

### Added

- Copyable continuation prompts when a conversation uses an older connector
  schema, with a **Prepare handoff** action to save task context. New chats use
  `set_project_root.resumePath` to reuse the exact saved worktree, direct checkout,
  or persistent scratch workspace without allocating another checkout. Resumption
  preserves uncommitted files, the index, and workspace memory; invalid paths fail
  without falling back. Clipboard denial supports manual copying.
- Version-only connector reload tracking across conversations and server restarts,
  scoped by endpoint and identified caller. The setup widget distinguishes a
  current schema, a connector requiring Refresh, and an older conversation that
  needs a new chat after the connector was refreshed.
- A worktree checkbox below **Chat without a project**, using the configured
  default. Explicit `set_project_root.createWorktree` choices override that
  default without changing the saved configuration.

### Changed

- Setup version rows appear above workspace selection and recheck live status on
  activation and every 30 seconds while visible. Old results cannot restore
  obsolete Upgrade or Refresh actions; unknown schema state stays hidden.
- Diff code text is now 13 px on desktop and mobile, with larger file labels and
  controls. Previous setup and diff resource URLs remain readable.

## [1.2.4] - 2026-09-02

### Changed

- The setup widget's stale-schema **Refresh** action now opens the relative
  `#settings/Plugins/plugin_asdk_app_<slug>:~:text=Information-,Refresh,-Connected`
  hash directly through ChatGPT's `window.openai.openExternal`, with
  `ui/open-link` as fallback. It no longer reconstructs the current ChatGPT URL
  from `document.referrer`.

## [1.2.3] - 2026-09-02

### Added

- Top-level `uiWidgets` configuration now controls Codexify's built-in MCP App
  widgets. It defaults to `true`; setting it to `false` removes widget template
  metadata, the MCP Apps extension, built-in UI resources, and component-only
  diff/updater/debug payloads while keeping the underlying tools and non-widget
  resource/file metadata available.

### Changed

- The setup widget's stale-schema **Refresh** action now derives the connector
  slug directly from ChatGPT's same-origin `asdk_app_<slug>.web-sandbox`
  ancestor and opens the matching plugin settings through the host link API,
  with the generic Plugins page as fallback. Refresh no longer sends an agent
  follow-up prompt.

## [1.2.2] - 2026-09-01

### Added

- Multi-project `setup` now renders a searchable project chooser with
  **Chat without a project** fixed above the results. The setup app calls the
  existing `list_projects` and `set_project_root` tools, discards stale debounced
  searches, and replaces the chooser with the selected direct path, managed
  worktree plus source checkout, or private scratch path.
- `set_project_root` now accepts `withoutProject: true`. ChatGPT conversations
  receive a durable private scratch workspace outside the configured access root;
  generic MCP transports receive an ephemeral scratch directory removed on
  disconnect. Project-scoped filesystem, command, memory, skill, and instruction
  tools use that scratch root without exposing the projects directory.

### Changed

- The setup MCP App now uses compact status rows, keeps explicit **Check for
  updates** and **Doctor** actions available, runs structured doctor diagnostics
  asynchronously, surfaces warnings/failures with **Autofix**, and delegates stale
  connector refresh instructions to a ChatGPT follow-up message. Manual release
  checks bypass the latest-version cache, while the obsolete
  `chatgptConnectorSettingsUrl` setting and connector-ID metadata probing were
  removed.

### Fixed

- MCP 2026-07-28 cacheable list and resource-read responses now include the
  required private, immediately-stale hints. This restores ChatGPT widget
  ingestion and connector refresh while preserving legacy wire shapes and
  capability-bounded cache lifetimes for bridged resources.

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

[Unreleased]: https://github.com/devnoname120/codexify/compare/v1.6.3...HEAD
[1.6.3]: https://github.com/devnoname120/codexify/compare/v1.6.2...v1.6.3
[1.6.2]: https://github.com/devnoname120/codexify/compare/v1.6.1...v1.6.2
[1.6.1]: https://github.com/devnoname120/codexify/compare/v1.6.0...v1.6.1
[1.6.0]: https://github.com/devnoname120/codexify/compare/v1.5.3...v1.6.0
[1.5.3]: https://github.com/devnoname120/codexify/compare/v1.5.2...v1.5.3
[1.5.2]: https://github.com/devnoname120/codexify/compare/v1.5.1...v1.5.2
[1.5.1]: https://github.com/devnoname120/codexify/compare/v1.5.0...v1.5.1
[1.5.0]: https://github.com/devnoname120/codexify/compare/v1.4.0...v1.5.0
[1.4.0]: https://github.com/devnoname120/codexify/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/devnoname120/codexify/compare/v1.2.4...v1.3.0
[1.2.4]: https://github.com/devnoname120/codexify/compare/v1.2.3...v1.2.4
[1.2.3]: https://github.com/devnoname120/codexify/compare/v1.2.2...v1.2.3
[1.2.2]: https://github.com/devnoname120/codexify/compare/v1.2.1...v1.2.2
[1.2.1]: https://github.com/devnoname120/codexify/compare/v1.2.0...v1.2.1
[1.2.0]: https://github.com/devnoname120/codexify/releases/tag/v1.2.0
[1.1.0]: https://github.com/devnoname120/codexify/releases/tag/v1.1.0
[1.0.0]: https://github.com/devnoname120/codexify/releases/tag/v1.0.0
