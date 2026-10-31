# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.3.25] - 2026-10-31

### Fixed

- Wait for ChatGPT's conversation editor to finish rendering before preparing a
  message. Report a missing editor as an availability problem instead of a
  sign-in failure, and avoid reloading a page that is still rendering.
- Recover pairing when an authorized ChatGPT conversation has no workspace
  attached. Provide a one-time continuation to the existing task, complete
  verification, and retain queued user messages until pairing succeeds.
- Preserve a pending pairing handoff when delayed activity arrives from its
  previous conversation owner.

## [2.3.24] - 2026-10-29

### Fixed

- Retain model-submenu ownership when ChatGPT replaces the opening control,
  including Japanese effort-only pickers whose model list opens in a portal.
- Isolate Windows service, update, and diagnostic helpers from the server's
  console and its control events.
- Register Windows services using private UTF-8 script files rather than nested
  encoded launch payloads; migrate recognized older launchers without changing
  their saved configuration or task settings.
- Fix Windows PowerShell update-status replacement by passing a typed .NET null
  backup path. Preserve Unicode in updater scripts, acknowledge worker startup
  immediately, and report stopped update tasks with their Task Scheduler result
  instead of retaining an indefinite scheduled state.
- Keep serving when a Ctrl+C listener cannot be installed, and record Windows
  console-close events, panic locations, and normal runtime exits for diagnosis.

## [2.3.23] - 2026-10-29

### Fixed

- Recover a Windows service whose server process stays alive but stops answering
  local health checks, while allowing slow startup and transient failures.
- Restart unexpectedly terminated Windows supervisors, including zero-code
  exits, and upgrade recognized older task launchers during service
  start, restart, or enable so failure recovery also applies after self-update.

## [2.3.22] - 2026-10-29

### Fixed

- Keep the dedicated browser in Chat mode by opting out of ChatGPT's client-side
  Work rollout, avoiding unsupported Work-mode submissions in projects.

## [2.3.21] - 2026-10-28

### Added

- Configure the offline timeout in Chats & goals with `agentChat.offlineTimeoutMinutes`
  (1–1440 minutes, default 5; restart to apply). Use the same threshold for presence,
  queued delivery, goal activity, offline notifications, and automatic offline wake-ups.

### Fixed

- Stop automatic offline wake-ups from using a shorter timeout than the one
  displayed in the chat interface.

## [2.3.20] - 2026-10-28

### Added

- Show a persistent waiting bar for queued conversations without a goal, with
  the remaining browser cooldown, live queue position, Do now, and Cancel.
  Pending requests can also be cancelled before queue admission, and a stopped
  browser is identified with a Start browser action instead of an invented ETA.

### Fixed

- Renew outdated connector schemas for idle conversations whose previous action
  no longer has a pairing generation, preserving the queued message through
  re-pairing and rejecting stale snapshots.
- Isolate Windows pipe-based command shells and checkpoint helpers from console
  windows and control events. Audit actual shell launches and exits separately
  from Git and filesystem workers to diagnose unexpected service termination.
- Keep a verified subscription plan when returning from billing is deferred by
  the shared conversation-loading cooldown, instead of discarding the result
  and reopening billing on a later retry.
- Allow billing details time to load, retain a verified base-plan fallback while
  resolving the Pro tier, and retry empty or incomplete results rather than
  caching a blank plan indefinitely.

## [2.3.19] - 2026-10-27

### Added

- Recover rejected agent tickets with the read-only, ticketless `ticket_wait`
  tool. Require repeated waits until an exclusive ticket renewal is available,
  then read the latest progress reports before continuing work. Waiting does
  not extend the active ticket's lease, consume messages, or count as work.
- Add thinking time to weekly recap tiles and share cards. Show hours and
  minutes below 100 hours, and whole hours from 100 hours onward.
- Start the browser directly from the stopped-browser banner, using the same
  button styling as Browser settings and removing the redundant instructions.

### Fixed

- Replace ChatGPT conversations that reach the conversation-length limit
  instead of repeatedly submitting to the full conversation. Preserve the
  Codexify task, workspace, queued messages, and goal across the handoff.
