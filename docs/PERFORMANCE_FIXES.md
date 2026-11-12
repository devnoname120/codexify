# Browser and agent performance fixes

## Recovery readiness compatibility (2026-11-08 hourly check)

The first hourly check found a deployed compatibility gap: the transplanted
control route exposed `/v1/health?ready=1`, but the older installed coordinator
lacked `readyHealth()`. That endpoint returned a generic server error. The
targeted builder now installs the method and checks every coordinator method
called by the patched route before packaging. The check rejects the actual
previous archive for its missing readiness method and accepts the repair.

New-chat preparation also finishes a real account/idle verification after
releasing its window lease. Older native recovery clients immediately read
passive health after preparation; previously that read could see normal warming
and refuse recovery. The new verification populates the existing two-second
passive proof with the actual prepared account. A changed account or occupied
browser rejects preparation, and actual Send still performs fresh verification.

Validation: 93 focused tests passed, one Windows symlink case skipped, and
TypeScript checking passed. Tests cover the HTTP readiness wait, an incompatible
route/class pair, legacy preparation followed by repeated passive reads, and an
account change after preparation. The installed archive SHA-256 is
`19a4399b98f805b57b0720a21c5bd60efb5dcf41e8580bea2386e81e00ea8eff`.
Live verification returned authenticated readiness instead of HTTP failure,
New Vegas resumed tool calls, LiveArea accepted its continuation and resumed a
tool call, and the previously stalled LiveArea interaction worker completed.
The existing native server and resident commands were preserved.

## Hourly stall checks (2026-11-08)

At 10:28 UTC the PARAM.SFO worker's activity had stopped after 149 calls. Its
messages established that those calls were initialization retries, not task
work: an accepted creation followed by failed model restoration had left its
pairing unverified and expired. Dedicated-profile history matched the original
pairing-reference digest and claimed-continuation digest; the existing physical
conversation's initialization was then visually confirmed. The supported owner
pairing API sent a fresh verification reference to that same conversation.
Generation 2 verified successfully, the error cleared, and the active saved goal
and its normal timer were retained. The original accepted initialization was
not resent, no pairing state was fabricated, and no service was restarted.

Monitoring seeds every active goal, including workers whose coordinator has
completed or is absent. The 08:29 UTC check found this coverage gap while two
such workers were still making calls. The local monitor was corrected and all
nine monitor tests passed, including completed-parent and missing-parent cases.
This changes observation only; stopped or completed work is never resumed by
the sampler. An existing worker pairing-restoration error with an accepted
Send remains preserved while that worker continues working on its timed goal.

The user requested hourly inspection and repair. The active Codex app heartbeat
`codexify-hourly-stall-repair` is attached to this repair chat and runs every hour.
It inspects live goals, queues, browser actions, approvals and tool progress,
then performs supported recovery for confirmed stalls. Explicit pauses/stops,
active commands and uncertain sends must be preserved.

`node scripts/monitor-agent-performance.mjs browser-companion/artifacts/hourly-stall-check.json`
records bounded tool timings and per-chat progress deltas for successive checks.
The snapshot contains no credentials or message bodies. A call gap alone is not
a stall: inspect builds, timers, waiting tools and child activity first. Targeted
recovery observations are saved in `browser-companion/artifacts/hourly-recovery-state.json`.

The initial check found LiveArea progressing again while several conversations
were retrying loads. One worker had three confirmed unavailable loads and no
accepted retry Send. Supported conversation replacement preserved its task,
goal, workspace and pending context and began a new verified handoff.

The check also reproduced an approval-monitor regression: an empty two-second
poll acquired a window lease, invalidating the shared passive health result.
Slow health probes could then repeatedly put queued work into a warming wait.
Empty polls now check for a visible platform approval card read-only before
taking a lease, and skip active controller work. A regression runs frequent
empty polls across a slow real coordinator health probe and verifies that both
UI and queued work retain the same completed proof without an approval lease.

