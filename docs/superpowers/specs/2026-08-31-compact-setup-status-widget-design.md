# Compact Setup Status Widget

## Problem

The current setup widget is visually too large for the amount of information it normally carries. It duplicates agent-only orchestration text, presents update/schema state as dashboard cards instead of compact status lines, renders doctor output as an unstyled text block, and offers no explicit retry for the latest-release check.

The desired behavior is a compact status surface that stays quiet when everything is healthy, exposes maintenance actions exactly where they are relevant, performs diagnostics in the background without blocking the agent, and expands only when there is something worth the user's attention.

## Goals

- Keep the healthy setup widget compact and centered.
- Show Codexify and connector-schema status as simple status rows.
- Keep a manual `Check for updates` action available next to the Codexify status.
- Show `Upgrade` next to the Codexify version only when a newer release is known.
- Show `Refresh` next to the connector-schema version only when the schema is stale or cannot be proven current.
- Run doctor asynchronously from the widget after setup completes, without delaying the agent's project-selection and `get_agent_brief` flow.
- Hide automatic doctor output when healthy.
- Surface warning/failure state with clear color semantics, and automatically expand failure details.
- Provide `Autofix` for warning/failure doctor results by asking ChatGPT to investigate and repair the findings.
- Make `Refresh` derive the connector slug from the same-origin ChatGPT widget sandbox and open the connector settings directly, with a safe generic Plugins fallback.
- Keep agent-only setup instructions available to the model while removing them from the user-facing widget.
- Remain responsive on mobile, tablet, and desktop hosts.

## Non-goals

- The widget does not perform arbitrary repair actions itself; `Autofix` delegates investigation and repair to ChatGPT.
- `Refresh` does not mutate connector state itself; it only opens ChatGPT settings and tells the user where the host-side **Refresh** control is.
- The setup widget does not replace the existing restart-safe self-update progress widget. Once the update is accepted, the existing self-update lifecycle remains authoritative.

## Healthy-state layout

The default widget is a single compact card with a normal maximum width of **500 px**. Its primary rows are:

```text
Codexify: v1.2.0 ✓
Connector schema: v1.2.0 ✓
[Check for updates] [Doctor]
```

There is no duplicate title/header, separate version badge, subtitle, dashboard grid, or user-visible `nextStep` instruction.

The setup response continues to contain the model-facing next step, such as selecting the project and calling `get_agent_brief`. The widget simply stops rendering that field.

## Codexify update states

### Current

```text
Codexify: v1.2.0 ✓
[Check for updates]
```

`Check for updates` remains available and invokes a dedicated app-only, read-only latest-release check. It updates only the Codexify row and does not rerun setup or its conversation-authorization flow.

### Update available

```text
Codexify: v1.2.0 → v1.3.0 available     [Upgrade] [Check for updates]
```

`Upgrade` is row-local and appears only while setup or the manual update check has positively identified a newer release. The existing self-update widget remains responsible for showing the checksum-bound changelog and monitoring installation after the user accepts the update.

### Update check unavailable

The row indicates that update status could not be determined and retains `Check for updates` as a retry. Doctor may independently report the release-check warning.

### Ahead of published release

Treat this as healthy/informational. Do not offer `Upgrade`; retain `Check for updates`.

## Connector-schema states

### Current

```text
Connector schema: v1.2.0 ✓
```

No button is shown.

### Stale or unknown

```text
Connector schema: v1.1.0 · refresh required   [Refresh]
```

`Refresh` sits on the schema row. The widget walks upward through same-origin iframe ancestors until the first cross-origin boundary. ChatGPT's widget sandbox ancestor has a hostname of the form `asdk_app_<slug>.web-sandbox.oaiusercontent.com`; the widget extracts only that `<slug>` and builds the relative `#settings/Plugins/plugin_asdk_app_<slug>:~:text=Information-,Refresh,-Connected` hash. If no validated sandbox slug is found, it falls back to `#settings/Plugins`.

The widget passes that relative hash directly to `window.openai.openExternal`, which preserves the current ChatGPT page without reconstructing it from `document.referrer`. The portable `ui/open-link` host request remains the compatibility fallback. The widget never attempts to reach through the cross-origin boundary or assign the outer frame's location directly.

After the link request is accepted, the widget tells the user to select Codexify if necessary, scroll below the list of tools, and click **Refresh**. Link-opening failures are shown inline and never fall back to an agent prompt.

The deployment-specific `chatgptConnectorSettingsUrl` setting and server-side connector-ID request plumbing remain unnecessary: the browser sandbox already exposes the routing slug without making it model-visible.

## Doctor behavior

### Background execution

After the setup widget initializes and renders the setup result, it starts an app-only `doctor` tool call asynchronously. The original setup invocation has already completed, so the agent can immediately continue with project selection and `get_agent_brief`.

The automatic doctor call is not awaited by setup and does not become part of model context.

### Healthy result