- Record the connector schema on each pairing and create a fresh ChatGPT
  conversation before sending, retrying, or renewing with an outdated schema.
  Hold messages until the replacement has verified the current tools.
- Retain the shared browser through re-pairing and its first normal prompt,
  with bounded timeout and cancellation cleanup, avoiding another page load
  and cooldown between initialization and continuing the task.
- Supply a fresh, scoped, single-use agent ticket inline with browser prompts,
  including initialization, retries, and turn renewals. Preserve in-flight
  calls and transport idempotency while admitting the new agent branch.
- Remove read-only-only turn-exit instructions from generated briefs when the
  read-only annotation override is enabled, including live settings changes.
- Require the matching browser companion for the updated handoff and ticket
  protocol so updates cannot retain incompatible automation code.

## [2.3.18] - 2026-10-25

### Fixed

- Expand ChatGPT’s collapsed model panel before selecting a model. Mounted
  options inside inert or hidden panels no longer cause model discovery and
  model switching to time out.
- Require the updated browser companion so an older running companion cannot
  retain the collapsed-picker regression after updating.

## [2.3.17] - 2026-10-25

### Fixed

- Discover models and verify pairing when ChatGPT mounts its model list only
  after expanding the effort picker's submenu, including inline and portaled
  lists. Retain the original model and effort while inspecting capabilities.
- Recognize uniquely owned localized effort controls, including Japanese labels,
  and borrow Medium for pairing without assuming fixed slider positions.
- Report the failed picker stage for discovery and pairing instead of reducing
  every browser conflict to "state changed". Keep raw page and request data out
  of diagnostic responses.
- Acknowledge queued browser actions before waiting for browser ownership or
  account preflight, so slow preparation remains observable and repeat requests
  cannot submit another copy of the message.
- Identify the operation and timeout or connection cause in transport failures,
  and require the updated browser companion compatibility fingerprint.

## [2.3.16] - 2026-10-25

### Fixed

- Handle Stop confirmations that appear during message preparation, including
  while selecting a model or connector. Confirm the dialog, allow up to 20 seconds
  for stopping to finish, then retry only the interrupted pre-Send step without
  reloading the conversation or duplicating the message.
- Advance the browser companion compatibility fingerprint so updates cannot
  reuse a companion that lacks late Stop-confirmation recovery.
- Remove redundant draft-preservation messages from startup banners and the
  composer status while retaining the current startup and refresh progress.

## [2.3.15] - 2026-10-24

### Fixed

- Show the spinning browser-cooldown hourglass in gray in light and dark mode.
- Label the manual override for a queued browser-cooldown wait as "Do now",
  while retaining "Retry" for failed actions.
- Update the saved model and effort shown in the conversation picker after a
  confirmed fallback, including effort-only downgrades, while preserving newer
  manual selections and leaving unsent attempts unchanged.
- Keep fallback usage attribution tied to the confirmed model and effort, and
  preserve the original send time when duplicate events or snapshots arrive.

## [2.3.14] - 2026-10-24

### Fixed

- Keep connector refresh exclusive from preparation through completion and
  restoration of the original page. Subscription checks, model discovery,
  navigation, and message submission wait instead of interrupting the refresh.
- Report the shared browser as busy while refresh completion is being observed,
  and retain bounded step deadlines, cancellation cleanup, and a two-minute
  overall refresh deadline.
- Advance the browser companion compatibility fingerprint so post-update
  maintenance cannot reuse an older companion with the interrupted-refresh bug.

## [2.3.13] - 2026-10-24

### Added

- Show live progress text, progress bars, elapsed time, and the latest update
  while cloning, copying, creating a worktree, or preparing a new chat.
- Hold the send button to choose Steer or Queue for one message without changing
  the saved default; support touch, mouse, and keyboard interaction.
- Require an explanation and unblock steps for blocked goals, or a work summary
  and suggested next steps for completed goals. Display both as persistent,
  expanded inline reports in conversation history.

### Changed

