# [Codexify](https://codexify.dev/)

Landing page: **https://codexify.dev/**

---------------

**Turn ChatGPT into a Codex-like coding environment — without using your Codex/Work quota.**

Codexify gives a regular ChatGPT conversation the tools to work on your projects:
file editing, terminal commands, Git, interactive diffs, and reusable skills.
Describe a task, let ChatGPT inspect the code, make changes and run tests, then
review the result — all from the same conversation.

**ChatGPT does the reasoning; your machine runs the tools.** Codexify connects
them through [Model Context Protocol (MCP)](https://modelcontextprotocol.io/),
rather than running a Codex agent or starting a ChatGPT Work task. The normal
workflow uses your ChatGPT allowance, leaving your Codex/Work quota untouched.

> ChatGPT's [own usage limits](https://help.openai.com/en/articles/20001354-gpt-56-in-chatgpt)
> still apply. If you explicitly invoke Codex, Work, or another metered service
> through connected tools, that service's usage counts normally.

[Get started](#get-started) · [Documentation](#documentation) · [Website](https://codexify.dev) · [Downloads](https://github.com/devnoname120/codexify/releases/latest)

## ChatGPT plan comparison

Codexify runs inside an ordinary ChatGPT **Chat**, so the relevant limits are the Chat allowances rather than the Work/Codex quota. Codexify doesn’t use any Codex/Work quota, it only uses Chat quota which is independent:

<div align="center">

| Plan | &ensp;5.6&nbsp;Luna&ensp; | &ensp;5.6&nbsp;Sol&ensp; | &ensp;5.6&nbsp;Sol&nbsp;Pro&ensp; | &ensp;6&nbsp;Astra&nbsp;Pro&ensp; | &ensp;Context&nbsp;window&ensp; |
| :--- | :---: | :---: | :---: | :---: | :---: |
| **Free** | ∞ | — | — | — | Varies |
| **Go**<br><sub>$8/month&emsp;</sub> | ∞ | — | — | — | 256K |
| **Plus**<br><sub>$20/month&emsp;</sub> | ∞ | ∞<br><sub>High</sub> | — | — | 256K |
| **Pro 5x**<br><sub>$100/month&emsp;</sub> | ∞ | ∞<br><sub>Extra High</sub> | **50/week**<sup>1</sup> | **50/week**<sup>1</sup> | 400K |
| **Pro 20x**<br><sub>$200/month&emsp;</sub> | ∞ | ∞<br><sub>Extra High</sub> | **1,190/week**<br><sub>170/day max</sub> | **200/week**<br><sub>&nbsp;</sub> | 400K |

</div>

<sup>1</sup> One shared 50-message weekly allowance across 5.6 Sol Pro and 6 Astra Pro.

Note: the context window is the total shared reasoning window, not the amount reserved exclusively for user input: system instructions, tools, memory, internal reasoning, and the
response also consume it.

See OpenAI's [Plan tiers details](https://chatgpt.com/pricing/#:~:text=Compare%20features%20across%20plans) for more information.

## A Codex-like workflow in ChatGPT

- **Explore, edit, and test.** Search a codebase, apply patches, run your project's
  tests, and inspect the results.
- **Review the work.** See file changes in an interactive diff card, and exchange
  attachments and generated files with ChatGPT.
- **Keep projects organized.** Connect one repository or choose a project for each
  conversation. Optional Git worktrees keep concurrent edits in separate checkouts.
- **Bring your conventions.** Load `AGENTS.md` and skills, and save task notes and
  plans for later conversations.
- **Connect more tools.** Make local or remote MCP servers available through the
  same connector, including servers configured in Codex.

## Get started

Codexify runs on **macOS, Linux, and Windows**. You need a ChatGPT account or
workspace with [custom MCP apps enabled](https://developers.openai.com/api/docs/guides/developer-mode)
and access to [OpenAI Secure MCP Tunnels](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)
in an OpenAI Platform organization. ChatGPT access and tunnel permissions are
separate; a workspace administrator may need to grant them.
One Codexify service can connect to several ChatGPT accounts by configuring
separate tunnel ID/key-reference pairs in `openaiTunnels`; all accounts share
the same local tools and project access. See [native tunnel setup](docs/REFERENCE.md#manual-native-openai-tunnel-setup).

### 1. Install

**macOS / Linux**

```sh
curl -q -fsSL https://codexify.dev/install.sh | sh
```

**Windows PowerShell**

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://codexify.dev/install.ps1 | iex"
```

The installer verifies the downloaded binary's checksum and adds it to your PATH.
When a config already exists it also refreshes the per-user background service;
on a first install, quickstart creates the config and installs the service. No
Rust toolchain is needed. Open a new terminal after installation so the
`codexify` command is available.

macOS release binaries use the stable Developer ID identity `dev.codexify` and
remain signature-verified. Notarization is temporarily disabled while Apple
resolves the team's enrollment rejection (status 7000); current macOS releases
are signed but not notarized. The first signed update from an older ad-hoc build
may require one final macOS privacy reapproval; later signed updates retain the
same identity.

Prefer a manual install? Get a [prebuilt binary](https://github.com/devnoname120/codexify/releases/latest)
or read the [installation guide](https://github.com/devnoname120/codexify/wiki/Installation).
The installer source is available for [macOS/Linux](https://github.com/devnoname120/codexify/blob/main/install.sh) and [Windows](https://github.com/devnoname120/codexify/blob/main/install.ps1).

### 2. Connect to ChatGPT

```sh
codexify quickstart
```

The wizard defaults to multi-project mode, asks for the matching projects root or
single project directory, then guides you through creating a tunnel, its runtime
API key, and the `Codexify` connector in ChatGPT. It prints the links and exact
settings to use, saves your configuration, and starts the connection.

The ChatGPT connector name is persisted as `instanceName` and advertised as the
MCP server's human-readable title while the programmatic implementation name stays
`codexify`. Give separate installations distinct names such as `Workstation` and
`HomeServer` so clients can distinguish the machines without creating different
Codexify binaries. You can change it later with
`codexify config set instanceName Workstation`.

Keep Codexify running while you use it; the background service handles that for a
standard installation.

This setup uses an outbound OpenAI tunnel: you do not need a public server URL or
an inbound port. For other deployments, see [connection options](docs/REFERENCE.md#connecting-to-chatgpt).
The fully populated [configuration example](codexify.config.example.json) shows
every supported JSON field with non-secret placeholders.

Configuration schema changes are migrated automatically on the next successful
startup. Codexify validates the migrated document before replacing the config,
keeps a byte-for-byte `*.before-schema-vN.bak` copy beside it, preserves file
permissions, and refuses configs written for a newer schema version.

### 3. Start a task

Start a regular **Chat** conversation in ChatGPT, not a Work or Codex task, and
select the connector you created. If you enabled multiple projects, give ChatGPT
the project path or choose one when asked. Start with a read-only request:

```text
Call get_agent_brief and follow the project's instructions.
Explain how this project is organized. Do not change any files.
```

Then ask for a concrete change, for example:

> Fix the failing test, run it again, and show me the diff. Do not commit yet.

A greeting leaves the setup project picker open; scratch is selected only by an
explicit choice. Paste a GitHub repository/PR/branch/commit URL, search local
projects, or use **Worktrees** to reuse an existing checkout. After selection,
**Switch to another project** reopens the picker without deleting prior files.
The agent receives the new path and must reload the project instructions. See the [workspace guide](https://github.com/devnoname120/codexify/wiki/Multi-Project-Mode)
for project selection and worktree options.

## Markdown chat (optional)

Steer an active conversation by appending to a local Markdown file instead of
sending another ChatGPT message. Enable the feature in your Codexify config:

```json
{
  "agentChat": {
    "enabled": true,
    "port": 3120,
    "maxWaitMs": 55000,
    "automation": {
      "enabled": false
    }
  }
}
```

`agentChat.port` defaults the separate standalone chat listener to localhost
port `3120`. Set it to `null` to use an OS-assigned port instead. It must differ
from the main top-level `port` when agent chat is enabled.

`agentChat.maxWaitMs` controls the server-side `chat_await` and `turn_completed` timeout in milliseconds.
It defaults to `55000` (55 seconds) and accepts values from `1000` to `300000`.
Existing explicit values remain in effect; omit the setting to use the default.

Every chat keeps a private `rollout.md` progress journal beside `CHAT.md`, enabled
by default. It is a chronological activity timeline of completed work, current
work and next steps, and can reference supporting files.
Agents read recent progress with `rollout_read` when a turn starts and append
status, progress and next steps using `rollout_update`. Tool results request a
checkpoint after **20 agent work calls** by default. Change
`agentChat.rolloutReminderInterval` in **Settings → Chats & goals → Progress
report interval**; it applies immediately. UI polling and idle chat waits do not
count. Recent recovery reads include up to five complete reports, capped at
64 KiB. At 256 KiB or 50 reports, the active file rotates into a dated archive
inside `rollouts/`; older history is preserved and recent reads span rotations.

**Turn renewal** is independent of the progress journal and is **off by default**.
Enable it in **Settings → Chats & goals → Turn renewal**. Its per-model/effort
limits remain visible and editable while disabled: GPT-5.6 Sol Pro defaults to
115 minutes; GPT-6 Astra Pro and every other selection default to 55 minutes.
Unlisted model/effort selections use 55 minutes. Limits are measured from the
last confirmed message sent to ChatGPT by Codexify, using the actual selected
model (including fallback), and apply without a service restart.

At the next tool response after the limit, the agent is asked to stop work and
write a comprehensive handoff in its **normal final ChatGPT answer**. This is an
explicit exception to the Markdown chat wait loop; the report is not sent with
`chat_write` and does not complete or block a goal. Codexify waits for the new
report to finish, then sends `Continue.` even without a goal. Queued messages
stay private during the handoff and are not included in that continuation.
Uncertain user-message sends are not replayed automatically. A stalled goal
continuation can retry after three minutes only after fresh completed-report
checks; native activity acknowledges delivery and prevents replay. User stop/cancellation and
disabling turn renewal cancel its pending continuation. Saved pairing, timer
and handoff state survive service restarts.

**Analytics**, planned for 2.1, is the last tab in global settings. Collection is
on by default (`agentChat.analyticsEnabled`) and starts with new activity; nothing
is backfilled. Choose seven days (default), 30 days, 90 days or lifetime to see
stacked model/effort charts, completed goals, conversation leaderboards, activity
streaks and milestones. Turns are confirmed ChatGPT sends, excluding pairing and
duplicate confirmations. Tool counts include authenticated native work requests, excluding idle wait
loops and UI polling. Active time is an estimate from confirmed sends and native tool
intervals; it excludes chat wait loops and long unexplained idle gaps, and is not
a measurement of internal model reasoning or billed usage. Days use the server's
local timezone. Analytics works independently of turn renewal and the progress
journal. Pausing collection applies within a few seconds and preserves totals.
Private `usage-analytics.json` sidecars contain counters and opaque event IDs,
without message content or conversation URLs. Failed collection or reads remain
silent and never block automation. Writes are buffered; a crash can lose the last
few seconds of best-effort counters.

Use **Share** in Analytics to preview a compact card for the selected period,
then choose **Static PNG** or **Animated SVG**. The 1200 × 675 card includes turns, work
tool calls, completed goals, an activity strip, and `codexify.dev` branding.
Images are generated entirely in the browser, with no upload, hosting, public
link, conversation names or project paths. PNG download remains available when
image clipboard access is unsupported or denied. The SVG is self-contained,
with no scripts, fonts or images to fetch: counters race to the final totals,
activity bars fill, and flames and sparks celebrate before the totals hold.
The six-second loop respects the viewer's reduced-motion preference. You can
pause the preview; **Copy PNG** always copies the still image for chat apps.
Download the SVG and add it to your repository, then embed it in a README with
`![Codexify stats](./codexify-stats.svg)`. Image animation support depends on the
viewer; use the PNG where animations or SVG files are unsupported.

**Model cutoff estimates** appear in **Settings → Chats & goals**. Passive local
collection runs while Turn renewal is off, grouping inferred unfinished stops by
the actual model and effort (including fallback). Durations run from the confirmed
ChatGPT send to the last native tool call, excluding the offline detection delay.
Running tools, completed turns, requests for input, stopped/blocked goals, user
stops, renewal handoffs and durations over five hours are excluded. Resumed
activity withdraws the current estimate. These are inactivity-based estimates,
not proof of a ChatGPT timeout; the table shows count, median, P90 and range.
Up to 256 historical observations per chat are kept in a separate private
`turn-cutoff-estimates.json` sidecar, without message content or conversation URLs.
Collection and display are best effort: failures stay silent and cannot affect
chat delivery, goals, continuation, or configured turn limits.

Disable progress reporting separately with `agentChat.rolloutEnabled` (default
`true`). Existing journals are preserved. Either feature can run alone, or both
can run together.

Restart Codexify, refresh the connector's tools in ChatGPT Settings, and start a
new conversation. `get_agent_brief` reports that conversation's `CHAT.md` path,
stored with project metadata outside the repository by default. Every
conversation has its own file, even when several use the same checkout.

With UI widgets enabled, `setup` opens one persistent chat panel beneath the
setup controls. Chat tools update it without opening more cards. Send with the
arrow button or Return; Shift+Return inserts a newline. In **Settings → Chats & goals**,
`agentChat.defaultMessageMode` selects **Steer** (the default) or **Queue**.
**Cmd+Return** on macOS, or **Ctrl+Return** on Windows/Linux, uses the other mode
for that message only. Queued messages appear above the composer with **Steer**,
**Trash**, and **Edit** controls. Steering sends immediately ahead of the queue.

Queued messages and their attachments stay private until released. Codexify sends
one oldest queued message per completed request (`turn_completed`), or once the
agent is offline. `chat_await` means the current request needs user input and does
not release the queue. An unfinished or blocked goal holds all queued messages,
including during automatic goal continuations, until marked complete.
 One grey tick means saved
to `CHAT.md`, two grey ticks mean returned to the agent, and two blue ticks mean
acknowledged through `chat_read`, `chat_write`, `chat_await`, or `turn_completed`. The Agent indicator
shows online for calls under three minutes old, last seen from three to under five minutes,
and offline thereafter. These are tool-activity receipts, not proof of model
comprehension. A small header total counts model-visible Codexify tool calls in
the conversation. Between agent messages, a compact counter shows the calls since
the previous agent message and increments live while the agent works. Widget
polling and other app-only helpers do not inflate either count. Widget sends do
not start a new ChatGPT turn.

To monitor all persisted chats outside ChatGPT, run `codexify chat` on the
computer running the service and open the printed URL. The standalone view
lists conversations by their latest chat entry, with activity state and an
unread dot, and reuses the widget's message history, composer, receipts, file
downloads, and tool counters. Its private access token survives service restarts,
so the same URL keeps working while the configured port remains unchanged.
It connects to a separate localhost listener, not the shared MCP tunnel.
ChatGPT does not pass conversation titles to the
connector, so list titles are derived from chat entries (preferring the first
user entry). See the
[standalone chat reference](docs/REFERENCE.md#standalone-owner-chat).

The **New chat** button in standalone chat creates and verifies a corresponding
ChatGPT conversation. Choose an existing project, a Git repository or GitHub
PR/branch/commit URL, a saved worktree, or **Chat without a project**. The worktree
checkbox defaults to the configured worktree mode and can be overridden per chat.
Model and effort options are discovered from a fresh ChatGPT chat when the picker
opens.

To create these conversations inside a particular ChatGPT project, set the
optional `agentChat.automation.chatgptProjectUrl` setting:

```json
{
  "agentChat": {
    "automation": {
      "enabled": true,
      "chatgptProjectUrl": "https://chatgpt.com/g/g-p-PROJECT_ID/project"
    }
  }
}
```

Omit `chatgptProjectUrl` to create chats outside a ChatGPT project. Codexify uses
its isolated Chrome profile and the existing shared browser action queue.

### Offline browser fallback

The standalone page can optionally wake an offline agent through the user's
authenticated ChatGPT web session. This is a fallback for agent chat; it does not
add ChatGPT Web models to Codex or change the ordinary online delivery path. The
user pastes the exact ChatGPT conversation URL, verifies that conversation with a
one-time Codexify tool call, and chooses the model and effort in standalone chat.
Codexify never guesses from the most recent browser tab, a project, or
`openai/session`.

Install the optional native companion after enabling
`agentChat.automation.enabled`:

```bash
codexify browser install
codexify browser status
codexify browser login
```

The release installer can perform the same explicit opt-in with
`CODEXIFY_INSTALL_BROWSER=1`. No companion is downloaded or started for existing
installations while automation remains disabled. The `browser start`, `stop`, and
`reveal` commands manage the separate user-session process; restarting the
Codexify service does not terminate it.

While the agent is online, messages continue through `CHAT.md` and normal tool
results. After the existing five-minute offline threshold, unread messages or an
active goal can queue one bounded browser action. Codexify uses one shared browser
window, so responses are serialized and the window is never navigated away from a
generating turn. Visible stable answer text streams back to standalone chat;
`chat_write` remains the authoritative persisted reply when both paths produce
output.

Goals use `/goal set`, `/goal run`, `/goal stop`, `/goal clear`, and `/goal status`.
The model reads the exact revision through `goal_read`. It can create/update and
automatically activate a goal with `goal_set`, or resume a paused/blocked goal
with `goal_resume`. Both check the revision and deduplicate identical request IDs.
It must call `goal_finish` to mark it completed or blocked; a blocked result
requires a reason. Browser
automation is unofficial and deliberately fails closed when ChatGPT changes its
UI, the selected model or connector cannot be verified, approval is required, or
Send acceptance is ambiguous. See
[Optional browser automation](docs/REFERENCE.md#optional-browser-automation).

`agentChat.notifications.events` selects which alerts to receive: ordinary
messages, completed chats/goals, requests for input, blocked goals, chat errors,
and offline transitions. The example configuration enables completion, input,
blocked-goal and error alerts, with ordinary messages and offline alerts disabled.
Input requests alert once until the user replies; repeated `chat_await` calls and
server restarts do not repeat the alert. Errors with an automatic retry pending
stay quiet. See [Waiting and notifications](docs/REFERENCE.md#waiting-and-notifications).

Messages show a small local `HH:mm` timestamp at the bottom right, before the
ticks on user messages. Earlier local calendar days also show `YYYY-MM-DD`
before the time. Dates follow the viewer's timezone and update at midnight.

The panel renders Markdown tables, reference links, nested lists, and code.
For files, use the `chatLink` returned by `export_host_file` in the agent's
Markdown message; supported hosts download it without starting another turn.
Older sandbox links work when they identify an unambiguous export in this workspace.

You can also append your instructions to the file and save. The agent uses `chat_read`,
`chat_write`, and `chat_await`; other tool results also deliver unread messages
without consuming them. Optional Apprise notifications send the agent's Markdown
to ntfy, Pushover, and other supported services through one URL-based configuration.
Notifications use only the local Apprise backend and require Python with Apprise
installed. Schema migration renames an existing `markdownChat` block to
`agentChat` and converts the former native `ntfy` child to an Apprise ntfy
service URL. The feature is disabled by default and does not guarantee unlimited
ChatGPT runtime or alter OpenAI's usage limits.

See [Markdown chat configuration and semantics](docs/REFERENCE.md#markdown-chat)
for notifications, timeouts, schema refreshes, and append-only editing rules.

## Use it safely

**Codexify runs real commands with your user account's permissions.** Shell
execution is unrestricted and has no command allowlist. Project boundaries and
Git worktrees are not an operating-system sandbox, and commands can reach beyond
the project.

Start with a trusted repository, review requested actions, and keep credentials
out of chats and version control. A private tunnel keeps the MCP endpoint off the
public internet; it does not make inference local. Code and tool results returned
to ChatGPT are sent to OpenAI.

See the [security reference](docs/REFERENCE.md#security) for access controls,
command restrictions, and the authority of connected MCP servers.

## Help and everyday use

| Task | Command |
| --- | --- |
| Check configuration and connectivity | `codexify doctor` |
| Print or inspect configuration | `codexify config` · `codexify config get <key>` |
| Edit configuration | `codexify config edit` |
| Check the background service state | `codexify service status` |
| Follow the formatted service log | `codexify service logs -f` |
| Stop the background service | `codexify service stop` |
| Start it again | `codexify service start` |
| Restart it without changing login startup | `codexify service restart` |

To update a standard installation, ask ChatGPT to update Codexify. After the
service restarts, open the connector in ChatGPT Settings and click **Refresh** at
the bottom of its tool list. The [update guide](docs/REFERENCE.md#self-update)
explains the process.

For setup problems, start with [Troubleshooting](https://github.com/devnoname120/codexify/wiki/Troubleshooting).
Report reproducible bugs in [GitHub Issues](https://github.com/devnoname120/codexify/issues).

## Documentation

| Looking for... | Read |
| --- | --- |
| Setup and usage guides | [Wiki](https://github.com/devnoname120/codexify/wiki) |
| CLI options, configuration, and tool behavior | [Technical reference](docs/REFERENCE.md) |
| Additional MCP servers and reusable workflows | [MCP bridging](docs/REFERENCE.md#bridging-other-mcp-servers) · [Skills](docs/REFERENCE.md#skills) |
| Building or contributing | [Architecture](docs/ARCHITECTURE.md) · [Development commands](docs/REFERENCE.md#dev-commands) |

## License

[MIT](LICENSE). Codexify is an independent project, not affiliated with OpenAI.

## Related projects

### ChatGPT connectors and local MCP servers

These connect a ChatGPT conversation to tools on your machine, as Codexify does.

| Project | Focus |
| --- | --- |
| [CodexPro](https://github.com/rebel0789/codexpro) | Local coding tools, attachment import, and handoff plans. |
| [local-dev-mcp](https://github.com/harukary/local-dev-mcp) | Registered projects with file, Git, shell, browser, and mobile tools. |
| [codex-free](https://github.com/hypnguyen1209/codex-free) | Rust MCP bridge with coding tools, saved context, skills, and MCP aggregation. |
| [chatgpt-web-oauth-mcp](https://github.com/escapeWu/chatgpt-web-oauth-mcp) | OAuth connector with background jobs, terminal sessions, and optional CLI-agent delegation. |
| [Agentic MCP](https://github.com/hugolsramos01-bit/mcp-agentic-server) | Structured editing, Git worktrees, semantic navigation, and checkpoints. |
| [Local Coding Agent](https://github.com/LongNgn204/local-coding-agent) | MCP workspace with desktop tray apps, a dashboard, and browser-preview tools. |

### Browser harnesses and Codex model providers

These bring web-chat models into a coding client rather than making ChatGPT the
primary interface.

| Project | Focus |
| --- | --- |
| [codex-chatgpt-web](https://github.com/miuuyy/codex-chatgpt-web) | ChatGPT Web as selectable Codex models through an embedded browser, with optional access to the active Codex task's tools. |