The corrected archive is deployed with SHA-256
`be59ce378a9a46caebd1684d64d0847653a646d98b744d26e4d6ef4fa1d1f8d9`.
The targeted patch also adds the coordinator's read-only busy getter to the
installed single-window class. Validation: 107 unique focused tests passed,
including 13 real-Chrome approval checks; one Windows symlink test skipped.
TypeScript checking passed. After initial warm-up, live passive health reads
returned authenticated/idle in 3–4 ms and queued work acquired the browser.

## Continuous Codexify request approval (2026-11-08)

The browser now checks its current conversation every two seconds for routine
Codexify approval cards, including file materialization while an existing model
turn is working. Checks do not overlap, navigate or send a prompt, and yield to
controller actions. They stop when the browser closes. Each check rereads
`autoApproveToolCalls`, so disabling the setting takes effect immediately.

Automatic checks accept the card's one-request button. They require the
configured connector name, account-bound app cache, current account proof and
trusted platform card structure. A later turn does not need another connector
mention; explicit conflicting identities still fail. Existing conversation-scope
approval remains available to the native supervisor. Checks have cancellation
and time budgets and do not accumulate an approval history cache.

The Windows updater tolerates a locked optional profile snapshot without changing
the profile, and verifies the existing browser launch after a short CLI readiness
timeout instead of immediately reporting a failed restart.

Validation: 70 focused tests passed, including 13 real-Chrome card/approval
regressions; one Windows symlink test was skipped. TypeScript checking and the
updater's PowerShell parser check passed. The initial approval archive SHA-256 was
`507d06a3a96d27aac6b841d892391e0283089dc25d0127ecb6854cedc39c3cc6`.
Live checks confirmed the enabled approval policy, authenticated browser, fresh
worker calls, and restored LiveArea pairing. A subsequent memory check reduced
the renderer heap from 1,196,202,600 to 52,083,544 bytes.

## Cancelled cleanup and health-flight recovery (2026-11-08)

Overnight logs confirmed that the memory guard reclaimed approximately 0.8 GiB
twice. A separate recurrence left the browser reporting busy without an active
Send, with a goal continuation queued behind it. Two cancellation paths could
retain browser ownership forever while the renderer itself remained responsive.

Retained maintenance and unsent-load cleanup now have a 30-second grace period.
If cleanup still has not settled, the old driver is retired and closed with a
bounded shutdown, allowing the existing native recovery to replace it. New work
cannot reuse the retired driver. Cleanup that settles within the grace period
keeps the existing browser. Attempted/accepted sends retain their uncertainty and
are never converted into an unsent retry.

Health inspection now has a five-second overall deadline and checks cancellation
between reads. A never-settling page/session observation cannot poison the shared
health promise indefinitely. Session attention checks stop issuing later reads
after cancellation.

An approval discovered before a new Send now keeps its approval event and attention
while its snapshot reports failed preparation with `sendAccepted:false`. This
lets the existing native supervisor handle the approval without incorrectly
treating that unsent message as an ambiguous accepted send.

Explicit approval recovery can hold one browser lease across target navigation
and approval; passive approval polling still never navigates. File-materialization
cards in later turns can inherit the connector from earlier structured user
mentions only when every matching identity agrees with the account-bound cached
app ID. Conflicting IDs, missing cache proof, wrong accounts/conversations,
unexpected options and untrusted card structures remain rejected. Real-Chrome
regressions cover the captured card/menu, inherited identities and changed targets.

The Windows browser-only updater now waits for real process exit and ignores
terminated process objects retained by other handles, rather than treating a
stale CIM row as a running browser.

Focused regressions cover forever-pending reads, recovery after a hung health
probe, late cancellation, cooperative cleanup, hung close, and approval before
Send. The targeted archive update preserves the existing native server, sessions,
goals and paused work.

## Windows stalled navigation and long-session memory repair (2026-11-07)

The installed single-window bundle held the context navigation gate until a
document load settled. An uncooperative load could block independent navigation
behind it. The targeted runtime patch installs the existing early gate release,
without enabling the separate multi-page implementation. A regression reproduces
the blocked navigation in the old installed function and passes with the repair.
Start spacing, server Retry-After, account proof and uncertain-Send rules remain.