- Goal, chat, and progress-journal descriptions and replies explicitly require
  continued work or repeated waits rather than ending the assistant turn.
  Questions direct the agent to continue useful work or await the needed answer.
- Advance the goal-tool schema marker so cached older definitions can be detected.

### Fixed

- Probe repository URLs without a `.git` suffix, including non-GitHub hosts, and
  retry with `.git` appended when the initial probe fails.
- Show whether an offline chat is queued to wake, sending to ChatGPT, or waiting
  for the agent's first tool call after a confirmed send.
- Wake a blocked, offline goal for new messages submitted after it became blocked,
  without requiring manual steering or resuming the goal. Keep messages queued
  before the blocker private, including across retries and restarts.
- Keep browser-wait countdowns readable on mobile and reduce the gap above Retry
  and Dismiss controls.
- Preserve goal reports that precede the first ordinary transcript message.

## [2.3.12] - 2026-10-23

### Added

- Create and select a new project folder directly under the configured projects
  root from the New chat dialog.

### Fixed

- Accept ordinary folders and Git repositories without commits in the New chat
  picker, using the selected folder directly when a worktree cannot be created.
- Confirm already-open ChatGPT Stop dialogs before editing the composer, then
  allow the full 20-second settling window before retry or reload recovery.
- Show a continuously rotating green hourglass for browser-cooldown waits and
  display the conversation's current position in the shared browser queue.
- Group linked worktrees under their base project in the chat sidebar while
  keeping each chat's actual workspace intact.
- Retain per-chat model and effort selections and reuse a global model catalog
  across upgrades. Refresh it at startup and daily, preserving the last good
  catalog when discovery fails.
- Buffer pasted-image uploads to avoid Safari upload failures and center the
  attachment removal icons.
- Exclude waiting and completion operations from progress-journal checkpoint
  counts and reminders while preserving normal agent activity tracking.

## [2.3.11] - 2026-10-22

### Fixed

- Keep browser actions queued until the shared conversation-loading cooldown ends,
  without treating the wait as a failure, consuming a retry, or adding another
  backoff. Actual failed requests retain their configured retry schedule.
- Show one neutral browser-wait countdown and preserve queued waits across
  restarts, including manual Retry, cancellation, and pause controls.
- Center the attachment preview's close icon inside its circular button.

## [2.3.10] - 2026-10-22

### Added

- Configure browser retry schedules, jitter, approval-resume waits, and shared
  conversation-loading cooldowns in Settings → Chats & goals → Automatic retries.
  Existing timings remain the defaults; timing changes apply after a restart.

### Changed

- Limit shared ChatGPT conversation loads and reloads to one per three minutes
  normally, or one per five minutes while conversation access is failing.
- Manual Retry bypasses local browser cooldowns for that attempt while retaining
  account verification, browser ownership, and protection against duplicate sends.
- Daily turns, Time in motion, and Daily tool calls show seven days of activity
  when Today is selected; summary totals and other analytics still cover Today.

### Fixed

- Recognize current ChatGPT conversation-load errors, including disabled retry
  controls, without incorrectly asking the user to sign in again.
- Handle Stop confirmation dialogs inside ChatGPT iframes and allow the full
  settling period after confirming before attempting another Stop or reload.
- Retry confirmed-unsent rate-limit and shared cooldown failures on the same
  jittered schedule as conversation-access errors, respecting pause and global
  retry settings.
- Keep Retry and Continue beside their error banner during pending, paused, and
  exhausted retries instead of inserting them into the goal bar.
- Update shared cooldown countdowns every second. Hide the shorter cooldown when
  the automatic retry is later, and omit expired-cooldown notices.

## [2.3.9] - 2026-10-22

### Fixed

- Reload an already-open ChatGPT sign-in failure before retrying a paired
  conversation, then verify the signed-in account again before sending.

## [2.3.8] - 2026-10-22

### Fixed

- Keep the ChatGPT browser on the submitted conversation for at least five seconds
  after Send, including cancellation and failure cleanup, so the next action
  cannot interrupt its pending network request.

## [2.3.7] - 2026-10-22

### Changed

- Newly unlocked achievement notifications stay visible twice as long (7.2 seconds).