If doctor returns no warnings or failures, its automatic result remains visually hidden. The `Doctor` button remains available for an explicit rerun.

A manual `Doctor` run may show the complete report even when healthy.

### Warning-only result

Show a compact warning strip, but do not automatically dump the full diagnostic report:

```text
Doctor: 2 warnings                     [Doctor] [Autofix]
```

### Failure result

Automatically expand the actionable doctor details:

```text
Doctor: 1 failure · 1 warning          [Doctor] [Autofix]
FAIL service — Native service is stopped
WARN release — Check unavailable
```

Per-check colors are:

- pass: green
- warning: amber
- failure: red
- skipped: muted gray

The complete report remains available from the explicit `Doctor` action.

### Structured doctor result

The app-only doctor tool should return the existing `DoctorReport` structure as structured data in addition to any human-readable text used for compatibility. The widget must not parse the CLI-oriented human report to determine status or styling.

## Autofix

`Autofix` appears whenever doctor reports at least one warning or failure.

It does not invoke remediation commands directly. Instead, the widget sends a follow-up message into the current ChatGPT conversation using the portable UI message operation, with the ChatGPT compatibility API as fallback when necessary.

The message includes the structured warning/failure records and asks the agent to:

- diagnose the actual causes;
- fix everything that is appropriate to fix;
- treat doctor remediation strings as hints rather than commands to execute blindly;
- verify the repairs;
- rerun doctor after the repair work.

The prompt should include only warning/failure findings unless additional context is required, keeping the follow-up concise while preserving IDs, summaries, details, and remediations.

If the host cannot send a follow-up message, `Autofix` becomes disabled or reports that the action is unavailable rather than silently doing nothing.

## Interaction and sizing

The widget targets a compact **500 px** maximum width and remains fluid below that host width. Every state transition that changes rendered height reports the new component size through the existing MCP Apps size-change notification path. Relevant transitions include:

- manual update checks;
- automatic doctor completion;
- manual doctor runs;
- doctor detail expansion;
- update scheduling output;
- schema-settings opening and Autofix message status.

Respect reduced-motion preferences. No transition is required for correctness.

## Upgrade action

`Upgrade` calls the existing app-accessible `self_update` tool with `confirm: true`, because the click itself is the user's explicit update request.

After self-update is accepted, the existing self-update widget owns download verification, changelog-from-archive display, restart monitoring, timeout handling, rollback/failure state, and the final connector-refresh instruction.

The setup widget should not duplicate restart monitoring.

## Error handling

- Setup release check fails: show a non-fatal unavailable state and retain the manual `Check for updates` retry.
- Manual update check fails: retain the current version, show a non-fatal unavailable state, and leave the retry enabled.
- Doctor tool call fails: show a compact diagnostic-call failure with a manual `Doctor` retry.
- Doctor has warnings/failures: show `Autofix`.
- `Autofix` follow-up message fails: report the failure in the widget and retain the doctor findings.
- `Refresh` settings-link opening fails: report the failure in the widget and retain the stale-schema state.
- Self-update failures retain the updater's existing durable failure/rollback status UI.

## Testing

### Rust/tool tests

- doctor app-only metadata remains private and widget-accessible;
- doctor structured output validates against its schema and matches `DoctorReport` counts/statuses;
- manual update-check tool is app-only/private, read-only, and returns the same status vocabulary as setup;
- setup output continues to contain the agent `nextStep` even though the widget no longer renders it;
- connector stale/current/unknown states preserve existing schema comparison behavior.
- setup output no longer returns a connector settings URL, and the obsolete settings-link configuration/request plumbing is absent.

### Widget tests

- healthy state has no `Upgrade`, `Refresh`, doctor summary, or agent-only next-step text, but has `Check for updates` and `Doctor`;
- update-available state has row-local `Upgrade` plus `Check for updates`;
- current state retains `Check for updates`;
- stale schema has row-local `Refresh`;
- `Refresh` extracts the slug only from a matching `asdk_app_<slug>.web-sandbox.oaiusercontent.com` same-origin ancestor, uses the generic Plugins route when absent, and never sends an agent prompt;
- `Refresh` passes the relative settings hash to `window.openai.openExternal` first, with `ui/open-link` as the compatibility fallback;
- automatic healthy doctor result stays hidden;
- warning-only doctor result shows compact summary plus `Autofix`;
- failure doctor result expands actionable diagnostics automatically;
- manual doctor can display a healthy full report;
- `Autofix` sends the structured findings in a follow-up prompt;
- mobile/tablet widths remain fluid below the desktop target.

### Integration/manual verification

Exercise the widget in representative host widths around 390 px, 768 px, ChatGPT-like inline desktop width, and wide desktop. Verify healthy, update-available, update-check-error, stale-schema, unknown-schema, warning-only doctor, failure doctor, Autofix failure, direct-slug Refresh, generic-fallback Refresh, and link-opening failure states in light and dark themes.