The managed renderer was observed at 10.6 GiB of private process memory. That
observation establishes memory pressure, not the owner of every allocation.
Identity, connector, draft and retained-action caches are already bounded. New
private diagnostics expose numeric heap, DOM and companion-memory counters to
distinguish reachable page growth from old-document retention. They contain no
message text or heap snapshots.

Action cleanup now checks renderer heap at most once per minute per page. Above
768 MiB it requests garbage collection of unreachable objects through CDP while
the action still owns the page. It preserves cookies, drafts, browser disk caches,
live DOM and conversation state. Collection and session cleanup have deadlines;
failure cannot change the Send result. This is memory-pressure mitigation, not a
claim that arbitrary reachable ChatGPT application state can be reclaimed.

Verification: 25 navigation tests, 80 coordinator/observation tests, 33 surface
and memory tests, TypeScript checking, and real-Chrome navigation-cache and
650-turn observer tests passed. An isolated 80-switch soak with 500 turns per
page ended with one retained document and 2,555,168 heap bytes after collection.
First/last 20-switch medians were 1,972/2,033 ms on this machine; this is a local
fixture measurement, not a guarantee of ChatGPT network/model response speed.
The opt-in `long-session-node.test.ts` keeps this soak repeatable.

The browser-only installer pins archive checksums, waits for action ownership
to clear, keeps a rollback archive and profile/account files, and restarts the
browser in a finally block. The native server and saved goals remain running.

**Fast setup** in Settings → General or Browser enables the recommended features
with one revision-checked save, followed by the existing Restart action. New
quickstart configuration supplies these defaults without extra questions. An
existing explicit value or legacy presentation choice is preserved by the wizard;
the settings button explicitly applies the complete preset.

The preset selects Bones, Steer delivery, browser recovery, automatic retries,
turn renewal, progress tracking and agent controls, and reduces decorative
animations, sounds and recap interruptions. It preserves model/effort choices,
individual renewal limits, tool-approval policy, request-blocking policy and
per-chat opt-outs. External agent control is not enabled by this preset.
The same validated preset is available as `codexify config fast-setup` for
scripted installations; `--config <path>` selects the exact installation.

Queued dispatch notices new owner steering between sends and performs a
lightweight work scan before dispatching the next queued goal. This preserves
the current Send, priority, FIFO and real-error backoff, and avoids adding passive
browser probes to every dispatch. New-chat discovery and account verification
also share a browser lease, preventing worker navigation from appearing to be an
account change.

Team status shares validated task configuration and automation state within each
request. A 12-worker regression fixture performs 13 workspace resolutions and 13
state loads, including the owner. The next request reloads permissions, so access
revocation is not hidden by a persistent cache. Agent guidance starts with two to
four independent implementation workers and reuses them before expanding.

Windows service installation now explicitly selects Normal scheduler priority
(`4`). Task Scheduler's default (`7`) had put the local supervisor at Below
Normal, delaying it during worker builds. The guarded local updater also repairs
the running native process priority. The native service launcher explicitly
starts the server at Normal priority, including when an existing task definition
is protected against modification. This avoids requiring an administrator prompt
to fix an existing installation.

The managed browser now exposes **Settings → Browser → Browser simplicity**:

| Level | Behavior |
| --- | --- |
| Off | Full ChatGPT presentation. |
| Simplified | Reduced decoration/motion and deferred layout for older turns. |
| Bones | Simplified plus narrow navigation and plain message/code/composer styling. |

The saved field is `agentChat.automation.browserSimplicity`: `off`, `simplified`,
or `bones`. Restart applies a level. Omitting it preserves legacy `minimalUi`
behavior. Account, model/apps menus, Send, Stop, Copy, approvals, message text and
turn identifiers remain available. Off removes the owned styles, including after
navigation or script-world changes.