### Fixed

- Give each ChatGPT Stop click up to 20 seconds to finish before retrying or
  reloading the conversation, while continuing immediately once it stops.

## [2.3.6] - 2026-10-21

### Fixed

- Confirmed-unsent ChatGPT editor insertion failures use the same jittered
  3, 5, 10, 20, 30, and 60-minute retry schedule as conversation-load failures.
  Existing or edited drafts still require manual attention.

## [2.3.5] - 2026-10-21

### Added

- Create a fresh worktree from a selected registered worktree’s current commit,
  including pasted worktree paths outside the project access root.
- Persistent Pause retries and Dismiss controls for failed actions, plus an
  immediate global Automatic retries setting under Chats & goals.

### Fixed

- Resolve file-materialization approvals in the latest ChatGPT turn before
  continuing a conversation, including prompts that appear during preparation.
  Persist the wait for resumed agent activity across restarts without sending
  another continuation or marking unsent messages delivered.
- Fresh agent activity clears a failed goal-only continuation and its pending
  retries, including exhausted retries and a retry queued before the agent resumed.
- Switching conversations uses cached models without probing the browser.
  Model lists survive compatible local updates for the same ChatGPT account;
  sending still verifies the requested model on the target page.
- Wait three times longer for the configured @ connector to appear before
  clearing and retrying the mention search. If it stays unavailable, send the
  message without the mention instead of failing or reloading the conversation.

## [2.3.3] - 2026-10-20

### Fixed

- Reuse healthy ChatGPT conversation pages for browser sends and retries, and
  reuse prepared new-chat pages. Failed conversation pages still reload once
  for recovery. Maintenance avoids duplicate restoration loads and reads an
  available account-menu plan before visiting billing.
- Handoff checks leave the current conversation loaded and bound necessary
  visits to other pending handoffs. Required conversation loads share a
  three-second spacing; HTTP 429 responses apply a shared Retry-After cooldown.

## [2.3.2] - 2026-10-20

### Added

- Optional `instanceName` installation display titles for MCP clients. Unset,
  null, and blank names preserve the existing appearance; connector names and
  the programmatic server identity remain unchanged.

### Changed

- Updates show progress outside settings, through installation, restart, browser
  startup and connector refresh. Startup keeps drafts editable and waits before
  sending messages that need unavailable services. Connector refresh explains
  when a restart is required and offers the restart action directly.

- File materialization in ChatGPT is disabled by default and has a named switch
  under Tools & files → File exports. Opting in enables native file exports and
  full attachment resources; inline attachment text and image previews remain
  available while it is off. Explicit saved preferences are preserved.
- ChatGPT navigation and message-submission phases allow 30 seconds before
  timing out, preserving the safeguards against replaying an uncertain send.
- Authentication and conversation-access failures retry after 3, 5, 10, 20, 30,
  and 60 minutes, then stop. Retry deadlines include up to 10% additional jitter
  to spread requests across chats and remain stable through restarts.
- Shared statistics label active time as Thinking hours with an `h` suffix.
  Model names include Pro inline or show thinking levels in parentheses.

### Fixed

- Workspace-picker setup and its app-only project actions remain available with
  embedded widgets enabled and agent chat disabled, while preserving setup
  authorization and keeping agent-chat tools disabled.
- Return and Cmd+Return/Ctrl+Return send answers immediately while the agent is
  waiting for input, including when the default message mode is Queue. Shortcut
  hints and the send-button tooltip match the available behavior.
- Error-banner Retry and Continue controls follow the server's retry permission
  during automatic backoff. Countdown text only advertises an available action
  and distinguishes retrying an unsent message from continuing a goal.
- Long active-time labels stay centered on one line inside the model-mix chart.
- Connector refresh can open settings from a failed conversation page without
  waiting for a missing message composer or incorrectly requiring sign-in.

- Auto-approval handles the current conversation's connector permission cards,
  including file materialization, by selecting conversation scope. It verifies
  the account and connector, supports localized menus, and resumes the existing
  turn without sending another prompt or consuming queued messages.
