# Compact Setup Status Widget

## Problem

The current setup widget is visually too large for the amount of information it normally carries. It duplicates agent-only orchestration text, presents update/schema state as dashboard cards instead of compact status lines, and renders doctor output as an unstyled text block. It also cannot show the changelog before the user commits to an update.

The desired behavior is a compact status surface that stays quiet when everything is healthy, exposes maintenance actions exactly where they are relevant, performs diagnostics in the background without blocking the agent, and expands only when there is something worth the user's attention.

## Goals

- Keep the healthy setup widget compact and centered.
- Show Codexify and connector-schema status as simple status rows.
- Show `Upgrade` only when a newer Codexify release is known.
- Show `Refresh` only when the connector schema is stale or cannot be proven current.
- Let the user inspect the relevant changelog before choosing whether to upgrade.
- Run doctor asynchronously from the widget after setup completes, without delaying the agent's project-selection and `get_agent_brief` flow.
- Hide automatic doctor output when healthy.
- Surface warning/failure state with clear color semantics, and automatically expand failure details.
- Provide `Autofix` for warning/failure doctor results by asking ChatGPT to investigate and repair the findings.
- Keep agent-only setup instructions available to the model while removing them from the user-facing widget.
- Render the supported changelog Markdown safely, including nested lists and images, without injecting raw HTML.
- Remain responsive on mobile, tablet, and desktop hosts.

## Non-goals

- The widget does not perform arbitrary repair actions itself; `Autofix` delegates investigation and repair to ChatGPT.
- The widget does not expose a manual `Check for updates` button. Setup already performs the bounded latest-release check.
- Changelog preview failure does not prevent upgrading.
- The Markdown renderer does not execute embedded HTML, scripts, iframes, or arbitrary Markdown extensions.
- The setup widget does not replace the existing restart-safe self-update progress widget. Once the update is accepted, the existing self-update lifecycle remains authoritative.

## Healthy-state layout

The default widget is a single compact card with a normal maximum width of **500 px**. Its primary rows are:

```text
Codexify: v1.2.0 ✓
Connector schema: v1.2.0 ✓
[Doctor]
```

There is no duplicate title/header, separate version badge, subtitle, dashboard grid, or user-visible `nextStep` instruction.

The setup response continues to contain the model-facing next step, such as selecting the project and calling `get_agent_brief`. The widget simply stops rendering that field.

## Codexify update states

### Current

```text
Codexify: v1.2.0 ✓
```

No update-related button is shown.

### Update available

```text
Codexify: v1.2.0 → v1.3.0 available     [Upgrade]
What changed                                  +
```

`Upgrade` is row-local and appears only while setup has positively identified a newer release. Expanding `What changed` loads the changelog preview lazily; setup itself does not wait for changelog retrieval.

If several releases were skipped, the changelog preview contains every release section in the interval `(current, target]`, newest first, using the same interval-selection semantics as self-update.

### Update check unavailable

The row may indicate that update status could not be determined, but no `Check for updates` action is added. Doctor may independently report the release-check warning.

### Ahead of published release

Treat this as healthy/informational. Do not offer `Upgrade`.

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

`Refresh` sits on the schema row and opens the existing ChatGPT connector-settings route. The existing connector-ID/configured-URL resolution remains the source of that destination.

If a usable settings destination cannot be resolved, the widget displays the refresh requirement without a dead button and keeps the existing explanatory fallback text available when needed.

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

## Changelog preview architecture

The current self-update flow obtains `CHANGELOG.md` only after downloading and extracting the release archive. That is too late for an upgrade-decision UI.

Add a dedicated app-only/private changelog-preview action. It receives or derives the already-known current and target versions and returns only the bounded changelog sections relevant to that exact upgrade interval.

Requirements:

- fetch from the exact published target tag, never an unversioned branch tip;
- enforce the existing changelog file and selected-output byte limits;
- reuse `select_changelog_sections` semantics rather than duplicating interval logic;
- cache successful preview data with the latest-release inspection so repeatedly opening the disclosure is cheap;
- keep the changelog component-only/app-only so large release notes do not enter model context;
- make failure non-blocking: the widget shows `Changelog unavailable` and leaves `Upgrade` enabled.

The post-download self-update changelog remains independent. It is still useful because it is extracted from the checksum-verified release archive that is actually being installed.

## Changelog Markdown subset

Render Markdown into DOM nodes; do not insert generated HTML with `innerHTML` and do not allow raw HTML from the source.

Supported block syntax:

- headings used by the changelog (`#`, `##`, `###` at minimum);
- paragraphs;
- unordered lists;
- ordered lists;
- nested unordered lists;
- nested ordered lists;
- mixed ordered/unordered nesting.

Supported inline syntax:

- inline code;
- links;
- images;
- linked images;
- italic;
- bold.

Links must accept only safe external schemes (`https`, and `http` only where existing policy explicitly permits it). Relative changelog links resolve against the exact release tag, not the default branch.

Images scale to the content width and never enlarge the widget beyond its text-based width calculation. Broken/unavailable images degrade to alt text or a compact unavailable-image presentation.