Healthy conversation loads and off-tab handoff revisits now default to one-second
spacing instead of 180 seconds. Explicitly saved loading intervals still win;
set `agentChat.automation.retryTimings.conversationLoadIntervalSeconds` to `1`
for the new default policy. Actual HTTP 429, Retry-After and unavailable-chat
backoffs remain enforced. The post-Send settling protection remains intact.

Composer, turn identity, Send acceptance and response observations have bounded,
cancellable reads so a hung renderer cannot hold the shared browser indefinitely.
Warm diff checkpoints validate their two current commit references before trying
legacy migration; missing baselines still initialize, and failures still refuse
mutation. This reduces warm checkpoint Git subprocesses from seven to three.

Concurrent edits no longer instruct agents to abandon the task. New workers
implement authorized assignments by default; an explicit read-only assignment or
parent restriction remains enforced. The coordinator continues its own
implementation while workers run. Routine reports need no acknowledgement.
Report deduplication includes the goal revision, and reassignment clears the
previous assignment's current report. The brief instructs agents to save
substantial implementation requests as active goals so automatic continuation
has an objective to restore.

The workflow follows [OpenAI's guidance on independent, bounded agent
workstreams](https://developers.openai.com/api/docs/guides/responses-multi-agent).
More agents do not make a single serialized browser Send concurrent.

## Verification evidence

- Companion suite: 211 passed; 124 platform/native opt-in skips.
- TypeScript typecheck passed.
- Focused native Rust checks: 50 passed, covering instructions, worker lifecycle,
  settings/configuration, checkpoint behavior and connector schema compatibility.
- Additional focused native checks: 59 passed, covering fast setup, configuration,
  quickstart, settings, new-chat account verification, team status and Auto mode.
- Full scheduler integration suite: 144 passed, one notification opt-in skipped;
  CLI integration suite: six passed. These ran with one test thread. An earlier
  concurrent run under build load had five loopback transport failures; all five
  passed in the complete serial rerun without production-code changes.
- Real-Chrome long-chat and handoff checks: two passed.
- Settings UI: six passed, including atomic fast-setup saves, failed-save retry,
  unsaved-edit protection, legacy compatibility and level save/reset.
- Cross-world style checks: seven passed, including Off and stale-script guards.
- Measured healthy handoff revisit: approximately 1.11 seconds. Unchanged verified
  handoff check: 45 ms.
- Synthetic 320-turn, 15,360-paragraph layout: median 2020.645 ms with full layout,
  22.858 ms with the common simplification containment (98.87% less layout work).
  This is neither an end-to-end model speedup nor an incremental comparison
  between Bones and the previous Simplified mode.
- Windows package staging moves the completed application within checked private
  scratch paths, avoiding about 700 MB of duplicate runtime copying; five
  packaging checks passed.

On this machine, a default debug-symbol Rust test build exhausted memory. The
focused test build uses `--jobs 1 --config profile.test.package.codexify.debug=0`
and `RUST_MIN_STACK=16777216` for large async test frames, reducing its footprint
without rebuilding all dependencies. A full Windows
package initially exceeded the available disk headroom. After space became
available, the complete companion package was built and passed its packaged
startup smoke check at
`browser-companion/artifacts/ready-20261030/codexify-browser-v2.3.15-windows-x64.zip`.
The earlier JavaScript-only bundle is a component rather than a standalone
installer. No live installation or profile was replaced during packaging.

## Local rollout

The optimized native binary and complete browser runtime are installed locally.
Fast setup is applied with Bones and one-second healthy conversation spacing.
The actual configuration preview preserved custom renewal limits, approval policy,
request blocking, connector selection and external-control settings. The profile,
previous native binary and previous browser runtime are backed up.

The old managed browser and its descendants had inherited Below Normal priority.
Normalizing the 13 verified managed processes restored an authenticated idle
health response in 281 ms after earlier probes exceeded 15 seconds. The native
launcher change gives future supervised server processes Normal priority.

The existing protected Windows task did not restart the service as its recovery
settings suggested. That fallback is now disabled before any mutation. Retried
local updates also use fresh readiness receipts so an old download endpoint
cannot be reused. A complete on-disk browser archive was validated and installed
directly after local HTTP transfers failed; all 405 extracted entries were
checked, including hashes of the manifest, launcher and app archive.

The owner subsequently started the updated service. Live native health is OK,
both native processes have Normal priority, and the managed browser is
authenticated and idle.

The Windows launcher now retains its native child process handle: Windows
PowerShell 5 otherwise returned a null exit code for a successful browser CLI
command, producing a false startup error with an empty stderr log. It also opens
the authenticated owner dashboard in the default browser. Readiness uses a small
authenticated connector-settings response rather than loading every conversation.
On this installation the previous list contained 105 chats and about 773 KiB;
one direct list request took 2265 ms, already beyond the native CLI's two-second
deadline. The replacement dashboard verification and shell-open succeeded in
786 ms. Proxy discovery and redirects are disabled for this fixed loopback check.
Private dashboard credentials are never printed or written to startup logs.

Bones now removes the actual modern navigation rail, Chat/Work header, home
welcome, voice controls, empty rounded page backdrop and Codexify badge. It uses
plain square surfaces and retains the composer, Apps, model/effort menus,
Send/Stop, Copy, account menu, messages, errors and approval surfaces. Its rules
are scoped to Bones, so Simplified and Off keep their own presentation.
The source and installed application archive contain these rules. The temporary
`~/.codexify/browser/fast-ui.css` override is empty; the compiled Bones stylesheet
now supplies the presentation. The previous CSS and app archives are saved beside
the other performance backups. Private-browser restarts preserved the profile.

## Setup Chat recovery

The live composer exposed `data-composer-navigation-target="reasoning"` without
`data-codex-intelligence-trigger`. The old effort selector consequently waited
for 30 seconds and failed despite a visible working button. The selector now
uses that explicit reasoning target, still scoped to the verified composer form.
Regression coverage excludes the neighboring Apps control.

Cancelled picker discovery now propagates its abort signal through row reads,
effort reads and restoration. Once cancelled it does not issue late arrows,
Escape or model restoration into a window another operation needs.

Passive startup plan checks now use authenticated session plan data. They no
longer navigate the shared browser to billing, preserving drafts and leaving the
window available for Setup Chat and actual actions. The cache includes account
identity and plan type. This reports the plan family (for example Pro), without
claiming an unverified billing tier such as Pro 20x. Explicit billing inspection
remains separately tested.

The installed browser app archive SHA-256 is
`053363f710fd8a19727ec127c71f564697e57bee44095bc3042a0308855bcb8a`.
The live owner Setup Chat endpoint succeeded with two discovered model entries
in 4134 ms; subsequent live preparations reused the verified catalogue in
423 ms and 351 ms. The browser remained authenticated, automatic control was
enabled, and its actual Bones screenshot and timing receipt are saved in
`browser-companion/artifacts/ready-20261030/`.

The original full ZIP predates these application-only recovery patches. The
current archive patch receipt is `browser-companion/artifacts/runtime-patch-ready.json`.
The patch preserves the Chrome runtime, profile and protocol version; only the
verified application archive is replaced after the private browser stops.

The native `chat` command source also uses the lightweight authenticated route
and bypasses proxies. This source correction is for the next native build; the
current launcher already uses its own verified direct check with the installed
binary.

## Agent execution and staged reliability update

New worker assignments already default to editing access. Enabled agent teams
now also default to delegation, through
`agentChat.features.defaultDelegation: true`; an explicit configuration false,
team disable, audit assignment, stop or project/account boundary still applies.
The normal worker brief explicitly authorizes implementation, builds, testing
and debugging. Current owner coordinators were granted full team development
access and delegation without changing their goals.

`agent_status` accepts `knownStatusRevision`. A matching freshly computed full
team revision returns a small unchanged receipt, reducing repeated response
bytes by over 95% in the twelve-worker regression. Actual tool activity, goal
revisions, permission changes and report-tail edits invalidate that receipt.
Detailed reports remain available on request. No cross-request authority cache
is introduced. The connector schema marks this optional capability separately.

Four actual private Chrome renderers were observed at Windows Idle priority
despite Chrome's background-throttling flags. Raising them to Normal restored
authenticated health in 621 ms under build load. The staged companion now
checks only its own browser-CDP renderer PIDs every two seconds and raises only
priorities below Normal. It never enumerates unrelated PIDs or lowers higher
priorities. Observations are bounded, nonoverlapping and stop with the context.

Passive health now returns a warming Busy response within 250 ms while a shared
real account inspection continues. It does not claim an unverified identity.
Login, capabilities and Send still wait for full verification, avoiding the
native status client's two-second timeout turning normal warm-up into a false
unavailable state.

The native file SHA-256 is
`3ee96033cada4bf5c82b43dd7f4cfc5f02b6805a1b2fa941c13d1efed30e7fe7`.
It is installed on disk; the existing server may still have the prior image
mapped. The tested browser candidate is pinned by `agent-update.json` beside
`agent-browser.asar`. Activation correctly deferred when an autonomous goal
started: the last check showed an active goal and twenty real tool calls.
`scripts/windows-activate-agent-update.ps1` checks for idle without mutation;
`-Apply` rechecks active work and identities before activation. It preserves the
existing supervisor, configuration, profile, explicit browser stops and backups.
The five-minute Codexify agent reliability heartbeat monitors real progress and
will activate the pending build only through that verified idle path.

Validation: five agent permission/workspace/idempotency tests and a config
default/false-persistence regression passed; four compact agent-tool and eight
connector-schema tests passed; the renderer-priority group passed 23 checks
with one existing opt-in Chrome skip; the coordinator group passed 68 checks,
including six warming/full-verification regressions. TypeScript typecheck and
six settings-UI checks passed.

## 2026-11-11 setup monitoring

The current New Vegas external coordinator successfully created a team after
an initial HTTP 422 state-conflict failure. The live status confirms no fixed
active-worker cap, batches of eight, two worker levels and delegation enabled.
Sixteen worker reservations are now visible; queued initialization and pairing
are distinct from actual project work. The coordinator has been notified of
the recovery, supported capacity and the pending patch.

Cold setup was observed spending almost twenty seconds opening a new page
before account, composer and model discovery. The coordinator now gives that
combined preparation ninety seconds instead of thirty, retaining cancellation,
bounded cleanup and fresh account verification. Four Node coordinator
regressions pass. The application-only candidate is checksum verified in
`browser-companion/artifacts/navigation-stall-ready.json`; it is staged, not
installed while current worker initialization owns the browser.

The Windows monitor now uses the authenticated owner inventory to filter the
native state layout. It excludes historical and nested test fixtures and
includes recent setup chats before they receive a goal. Native mixed path
separators are verified against the inventory rather than assumed. The five
discovery cases pass in both Windows PowerShell 5 and PowerShell 7.

Bones user bubbles now use the primary text color when their filled background
is removed. The correction is in source and applied live through the local
presentation API without a restart. A real isolated Chrome fixture verifies
light and dark contrast and preserves the filled bubble in Off mode. All five
Node/Chrome checks and TypeScript typecheck pass.

The active thirty-minute heartbeat is `codexify-agent-setup-and-stall-repair`,
attached to the current repair chat. It monitors actual owner/team state,
coordinates repairs with the current port chat, preserves explicit stops and
avoids duplicate requests or restarts during live work. Current compact
evidence is `browser-companion/artifacts/setup-monitor-20261111.json`.

The first heartbeat verified real tool activity in eleven of the sixteen new
workers; five startup conversations remain in the managed unsent-load retry
queue. The coordinator was informed and reservations were preserved. The patch
is still staged while browser actions are active. Its installer now requires
authenticated idle health and checks the full pooled action list and available
capacity; a null singular action ID or warming response cannot authorize a
restart. All eighteen updater process/idle guard checks pass.

The next check verified substantial real tool activity in all sixteen workers,
with scene streaming, NPC combat and ambient streaming reporting completed
goals. Initial API connection timeouts cleared without a restart. A live
screenshot exposed an active Stop control despite idle action health. Private
diagnostics now report generation state, and the updater requires explicit
generation-stopped proof for every action page. Unknown/older diagnostics fail
closed. Twenty-four updater checks and five Node/Chrome regressions pass; a
live guard attempt deferred before mutation, preserving the browser PID and
archive hash. The updated candidate remains staged in the pinned receipt while
accepted responses continue running; no current worker was stopped.

The later terrain/LOD revision-2 assignment exposed an orphaned handoff:
`reportComplete=false` remained pending from its prior turn, and ordinary
Resume did not release dispatch. Its files, assignment and reports were
preserved. After verifying the worker had no stop request, presence, queued
action or running command, a single idempotent pause/resume cycle through the
public agent-control API let the supervisor clear the old handoff. The exact
same revision is active again and has re-entered the normal browser retry
queue; actual new tool activity must still be verified. No state files were
edited and the shared server/browser were not restarted. The monitor now shows
goal/action revisions and pending-handoff age in both PowerShell versions.

Recovery was verified at 16:04 UTC when terrain's counter advanced from 135 to
138 with fresh native tool activity, revision 2 active, no stop, no queue and
no automation error. The coordinator was informed. Story-media generation-3
pairing remains separately pending after an accepted initialization Send;
that send was preserved without replay.

The 17:13 UTC check found actor-animation's newly released proof assignment at
revision 2 blocked by a failed, confirmed-unsent revision-1 goal action. A
revision-checked pause/resume alone preserved the stale failure. Cancelling
only that exact failed action through the owner API, then resuming revision 2,
produced a new revision-2 combined dispatch with confirmed acceptance and fresh
tool activity (326 to 399 calls). Scope, reports and queued owner instructions
were retained; no shared process or profile was restarted.

The preventive native source change releases only superseded, confirmed-unsent
pure goal failures during parent reassignment. It preserves accepted or
uncertain actions, message batches, active retries and explicit retry pauses.
Two store regressions cover 17 scenarios. The related recovery race test now
uses a test-only locked mutation helper and changes retry opt-out through a
private temporary settings file, matching its real hot-reload path. All three
targeted native tests pass. This native fix is source-only pending a safe
installation; installed executable and browser archive checksums are unchanged.

The next check recovered scripted-scenes replacement setup through the owner
creation-retry endpoint after verifying its generation-3 failure was unsent.
The HTTP observation timed out, but status had already advanced the same worker
to queued generation 4. The request was not replayed. Generation 4 then verified
and made fresh native calls, retaining goal revision 2 and its workspace,
history and reports. The coordinator received the verified recovery details.
Follow-up inspection verified actual task execution beyond pairing for this
worker and the previously retried death worker, with accepted goal Sends and
continuing native tool activity.
Compact monitoring now includes action kind, Send acceptance, pairing state,
cooldown and retry counts. Live PowerShell 7 and Windows PowerShell 5 snapshots
both read all 24 selected rows without failures; discovery regression checks
also pass in Windows PowerShell 5.

Owner inventory timeouts now preserve the monitor's independent service and
browser diagnostics. The JSON explicitly reports `inventoryAvailable=false`,
an `inventoryError`, and null agent/traversal collections, then exits with code
1. This distinguishes unknown inventory from a successfully observed empty
array. Prefix traversal never bypasses authenticated inventory discovery.
The CLI regression fixture verifies unavailable, empty and single-chat
inventories in full and compact output, including authorization, historical
filtering, array shapes, saved snapshots and exit codes. All six scenarios pass
in both PowerShell 7 and Windows PowerShell 5. The installed native inventory
endpoint was separately observed timing out while native health and the
published agent-client team status remained responsive; no shared restart was
performed to repair this monitoring failure path.
A later authenticated live snapshot completed again with zero state-read
failures, independently confirming that inventory availability had recovered.

The published JavaScript agent client now gives `capabilities`, `list` and
`status` one bounded retry after a transient connection failure. It freezes
the serialized arguments before awaiting anything, rereads the private
connection descriptor, and shares the original caller deadline/cancellation
with the retry delay. Setup, spawn and control requests never retry
automatically; HTTP errors also remain visible. Nine transport regressions
pass, including lost mutation responses, changed descriptors, mutable caller
arguments and cancellation during backoff.

This helper-only repair was atomically installed without restarting the
native service or browser. Installed list/status reads passed separately.
The prior code-only helper is backed up in
`browser-companion/artifacts/agent-client-before-read-retry-20261111.mjs`;
credentials and connection settings were untouched. The installed native
executable remains the earlier build, so its next startup can republish the
older embedded helper until a native build containing this source is safely
installed. The monitor records both helper hashes separately from the native
and browser archive hashes.

When full owner inventory times out, the Windows monitor now independently
queries the installed published JavaScript client for all paginated teams and
their worker statuses. It uses only `list` and `status`, four concurrent read
requests and one shared 20-second budget. Complete, partial and unknown team
observations remain distinct. Stops, revisions, roles, scopes and proof blocks
are retained in full saved snapshots; compact output omits long scope/proof
text. All-chat inventory stays explicitly unknown and the monitor still exits
1, even when every published team status is available. No raw historical scan
is used to fill that gap and no setup or Send is retried.

Fourteen Node transport/monitoring checks and ten CLI integration cases in
each of PowerShell 7 and Windows PowerShell 5 pass. Live installed-service
checks in both shells separately verified owner-inventory timeout plus a
complete published team snapshot: one ready team, 24 workers. This workspace
monitoring repair requires no native/browser restart. The native inventory
latency remains unresolved; a transient completed-worker lock cleared on a
second nonblocking observation and was not treated as its cause.

Native command cleanup now retains an unpolled exit status even when the
process produced no output. A caller can reconnect, start a different command,
and still poll the original result. The final poll removes the session; an
unread result still expires under the configured idle timeout. Expiry now
logs the session ID, PID and known exit code without logging command text.

A Windows process regression passed for quiet exits with codes 0 and 23.
Five related checks passed for transport replacement, final output drain and
idle expiry. This fix is tested in the source checkout; the installed native
service, browser and published helper checksums are unchanged. The separate
actor-animation R6 interruption remains unexplained: its retained receipt
shows that the proof stopped before compilation and ran zero tests. The proof
was not rerun, and the installed five-minute idle timeout was left in place.

On November 12 the installed browser retired its window twice after cancelled
maintenance failed to settle. The old log did not identify the operation. An
isolated test using the installed archive reproduced the same retirement when
a cancelled presentation read never settled. The candidate lets a read-only
presentation probe release its lease on cancellation. Screenshots and actions
that can change the page retain their existing cleanup rules. Future retirement
logs name the operation. The same archive test verified a subsequent read on
the candidate. This browser repair is staged pending a safe activation window.

The Windows installer can now wait for the managed browser to exit naturally.
It takes the native lifecycle lock, rechecks the process inventory and pinned
checksums, then atomically stages the archive with a rollback copy for the next
managed launch. An explicit stop defers staging. Nine fixture scenarios passed,
including a replacement process starting during preflight and a real process
exit observed by the watcher. Activation state is recorded in
`browser-companion/artifacts/navigation-stall-activation.json`.

The watcher staged the browser repair at 06:48 UTC on November 12 after the old
process exited. The replacement launched from archive SHA256
`0d32761fee3f89380b108c25de7f8ae96756f40284bc03435be85d784a467f3a`.
The rollback archive matched the previous checksum and all three active
workers subsequently advanced. The deployment notice reached the coordinator.

A later action deadline was followed by a Windows Chrome profile singleton
conflict. The orphan recovery script now holds and verifies the process handle,
terminates that orphan, and waits for its exit before returning to the launcher.
Four Windows fixtures cover the live parent, parent PID reuse, process handle
reuse and exit timeout. The candidate bundle differs semantically only in this
recovery script. Its exact role in the historical startup conflict remains
unconfirmed. The follow-up archive is staged for the next natural exit.