- Empty audit Log File settings use the per-project default destination and show
  that path in settings. The destination is optional when audit logging is off.
- Generated ChatGPT instructions use naturally wrapping paragraphs instead of
  hard line breaks, while preserving user-message formatting and paragraph breaks.
- Recognize ChatGPT's conversation-load failure and report "ChatGPT conversation
  temporarily not accessible" instead of requiring authentication. Detection uses
  the page structure across UI languages, while other conversations remain usable.
- Updates use a temporary browser-maintenance stop instead of recording a
  permanent manual stop, so automatic browser recovery resumes after updating.
  Explicit Stop browser preferences and the signed-in profile are preserved.
  The browser status describes a paused auto-start setting without attributing
  it to a user action.

## [2.3.0] - 2026-10-19

### Added

- Cached image thumbnails for chat attachment cards. Opening or downloading an
  image still uses the original; unchanged previews reuse browser-cached bytes.
- Estimated agent-hours and the most-used model on shared statistics cards,
  including PNG, animated SVG and weekly recap exports.
- `codexify update` uses the verified updater from the terminal, restarting a
  background service only when it is running.
- Brief synthesized celebration sounds, with an independent Sounds option in
  Extras and playback unlocked by interaction with the page.

### Changed

- Add a one-day analytics range, including shared statistics cards, while
  keeping seven days as the default.
- Show the sidebar waiting icon while a completed request waits for the next
  message, without treating that message as an answer to an unfinished request.
- Hide generated filenames on pasted image attachment cards and previews.
  Files chosen through the file picker keep their original name visible.
- Conversation unread markers sync across browsers through a persisted owner
  read position, including migration of existing browser-local read history.
- Unread dots track incoming messages only, so outgoing messages and delivery
  or read receipt changes do not make a conversation appear unread.
- Conversation goal bars appear immediately from sidebar state, then reconcile
  with the live automation response without shifting the initial chat layout.
- Browser start, stop, sign-in and model refresh controls live in global
  settings under Browser and work without selecting a conversation.
- Analytics automatically fetch fresh totals, streaks and records whenever
  opened, while keeping the last figures visible during loading.
- Sidebar chat and goal icons keep spinning in red during automatic recovery;
  terminal errors keep the static attention icon.
- Celebrations use five larger staggered fireworks, glowing sparks and rising
  trails, while remaining click-through and lasting less than four seconds.

### Fixed

- Download attachments directly, avoiding popup download failures in
  Linux WebKit and keeping the current chat open.

## [2.2.1] - 2026-10-18

### Fixed

- Scope managed-worktree cleanup to source projects under the server's current
  access root. Servers with different access roots can share a worktree directory
  without removing each other's clean worktrees. Missing or unresolved source
  paths are preserved.

## [2.2.0] - 2026-10-18

### Added

- Automatically recover an installed browser companion when it closes, stops
  responding or retires a frozen browser session. Recovery uses a browser-only
  responsiveness probe and preserves the private profile and pairings.
- Start browser and Stop browser controls in conversation settings. An explicit
  stop persists until Start is pressed, including across server restarts. Stop
  can cancel startup; concurrent callers share the same lifecycle lock.
- Expand achievements from 8 to 32 across completed goals, lifetime turns, tool
  calls, daily streaks, active goal hours and turns in one conversation. New
  long-term tiers reach 1,000 goals, 100,000 turns, 10 million tools and a
  year-long streak. Existing unlocks remain earned; newly added historical
  milestones unlock quietly without a backlog of celebrations.
- PWA installation beside Statistics, with native prompts where supported and
  iPhone installation instructions. Installed apps hide the install control.
- Automatic connector tool refresh after schema edits, debounced for 10 seconds.
  Annotation edits refresh live; structural edits refresh after their restart.
- Independent request-blocking switch, disabled by default, with five ChatGPT
  telemetry, history and sidebar patterns ready to enable.
- Standalone Statistics beside the global settings cog, with daily streaks,
  personal records, achievement badges and shareable calendar-week recaps.
  Celebrations are brief and do not intercept clicks. Persistent receipts prevent
  repeated record animations after reloads or goal pause/resume. Extras settings
  independently disable celebrations and recaps while preserving all statistics.