## Remote image handling

The widget CSP should not be opened to arbitrary image hosts.

Use an app-only bounded image-fetch path for Markdown images that are not already covered by a narrowly allowed static resource domain. The fetch path must:

- allow only validated public HTTPS URLs;
- reject credential-bearing URLs and unsafe/private destinations;
- revalidate redirects;
- bound download size and time;
- require an image content type and reject malformed/non-image responses;
- return image bytes only to the component, never model-visible content.

Relative release images resolve against the exact tagged repository state before fetching.

Linked images use the image-fetch path for the visual asset and the same safe-link handling as ordinary links for the click destination.

## Changelog wrapping and scrolling

The changelog is vertical-scroll only. It must not require a horizontal scrollbar for prose, URLs, paths, inline code, or list content.

Representative styling:

```css
.changelog-body {
  overflow-y: auto;
  overflow-x: hidden;
  overflow-wrap: anywhere;
  white-space: normal;
}

.changelog-body img {
  display: block;
  max-width: 100%;
  height: auto;
}
```

The body receives a bounded maximum height so long changelogs scroll inside the widget instead of making the conversation card arbitrarily tall.

## Dynamic width

The closed/normal widget target is **500 px**.

When the changelog is expanded, the widget may become wider to reduce excessive wrapping, but it must remain bounded. The conceptual rule is:

```text
desired width =
min(
  host available width,
  680 px,
  max(500 px, measured changelog content width + horizontal chrome)
)
```

The measurement is performed after rendering the changelog and is based on text/list content that benefits from additional width. Images, pathological URLs, long filesystem-like tokens, and other intentionally wrappable atomic content do not force expansion.

This is deliberately not a protocol assumption about ChatGPT's own inline-widget width. The host may provide less than 500 px; in that case the widget remains fluid and uses `width: 100%` within the available host width.

When the changelog is closed, the widget returns to the 500 px target.

## Interaction and sizing

Every state transition that changes rendered height or desired width reports the new component size through the existing MCP Apps size-change notification path. Relevant transitions include:

- opening/closing `What changed`;
- changelog preview loading/completion/failure;
- automatic doctor completion;
- manual doctor runs;
- doctor detail expansion;
- update scheduling output;
- schema-refresh notices.

Respect reduced-motion preferences. No transition is required for correctness.

## Upgrade action

`Upgrade` calls the existing app-accessible `self_update` tool with `confirm: true`, because the click itself is the user's explicit update request.

After self-update is accepted, the existing self-update widget owns download verification, changelog-from-archive display, restart monitoring, timeout handling, rollback/failure state, and the final connector-refresh instruction.

The setup widget should not duplicate restart monitoring.

## Error handling

- Setup release check fails: show non-fatal unavailable state; no manual update-check button.
- Changelog preview fails: keep `Upgrade` enabled and show an inline unavailable message.
- Doctor tool call fails: show a compact diagnostic-call failure with a manual `Doctor` retry.
- Doctor has warnings/failures: show `Autofix`.
- `Autofix` follow-up message fails: report the failure in the widget and retain the doctor findings.
- Connector settings URL unavailable: show refresh-required text without an unusable link/button.
- Remote changelog image fails: preserve text layout and render alt/fallback content.

## Testing

### Rust/tool tests

- doctor app-only metadata remains private and widget-accessible;
- doctor structured output validates against its schema and matches `DoctorReport` counts/statuses;
- changelog-preview tool is app-only/private and bounded;
- exact-tag changelog retrieval rejects invalid versions/tags and oversized content;
- preview interval selection reuses the same semantics as self-update;
- preview failures do not affect update availability;
- setup output continues to contain the agent `nextStep` even though the widget no longer renders it;
- connector stale/current/unknown states preserve existing schema comparison behavior.

### Widget tests

- healthy state has no `Upgrade`, `Refresh`, doctor summary, or agent-only next-step text;
- update-available state has row-local `Upgrade` and collapsible `What changed`;
- current state never shows a `Check for updates` button;
- stale schema has row-local `Refresh`;
- automatic healthy doctor result stays hidden;
- warning-only doctor result shows compact summary plus `Autofix`;
- failure doctor result expands actionable diagnostics automatically;
- manual doctor can display a healthy full report;
- `Autofix` sends the structured findings in a follow-up prompt;
- changelog renderer covers inline code, links, images, linked images, italic, bold, nested bullets, nested numbered lists, and mixed nesting;
- Markdown source containing raw HTML cannot inject DOM/script content;
- long URLs/paths and nested list content do not create horizontal scrolling;
- images remain within the changelog width;
- dynamic width returns to 500 px when changelog closes, can expand toward 680 px for useful text width, and never exceeds host width;
- mobile/tablet widths remain fluid below the desktop target.

### Integration/manual verification

Exercise the widget in representative host widths around 390 px, 768 px, ChatGPT-like inline desktop width, and wide desktop. Verify healthy, update-available, stale-schema, warning-only doctor, failure doctor, changelog loading/error, and multi-release changelog states in light and dark themes.