- Turn renewal can be enabled independently for each model and reasoning effort.
  Disabled and unlisted selections are left alone, including after a model
  fallback. Model switches apply immediately and cancel pending renewals.

### Changed

- Strip Markdown from generated and custom chat titles while preserving their
  readable text. Sidebar labels, conversation headers and rename fields use the
  same plain title; message formatting remains intact.
- Compact mobile settings keep the same readable control text size during
  editing, fix date-range text clipping, and respect iPhone screen safe areas.
- Rename the chat sidebar to Codexify and always use workspace selection.
- Enable agent chat, browser automation, auto approval, debug and audit previews
  by default. Use 8 filesystem workers/downloads, Info/all tool logs, queue
  delivery, a 25-call progress interval and best-effort upstream refresh.
  Destructive and open-world annotation overrides default to false. Explicit
  saved settings remain in effect.

### Fixed

- Recognize localized ChatGPT controls, model efforts, plans and errors through
  verified UI identities and the bundled locale catalog. Greek and Italian
  pairing, submission, model fallback and Stop confirmation have regression
  coverage; ambiguous controls are rejected before sending.
- Handle ChatGPT's optional Stop-response confirmation without depending on its
  displayed language, while keeping cancellation and stop deadlines bounded.
- Browser action deadlines renew only when a new submission stage makes
  progress. Repeated events cannot keep a stalled action alive indefinitely.
- Resume cancelled chats and goals without resending messages already accepted
  by ChatGPT. Goal failures use the same retry backoff as ordinary messages.
- Require the configured connector name before creating chats or starting goals,
  and offer setup instead of guessing a connector.
- Use interactive macOS service scheduling to prevent background I/O throttling
  from stalling child processes.

## [2.1.0] - 2026-10-18

### Added

- Always-on local Analytics with stacked model/effort charts, date ranges,
  lifetime totals, conversation leaderboards, completed goals, streaks and
  milestones. Collection starts with new activity, uses separate best-effort
  storage and never blocks chat automation.
- Colorful Analytics share cards with static PNG previews, copy and download,
  plus animated SVG exports with counting totals, activity bars, flames and
  sparks. Cards respect reduced-motion preferences, contain aggregate totals
  and `codexify.dev` branding, and are generated locally without public hosting.
- Private, persistent message queues with edit, delete and steer actions.
  Configure steering or queuing as the default; Cmd+Enter on macOS or Ctrl+Enter
  on Windows/Linux uses the other mode for that message. Only one queued message
  is delivered per completed request, and unfinished goals keep queued tasks
  private while automatic continuations run.
- Separate `chat_await` and `turn_completed` tools: requested user answers go
  straight to the agent, while completed requests can consume the next queued
  task. Pending input requests survive ordinary tool activity.
- A per-chat `rollout.md` progress journal, enabled by default, for chronological
  progress, current work and next steps. The configurable reminder defaults to
  every 20 work tool calls; bounded recent-report reads and dated rotation keep
  recovery context manageable while preserving older reports.
- Optional turn renewal, independent of the progress journal and disabled by
  default. Model/effort deadlines ask the agent to write a final-answer handoff,
  then continue automatically even without a goal. GPT-5.6 Sol Pro defaults to
  115 minutes; other selections default to 55 minutes.
- Passive model cutoff estimates from confirmed sends to the last native tool
  call, excluding completed work, input waits, renewal handoffs and implausible
  samples. Collection failures remain silent and cannot interrupt chat work.
- Minimal Home Screen web app support with a manifest, icons, standalone display
  and private authenticated launch. The app remains online-only.

### Changed

- Mobile layouts use full-width chat lists and conversations with a back button,
  compact model/effort selectors and an aligned sidebar footer. Composer outlines
  are rounded, and the mobile viewport avoids input-focus zoom.
- Agent chat responses omit unnecessary message metadata while preserving
  message timestamps and delivery tracking internally.

### Fixed

- Active goals are rediscovered at service startup and every 30 seconds, even
  without queued messages or a pending handoff. Restoring a goal no longer
  requires opening its conversation; paused and blocked goals keep their state.
- Dismissing an error no longer stops the objective or disables its retries.
  Native agent activity reconciles delivery and renewal state without replaying
  messages whose submission is uncertain.
- Automation state loading tolerates recoverable state inconsistencies without
  preventing the conversation list from loading.
- Replies to explicit input requests bypass the default queue mode without
  consuming queued tasks. Completed work advances the queue, while a request
  for user input keeps it on hold.
- Queued attachment previews stay compact and unclipped, and recent chats no
  longer show a duplicate divider below pinned conversations.

## [2.0.0] - 2026-10-17

### Added

- Agent-chat browser automation is available as an opt-in offline fallback for
  standalone chats and autonomous goals, with one shared ChatGPT tab, verified
  conversation pairing, a global action queue, and goal controls through
  `goal_read` and `goal_finish`.
- A separately packaged browser companion with bundled private Chrome, Patchright,
  an isolated profile, and `codexify browser install|start|stop|status|login|reveal`
  lifecycle commands. Browser installation remains opt-in with
  `CODEXIFY_INSTALL_BROWSER=1`.
- Local chat pinning and archives, transcript search, project and recent views,
  date grouping, persistent chat names, and inline renaming. Pinning and archiving
  affect only Codexify's interface.
- Persistent chat attachments with preview, download, and agent attachment tools.
- Redesigned global and conversation settings, settings search, validated
  automatic saving, a raw JSON editor, and actual default values that remain
  unsaved until explicitly changed.
- Browser request blocking with configurable regular expressions, plus independent
  experimental overrides for read-only, destructive, idempotent, and open-world
  tool annotations.
- Conversation workspace controls that preserve chat identity and keep project
  memory separate when changing the workspace.

### Changed

- Unavailable model and effort selections automatically fall back to the next
  verified available option for chats and goal continuations, starting with
  GPT-6 Astra Pro, GPT-5.6 Sol Pro, GPT-6 Sol Extra High, and GPT-6 Sol High.
  Saved preferences remain intact and are retried on later turns.
- Automatic pairing uses Medium effort, records created conversation URLs before
  verification completes, accepts valid late pairing, and restores the requested
  conversation settings afterward.
- Browser queue ownership ends after verified submission instead of waiting for
  the generated response. Operations use bounded deadlines and cancellation
  cleanup; connector refreshes queue behind active work and reuse the existing tab.
- Connector tools refresh automatically after updates. Subscription labels use
  billing information with an account-menu fallback and remain hidden when no
  reliable plan label is available.
- Goal completion appears at its transcript position with a compact expandable
  summary. Completed-goal icons share consistent colors and reflect offline
  presence; conversation headers, controls, and sidebar footers are aligned.

### Fixed

- Message submission now waits for the real enabled Send control, excludes voice
  controls, handles repeated DOM markers for one logical turn, and clears stale
  drafts before browser actions.
- Native message delivery and read receipts reconcile without waiting for browser
  work. Agent acknowledgments can resolve uncertain submissions without sending
  the same message again.
- Verified pairings survive browser failure cleanup. Created chat URLs remain
  available for retrying failed pairing instead of reverting to a project URL.
- Failed chat actions can be cancelled as well as retried. Goal updates no longer
  insert internal control messages into the visible conversation.
- Archived-chat navigation preserves the selected conversation when it has been
  restored to the inbox. Pinned chats have a persistent filled pin and a visual
  separator from unpinned chats.
- Unresponsive upstream services and blocking filesystem work are isolated so
  they cannot stall unrelated tool requests.

### Security

- Browser archives are SHA-256-verified and installed privately with the required
  license notices. Account, conversation, model, effort, connector, and pairing
  identity are checked before submission; ambiguous Send, UI drift, approvals,
  account changes, and interference fail closed without automatic replay.
- Pairing references, ChatGPT URLs, prompts, goals, browser output, and account
  fingerprints are redacted from payload logging and omitted from audit values.

## [1.7.0] - 2026-10-07

### Added

- Setup-card continuation prompts now transfer one complete Codexify task to a
  replacement ChatGPT conversation through a one-time `continue_task` token.
  Workspace and Git state, agent-chat transcript/cursor, saved plan and memory,
  diff checkpoints, resident command sessions, and the standalone-chat entry stay
  attached to the same task instead of being copied. The old conversation becomes
  read-only after the claim. Tokens are random, digest-only on disk, single-use,
  restart-safe, and safe to retry after a lost successful response; claims fail
  while the old owner has a model call in flight.
- The standalone owner-chat page can copy an individual message's original
  Markdown or the complete clean conversation transcript. Whole-chat copy
  includes unloaded history and omits internal markers, timestamps, receipts,
  and tool-call counters. Clipboard controls remain absent from the embedded
  ChatGPT widget.

### Changed

- The setup widget's **Refresh** dialog links to connector settings without a
  text-fragment suffix. The connector-specific destination and manual refresh
  instructions are unchanged.
- Connector schema markers now include `+continuation-v1`. Current setup,
  setup-chat, and Markdown-chat resources are versioned v8, v6, and v3;
  previous resource URLs remain readable. `resumePath` remains available only as
  a workspace-only compatibility path.

## [1.6.6] - 2026-09-27

### Changed

- `chat_read` and `chat_await` now advertise `readOnlyHint: false` because they
  acknowledge user messages and advance the conversation's read cursor. Their
  execution behavior and other annotations are unchanged. The experimental
  forced-read-only annotation override still takes precedence when enabled.

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

[Unreleased]: https://github.com/devnoname120/codexify/compare/v2.3.24...HEAD
[2.3.24]: https://github.com/devnoname120/codexify/compare/v2.3.23...v2.3.24
[2.3.23]: https://github.com/devnoname120/codexify/compare/v2.3.22...v2.3.23
[2.3.22]: https://github.com/devnoname120/codexify/compare/v2.3.21...v2.3.22
[2.3.14]: https://github.com/devnoname120/codexify/compare/v2.3.13...v2.3.14
[2.3.13]: https://github.com/devnoname120/codexify/compare/v2.3.12...v2.3.13
[2.3.12]: https://github.com/devnoname120/codexify/compare/v2.3.11...v2.3.12
[2.3.11]: https://github.com/devnoname120/codexify/compare/v2.3.10...v2.3.11
[2.3.10]: https://github.com/devnoname120/codexify/compare/v2.3.9...v2.3.10
[2.3.9]: https://github.com/devnoname120/codexify/compare/v2.3.8...v2.3.9
[2.3.8]: https://github.com/devnoname120/codexify/compare/v2.3.7...v2.3.8
[2.3.7]: https://github.com/devnoname120/codexify/compare/v2.3.6...v2.3.7
[2.3.6]: https://github.com/devnoname120/codexify/compare/v2.3.5...v2.3.6
[2.3.5]: https://github.com/devnoname120/codexify/compare/v2.3.3...v2.3.5
[2.3.3]: https://github.com/devnoname120/codexify/compare/v2.3.2...v2.3.3
[2.3.2]: https://github.com/devnoname120/codexify/compare/v2.3.0...v2.3.2
[2.3.0]: https://github.com/devnoname120/codexify/compare/v2.2.1...v2.3.0
[2.2.1]: https://github.com/devnoname120/codexify/compare/v2.2.0...v2.2.1
[2.2.0]: https://github.com/devnoname120/codexify/compare/v2.1.0...v2.2.0
[2.1.0]: https://github.com/devnoname120/codexify/compare/v2.0.0...v2.1.0
[2.0.0]: https://github.com/devnoname120/codexify/compare/v1.7.0...v2.0.0
[1.7.0]: https://github.com/devnoname120/codexify/compare/v1.6.6...v1.7.0
[1.6.6]: https://github.com/devnoname120/codexify/compare/v1.6.5...v1.6.6
[1.6.5]: https://github.com/devnoname120/codexify/compare/v1.6.4...v1.6.5
[1.6.4]: https://github.com/devnoname120/codexify/compare/v1.6.3...v1.6.4
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
