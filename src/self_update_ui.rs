use rmcp::model::{MetaObject, Resource, ResourceContents};
use serde_json::json;

use crate::self_update::SelfUpdateReceipt;

pub const SELF_UPDATE_UI_URI: &str = "ui://codexify/self-update/v1/mcp-app.html";
pub const SELF_UPDATE_UI_MIME_TYPE: &str = "text/html;profile=mcp-app";
pub const SELF_UPDATE_RESULT_META_KEY: &str = "io.github.devnoname120/codexify/self-update";

pub fn tool_meta() -> MetaObject {
    serde_json::from_value(json!({
        "ui": {
            "resourceUri": SELF_UPDATE_UI_URI,
            "visibility": ["model", "app"]
        },
        "ui/resourceUri": SELF_UPDATE_UI_URI,
        "openai/outputTemplate": SELF_UPDATE_UI_URI,
        "openai/widgetAccessible": true,
        "openai/toolInvocation/invoking": "Downloading and verifying Codexify...",
        "openai/toolInvocation/invoked": "Codexify update prepared"
    }))
    .expect("static self-update tool metadata must be an object")
}

pub fn resource_meta() -> MetaObject {
    serde_json::from_value(json!({
        "ui": {
            "prefersBorder": false,
            "csp": {
                "connectDomains": [],
                "resourceDomains": []
            }
        },
        "openai/widgetPrefersBorder": false,
        "openai/widgetDescription": "Shows verified Codexify release notes and monitors the detached update through restart.",
        "openai/widgetCSP": {
            "connect_domains": [],
            "resource_domains": []
        }
    }))
    .expect("static self-update resource metadata must be an object")
}

pub fn result_meta(receipt: &SelfUpdateReceipt) -> MetaObject {
    let mut meta = MetaObject::new();
    meta.0.insert(
        SELF_UPDATE_RESULT_META_KEY.to_string(),
        json!({
            "status": receipt.status,
            "currentVersion": receipt.current_version,
            "targetVersion": receipt.target_version,
            "updateId": receipt.update_id,
            "serviceRestart": receipt.service_restart,
            "logPath": receipt.log_path,
            "changelog": receipt.changelog.as_deref()
        }),
    );
    meta
}

pub fn resource() -> Resource {
    Resource::new(SELF_UPDATE_UI_URI, "codexify-self-update")
        .with_title("Codexify update")
        .with_description("Verified release notes and restart-safe self-update progress")
        .with_mime_type(SELF_UPDATE_UI_MIME_TYPE)
        .with_size(SELF_UPDATE_UI_HTML.len() as u64)
        .with_meta(resource_meta())
}

pub fn contents() -> ResourceContents {
    contents_for_uri(SELF_UPDATE_UI_URI).expect("current self-update UI URI must be supported")
}

pub fn contents_for_uri(uri: &str) -> Option<ResourceContents> {
    (uri == SELF_UPDATE_UI_URI).then(|| {
        ResourceContents::text(SELF_UPDATE_UI_HTML, uri)
            .with_mime_type(SELF_UPDATE_UI_MIME_TYPE)
            .with_meta(resource_meta())
    })
}

pub const SELF_UPDATE_UI_HTML: &str = r####"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Codexify update</title>
<style>
:root {
  color-scheme: light dark;
  --canvas: transparent;
  --card: var(--color-background-primary, light-dark(#ffffff, #181818));
  --surface: var(--color-background-secondary, light-dark(#f5f6f7, #242424));
  --surface-strong: light-dark(#eceef0, #2d2d2d);
  --text: var(--color-text-primary, light-dark(#171717, #f4f4f4));
  --muted: var(--color-text-secondary, light-dark(#62666d, #a8a8a8));
  --border: var(--color-border-primary, light-dark(#d9dce1, #3b3b3b));
  --accent: light-dark(#1167d8, #69a7ff);
  --success: light-dark(#137333, #54d17a);
  --danger: light-dark(#b3261e, #ff7b72);
  --warning: light-dark(#8a5100, #f2c66d);
  --shadow: light-dark(0 8px 28px rgba(22, 28, 36, .08), 0 8px 28px rgba(0, 0, 0, .24));
}
* { box-sizing: border-box; }
html, body { margin: 0; min-width: 0; background: var(--canvas); color: var(--text); }
body { padding: 2px; font-family: ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
button, summary { font: inherit; }
.card {
  overflow: hidden;
  border: 1px solid var(--border);
  border-radius: 16px;
  background: var(--card);
  box-shadow: var(--shadow);
}
.hero { padding: 18px 18px 15px; }
.title-row { display: flex; align-items: flex-start; gap: 12px; }
.indicator {
  flex: 0 0 auto;
  width: 30px;
  height: 30px;
  display: grid;
  place-items: center;
  border-radius: 10px;
  background: color-mix(in srgb, var(--accent) 13%, transparent);
  color: var(--accent);
}
.indicator.running::before {
  content: "";
  width: 14px;
  height: 14px;
  border: 2px solid color-mix(in srgb, currentColor 24%, transparent);
  border-top-color: currentColor;
  border-radius: 50%;
  animation: spin .9s linear infinite;
}
.indicator.success { color: var(--success); background: color-mix(in srgb, var(--success) 13%, transparent); }
.indicator.failure { color: var(--danger); background: color-mix(in srgb, var(--danger) 13%, transparent); }
.indicator.warning { color: var(--warning); background: color-mix(in srgb, var(--warning) 15%, transparent); }
.indicator.success::before { content: "✓"; font-size: 18px; font-weight: 750; }
.indicator.failure::before { content: "×"; font-size: 22px; line-height: 1; font-weight: 650; }
.indicator.warning::before { content: "!"; font-size: 18px; font-weight: 750; }
.heading { min-width: 0; flex: 1; }
h1 { margin: 0; font-size: 16px; line-height: 1.3; letter-spacing: -.01em; }
.versions { margin-top: 3px; color: var(--muted); font: 12px/1.4 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
.status { margin: 13px 0 0; font-size: 13px; line-height: 1.45; }
.detail { margin: 5px 0 0; color: var(--muted); font-size: 12px; line-height: 1.45; }
.progress-track { height: 5px; margin-top: 14px; overflow: hidden; border-radius: 999px; background: var(--surface-strong); }
.progress-bar { height: 100%; width: 8%; border-radius: inherit; background: var(--accent); transition: width .35s ease; }
.progress-bar.success { background: var(--success); }
.progress-bar.failure { background: var(--danger); }
.phase-row { display: flex; justify-content: space-between; gap: 12px; margin-top: 8px; color: var(--muted); font-size: 11px; }
.elapsed { white-space: nowrap; font-variant-numeric: tabular-nums; }
.action-row { display: flex; align-items: center; gap: 10px; margin-top: 13px; }
.retry {
  appearance: none;
  border: 1px solid var(--border);
  border-radius: 9px;
  padding: 6px 10px;
  background: var(--surface);
  color: var(--text);
  cursor: pointer;
  font-size: 12px;
  font-weight: 620;
}
.retry:hover { background: var(--surface-strong); }
.retry:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
.instruction {
  margin-top: 13px;
  padding: 10px 11px;
  border-radius: 10px;
  background: color-mix(in srgb, var(--success) 9%, var(--surface));
  color: var(--text);
  font-size: 12px;
  line-height: 1.48;
}
.notes { border-top: 1px solid var(--border); }
.notes summary {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 12px 18px;
  cursor: pointer;
  list-style: none;
  font-size: 13px;
  font-weight: 650;
}
.notes summary::-webkit-details-marker { display: none; }
.notes summary::after { content: "+"; color: var(--muted); font-size: 18px; font-weight: 400; }
.notes[open] summary::after { content: "−"; }
.notes-body { max-height: 360px; overflow: auto; padding: 0 18px 16px; color: var(--text); }
.notes-body h2, .notes-body h3 { margin: 13px 0 7px; font-size: 13px; line-height: 1.35; }
.notes-body h2:first-child, .notes-body h3:first-child { margin-top: 2px; }
.notes-body p { margin: 6px 0; font-size: 12px; line-height: 1.5; }
.notes-body ul { margin: 6px 0 9px; padding-left: 20px; }
.notes-body li { margin: 4px 0; font-size: 12px; line-height: 1.5; }
.notes-unavailable { padding: 0 18px 16px; color: var(--muted); font-size: 12px; line-height: 1.45; }
.debug-timing { padding: 9px 18px 12px; border-top: 1px solid var(--border); color: var(--muted); font: 10px/1.45 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; text-align: right; overflow-wrap: anywhere; }
@keyframes spin { to { transform: rotate(360deg); } }
@media (prefers-reduced-motion: reduce) {
  .indicator.running::before { animation: none; }
  .progress-bar { transition: none; }
}
@media (max-width: 480px) {
  .hero { padding: 15px 14px 13px; }
  .notes summary { padding-inline: 14px; }
  .notes-body, .notes-unavailable { padding-inline: 14px; }
}
</style>
</head>
<body>
<main id="root" class="card"></main>
<script>
(() => {
  const META_KEY = "io.github.devnoname120/codexify/self-update";
  const DEBUG_META_KEY = "io.github.devnoname120/codexify/debug";
  const STATUS_TOOL = "self_update_status";
  const TIMEOUT_MS = 60_000;
  const REQUEST_TIMEOUT_MS = 5_000;
  const POLL_DELAY_MS = 1_000;
  const RETRY_DELAY_MS = 2_000;
  const WIDGET_STATE_VERSION = 1;
  const root = document.getElementById("root");
  const pending = new Map();
  let nextId = 1;
  let initialized = false;
  let payload = null;
  let status = null;
  let deadlineAt = null;
  let terminalStatus = null;
  let timedOut = false;
  let reconnecting = false;
  let polling = false;
  let pollTimer = null;
  let elapsedTimer = null;
  let resizeObserver = null;
  let initialTiming = null;
  let statusTiming = null;
  let persistedState = normalizeWidgetState(window.openai && window.openai.widgetState);

  function post(message) {
    window.parent.postMessage(message, "*");
  }

  function request(method, params, timeoutMs) {
    const id = nextId++;
    return new Promise((resolve, reject) => {
      const timer = timeoutMs ? setTimeout(() => {
        pending.delete(id);
        reject(new Error("request timed out"));
      }, timeoutMs) : null;
      pending.set(id, { resolve, reject, timer });
      post({ jsonrpc: "2.0", id, method, params });
    });
  }

  function notify(method, params) {
    post({ jsonrpc: "2.0", method, params });
  }

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = String(text);
    return node;
  }

  function normalizeWidgetState(value) {
    const source = value && typeof value === "object" && value.privateContent && typeof value.privateContent === "object"
      ? value.privateContent
      : value;
    const savedTerminal = source && source.terminalStatus && typeof source.terminalStatus === "object"
      ? source.terminalStatus
      : null;
    return {
      updateId: source && typeof source.updateId === "string" ? source.updateId : null,
      deadlineAt: source && Number.isFinite(source.deadlineAt) ? source.deadlineAt : null,
      terminalStatus: savedTerminal,
      timedOut: Boolean(source && source.timedOut)
    };
  }

  function persistWidgetState() {
    const api = window.openai;
    if (!api || typeof api.setWidgetState !== "function" || !payload) return;
    try {
      api.setWidgetState({
        privateContent: {
          version: WIDGET_STATE_VERSION,
          updateId: payload.updateId || null,
          deadlineAt,
          terminalStatus,
          timedOut
        }
      });
    } catch (_) {}
  }

  function payloadFromMetadata(value) {
    const queue = [value];
    const seen = new Set();
    const nestedKeys = ["_meta", "meta", "call_tool_result", "callToolResult", "mcp_tool_result", "mcpToolResult", "result"];
    while (queue.length) {
      const candidate = queue.shift();
      if (!candidate || typeof candidate !== "object" || seen.has(candidate)) continue;
      seen.add(candidate);
      const found = candidate[META_KEY];
      if (found && typeof found === "object") return found;
      for (const key of nestedKeys) {
        if (candidate[key] && typeof candidate[key] === "object") queue.push(candidate[key]);
      }
    }
    return null;
  }

  function debugFromMetadata(value) {
    const queue = [value];
    const seen = new Set();
    const nestedKeys = ["_meta", "meta", "call_tool_result", "callToolResult", "mcp_tool_result", "mcpToolResult", "result"];
    while (queue.length) {
      const candidate = queue.shift();
      if (!candidate || typeof candidate !== "object" || seen.has(candidate)) continue;
      seen.add(candidate);
      const found = candidate[DEBUG_META_KEY];
      if (found && typeof found === "object" && Number.isFinite(found.durationMs)) return found;
      for (const key of nestedKeys) {
        if (candidate[key] && typeof candidate[key] === "object") queue.push(candidate[key]);
      }
    }
    return null;
  }

  function legacyPayload(value) {
    if (!value || typeof value !== "object") return null;
    const candidate = value.structuredContent || value.structured_content || value;
    return candidate && typeof candidate.status === "string" && typeof candidate.currentVersion === "string"
      ? candidate
      : null;
  }

  function statusFromToolResult(value) {
    const queue = [value];
    const seen = new Set();
    const nestedKeys = ["structuredContent", "structured_content", "result", "call_tool_result", "callToolResult", "mcp_tool_result", "mcpToolResult"];
    while (queue.length) {
      const candidate = queue.shift();
      if (!candidate || typeof candidate !== "object" || seen.has(candidate)) continue;
      seen.add(candidate);
      if (
        typeof candidate.updateId === "string" &&
        typeof candidate.state === "string" &&
        typeof candidate.runningVersion === "string"
      ) return candidate;
      for (const key of nestedKeys) {
        if (candidate[key] && typeof candidate[key] === "object") queue.push(candidate[key]);
      }
    }
    return null;
  }

  function phaseProgress(value) {
    switch (value) {
      case "installing": return 38;
      case "validating": return 66;
      case "restarting": return 84;
      case "succeeded": return 100;
      case "failed":
      case "rolled_back": return 100;
      default: return 12;
    }
  }

  function phaseLabel(value) {
    switch (value) {
      case "installing": return "Installing the verified release...";
      case "validating": return "Validating the replacement executable...";
      case "restarting": return "Restarting the Codexify service...";
      case "succeeded": return "Codexify was updated successfully.";
      case "failed": return "The Codexify update failed.";
      case "rolled_back": return "The update was rolled back safely.";
      default: return "The update is scheduled...";
    }
  }

  function isTerminal(value) {
    return value === "succeeded" || value === "failed" || value === "rolled_back";
  }

  function effectiveState() {
    if (terminalStatus) return terminalStatus.state;
    if (status) return status.state;
    return payload && payload.status === "scheduled" ? "scheduled" : null;
  }

  function renderChangelog(container, changelog) {
    let list = null;
    for (const rawLine of String(changelog).split("\n")) {
      const line = rawLine.trimEnd();
      if (!line.trim()) {
        list = null;
        continue;
      }
      if (line.startsWith("### ")) {
        list = null;
        container.append(el("h3", "", line.slice(4)));
      } else if (line.startsWith("## ")) {
        list = null;
        container.append(el("h2", "", line.slice(3)));
      } else if (line.startsWith("- ")) {
        if (!list) {
          list = el("ul");
          container.append(list);
        }
        list.append(el("li", "", line.slice(2)));
      } else {
        list = null;
        container.append(el("p", "", line));
      }
    }
  }

  function render() {
    if (!payload) {
      root.replaceChildren(el("section", "hero", "Preparing update details..."));
      scheduleSizeReport();
      return;
    }

    const state = effectiveState();
    const waitingForUpdatedService = Boolean(
      state === "succeeded" &&
      payload.serviceRestart &&
      status &&
      status.runningVersion !== payload.targetVersion &&
      !terminalStatus
    );
    const displayState = waitingForUpdatedService ? "restarting" : state;
    const terminal = isTerminal(state) && !waitingForUpdatedService;
    const failure = state === "failed";
    const rolledBack = state === "rolled_back";
    const succeeded = state === "succeeded" && !waitingForUpdatedService;
    const upToDate = payload.status === "up_to_date";
    const aheadOfLatest = payload.status === "ahead_of_latest";
    const hero = el("section", "hero");
    const titleRow = el("div", "title-row");
    const indicatorClass = failure
      ? "indicator failure"
      : rolledBack || timedOut || aheadOfLatest
        ? "indicator warning"
        : succeeded || upToDate
          ? "indicator success"
          : "indicator running";
    titleRow.append(el("div", indicatorClass));
    const heading = el("div", "heading");
    heading.append(el("h1", "", payload.status === "scheduled" ? "Updating Codexify" : "Codexify update"));
    heading.append(el("div", "versions", `${payload.currentVersion} → ${payload.targetVersion}`));
    titleRow.append(heading);
    hero.append(titleRow);

    let label;
    let detail = "";
    if (payload.status === "up_to_date") {
      label = `Codexify ${payload.currentVersion} is already current.`;
    } else if (payload.status === "ahead_of_latest") {
      label = `Codexify ${payload.currentVersion} is newer than the published ${payload.targetVersion} release.`;
    } else if (timedOut) {
      label = "Update completion could not be verified.";
      detail = "Codexify did not report a terminal state within 60 seconds. This does not prove that the update failed.";
    } else if (reconnecting) {
      label = "Reconnecting to Codexify...";
      detail = "A brief connection interruption is expected while the background service restarts.";
    } else if (waitingForUpdatedService) {
      label = "The release is installed; waiting for the updated connector...";
      detail = `The responding process still reports ${status.runningVersion}.`;
    } else {
      label = phaseLabel(displayState);
      if ((failure || rolledBack) && terminalStatus && terminalStatus.failureDetail) {
        detail = terminalStatus.failureDetail;
      }
    }
    const statusLine = el("p", "status", label);
    statusLine.setAttribute("role", "status");
    statusLine.setAttribute("aria-live", "polite");
    statusLine.setAttribute("aria-atomic", "true");
    hero.append(statusLine);
    if (detail) hero.append(el("p", "detail", detail));

    if (payload.status === "scheduled") {
      const track = el("div", "progress-track");
      const bar = el("div", failure || rolledBack ? "progress-bar failure" : succeeded ? "progress-bar success" : "progress-bar");
      bar.style.width = `${phaseProgress(displayState)}%`;
      track.append(bar);
      hero.append(track);
      const phaseRow = el("div", "phase-row");
      phaseRow.append(el("span", "", displayState ? displayState.replace("_", " ") : "scheduled"));
      const startedAt = deadlineAt ? deadlineAt - TIMEOUT_MS : Date.now();
      const elapsed = Math.max(0, Math.floor((Date.now() - startedAt) / 1_000));
      phaseRow.append(el("span", "elapsed", `${elapsed}s elapsed`));
      hero.append(phaseRow);
    }

    if (timedOut) {
      const actions = el("div", "action-row");
      const retry = el("button", "retry", "Check again");
      retry.type = "button";
      retry.addEventListener("click", retryPolling);
      actions.append(retry);
      hero.append(actions);
    }

    if (succeeded) {
      const instruction = payload.serviceRestart
        ? "Open ChatGPT Settings, select the Codexify connector, scroll to the bottom of its tool list, and click Refresh so ChatGPT reloads the updated tools."
        : "Restart the foreground Codexify process, then open ChatGPT Settings and click Refresh for the Codexify connector.";
      hero.append(el("div", "instruction", instruction));
    }

    root.replaceChildren(hero);
    if (payload.changelog) {
      const notes = el("details", "notes");
      notes.open = true;
      notes.append(el("summary", "", "What changed"));
      const body = el("div", "notes-body");
      renderChangelog(body, payload.changelog);
      notes.append(body);
      notes.addEventListener("toggle", scheduleSizeReport);
      root.append(notes);
    } else if (payload.status === "scheduled") {
      const notes = el("section", "notes");
      notes.append(el("div", "notes-unavailable", "Release notes were unavailable in this release archive."));
      root.append(notes);
    }
    const timings = [];
    if (initialTiming) timings.push(`self_update: server ${initialTiming.durationMs} ms`);
    if (statusTiming) {
      timings.push(`self_update_status: server ${statusTiming.serverMs} ms`);
      timings.push(`round trip ${statusTiming.roundTripMs} ms`);
    }
    if (timings.length) root.append(el("div", "debug-timing", timings.join(" · ")));

    if (terminal || timedOut || payload.status !== "scheduled") stopElapsedTimer();
    else startElapsedTimer();
    scheduleSizeReport();
  }

  function stopElapsedTimer() {
    if (elapsedTimer) clearInterval(elapsedTimer);
    elapsedTimer = null;
  }

  function startElapsedTimer() {
    if (elapsedTimer) return;
    elapsedTimer = setInterval(render, 1_000);
  }

  function schedulePoll(delay) {
    if (pollTimer) clearTimeout(pollTimer);
    pollTimer = setTimeout(pollStatus, delay);
  }

  function markTimedOut() {
    timedOut = true;
    reconnecting = false;
    persistWidgetState();
    render();
  }

  async function pollStatus() {
    if (!initialized || !payload || !payload.updateId || terminalStatus || timedOut || polling) return;
    if (deadlineAt && Date.now() >= deadlineAt) {
      markTimedOut();
      return;
    }
    polling = true;
    const startedAt = performance.now();
    try {
      const result = await request("tools/call", {
        name: STATUS_TOOL,
        arguments: { updateId: payload.updateId }
      }, REQUEST_TIMEOUT_MS);
      const debug = debugFromMetadata(result);
      if (debug) {
        statusTiming = {
          serverMs: debug.durationMs,
          roundTripMs: Math.max(0, Math.round(performance.now() - startedAt))
        };
      }
      const nextStatus = statusFromToolResult(result);
      if (!nextStatus || nextStatus.updateId !== payload.updateId || nextStatus.targetVersion !== payload.targetVersion) {
        throw new Error("invalid update status response");
      }
      status = nextStatus;
      reconnecting = false;
      const waitingForUpdatedService = nextStatus.state === "succeeded" && payload.serviceRestart && nextStatus.runningVersion !== payload.targetVersion;
      if (isTerminal(nextStatus.state) && !waitingForUpdatedService) {
        terminalStatus = nextStatus;
        persistWidgetState();
        render();
      } else {
        render();
        schedulePoll(POLL_DELAY_MS);
      }
    } catch (_) {
      reconnecting = true;
      if (deadlineAt && Date.now() >= deadlineAt) markTimedOut();
      else {
        render();
        schedulePoll(RETRY_DELAY_MS);
      }
    } finally {
      polling = false;
    }
  }

  function retryPolling() {
    timedOut = false;
    reconnecting = false;
    terminalStatus = null;
    deadlineAt = Date.now() + TIMEOUT_MS;
    persistWidgetState();
    render();
    pollStatus();
  }

  function adoptPayload(nextPayload, metadata) {
    if (!nextPayload || typeof nextPayload !== "object") return;
    const debug = debugFromMetadata(metadata);
    if (debug) initialTiming = debug;
    const changedUpdate = !payload || payload.updateId !== nextPayload.updateId;
    payload = nextPayload;
    if (changedUpdate) {
      status = null;
      reconnecting = false;
      if (persistedState.updateId === payload.updateId) {
        deadlineAt = persistedState.deadlineAt;
        terminalStatus = persistedState.terminalStatus;
        timedOut = persistedState.timedOut;
      } else {
        deadlineAt = payload.status === "scheduled" ? Date.now() + TIMEOUT_MS : null;
        terminalStatus = null;
        timedOut = false;
      }
    }
    if (payload.status === "scheduled" && !deadlineAt) deadlineAt = Date.now() + TIMEOUT_MS;
    persistWidgetState();
    render();
    if (payload.status === "scheduled" && !terminalStatus && !timedOut) pollStatus();
  }

  function applyHostContext(context) {
    if (context && context.theme) document.documentElement.dataset.theme = context.theme;
  }

  function reportSize() {
    notify("ui/notifications/size-changed", {
      width: Math.ceil(document.documentElement.clientWidth || root.getBoundingClientRect().width),
      height: Math.ceil(document.documentElement.scrollHeight)
    });
  }

  function scheduleSizeReport() {
    requestAnimationFrame(reportSize);
  }

  function startSizeReporting() {
    if ("ResizeObserver" in window) {
      resizeObserver = new ResizeObserver(reportSize);
      resizeObserver.observe(document.documentElement);
    }
    reportSize();
  }

  window.addEventListener("message", event => {
    if (event.source !== window.parent) return;
    const message = event.data;
    if (!message || message.jsonrpc !== "2.0") return;
    const hasResult = Object.prototype.hasOwnProperty.call(message, "result");
    const hasError = Object.prototype.hasOwnProperty.call(message, "error");
    if (Object.prototype.hasOwnProperty.call(message, "id") && (hasResult || hasError)) {
      const waiter = pending.get(message.id);
      if (!waiter) return;
      pending.delete(message.id);
      if (waiter.timer) clearTimeout(waiter.timer);
      hasError ? waiter.reject(message.error) : waiter.resolve(message.result);
      return;
    }
    if (message.method === "ui/notifications/tool-result") {
      adoptPayload(payloadFromMetadata(message.params) || legacyPayload(message.params), message.params);
    } else if (message.method === "ui/notifications/host-context-changed") {
      applyHostContext(message.params);
    } else if (message.method === "ui/resource-teardown" && message.id !== undefined) {
      initialized = false;
      if (pollTimer) clearTimeout(pollTimer);
      stopElapsedTimer();
      post({ jsonrpc: "2.0", id: message.id, result: {} });
    }
  });

  const legacyMetadata = window.openai && window.openai.toolResponseMetadata;
  const legacy = window.openai && (
    payloadFromMetadata(legacyMetadata) || legacyPayload(window.openai.toolOutput)
  );
  if (legacy) adoptPayload(legacy, legacyMetadata);
  window.addEventListener("openai:set_globals", event => {
    const globals = event.detail && event.detail.globals;
    if (!globals) return;
    if (Object.prototype.hasOwnProperty.call(globals, "widgetState")) {
      persistedState = normalizeWidgetState(globals.widgetState);
    }
    const nextPayload = payloadFromMetadata(globals.toolResponseMetadata) || (!payload ? legacyPayload(globals.toolOutput) : null);
    if (nextPayload) adoptPayload(nextPayload, globals.toolResponseMetadata);
  });

  request("ui/initialize", {
    protocolVersion: "2026-01-26",
    appInfo: { name: "codexify-self-update", version: "1.0.0" },
    appCapabilities: {}
  }).then(result => {
    initialized = true;
    applyHostContext(result && result.hostContext);
    notify("ui/notifications/initialized", {});
    startSizeReporting();
    render();
    if (payload && payload.status === "scheduled" && !terminalStatus && !timedOut) pollStatus();
  }).catch(error => {
    if (!payload) root.replaceChildren(el("section", "hero", `Update UI could not initialize: ${error && error.message ? error.message : String(error)}`));
  });
})();
</script>
</body>
</html>
"####;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_update::{SelfUpdateReceipt, SelfUpdateStatus};
    use serde_json::{Value, json};

    fn receipt() -> SelfUpdateReceipt {
        SelfUpdateReceipt {
            status: SelfUpdateStatus::Scheduled,
            current_version: "1.0.0".to_string(),
            target_version: "2.0.0".to_string(),
            update_id: Some("0123456789abcdef01234567".to_string()),
            service_restart: true,
            log_path: "/tmp/codexify.log".to_string(),
            changelog: Some("## [2.0.0]\n\n- New behavior.\n".to_string()),
        }
    }

    #[test]
    fn metadata_links_the_user_confirmed_tool_to_a_borderless_app_resource() {
        let meta = tool_meta();
        assert_eq!(
            meta.get("ui")
                .and_then(|value| value.get("resourceUri"))
                .and_then(Value::as_str),
            Some(SELF_UPDATE_UI_URI)
        );
        assert_eq!(
            meta.get("ui").and_then(|value| value.get("visibility")),
            Some(&json!(["model", "app"]))
        );
        assert_eq!(
            meta.get("openai/widgetAccessible"),
            Some(&json!(true)),
            "the setup card invokes the update only from its explicit user action"
        );

        let resource = resource();
        assert_eq!(resource.uri, SELF_UPDATE_UI_URI);
        assert_eq!(
            resource.mime_type.as_deref(),
            Some(SELF_UPDATE_UI_MIME_TYPE)
        );
        assert_eq!(
            resource
                .meta
                .as_ref()
                .and_then(|meta| meta.get("ui"))
                .and_then(|value| value.get("prefersBorder"))
                .and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            resource
                .meta
                .as_ref()
                .and_then(|meta| meta.get("ui"))
                .and_then(|value| value.get("csp"))
                .and_then(|value| value.get("connectDomains")),
            Some(&json!([]))
        );
        assert_eq!(
            resource
                .meta
                .as_ref()
                .and_then(|meta| meta.get("ui"))
                .and_then(|value| value.get("csp"))
                .and_then(|value| value.get("resourceDomains")),
            Some(&json!([]))
        );
    }

    #[test]
    fn result_metadata_keeps_the_changelog_component_only() {
        let receipt = receipt();
        let meta = result_meta(&receipt);
        let payload = meta.get(SELF_UPDATE_RESULT_META_KEY).unwrap();
        assert_eq!(payload["updateId"], "0123456789abcdef01234567");
        assert_eq!(payload["changelog"], "## [2.0.0]\n\n- New behavior.\n");
        assert_eq!(payload["serviceRestart"], true);
    }

    #[test]
    fn embedded_app_polls_safely_and_persists_an_absolute_deadline() {
        assert!(SELF_UPDATE_UI_HTML.contains("ui/initialize"));
        assert!(SELF_UPDATE_UI_HTML.contains("ui/notifications/initialized"));
        assert!(SELF_UPDATE_UI_HTML.contains("ui/notifications/tool-result"));
        assert!(SELF_UPDATE_UI_HTML.contains("ui/notifications/size-changed"));
        assert!(SELF_UPDATE_UI_HTML.contains("tools/call"));
        assert!(SELF_UPDATE_UI_HTML.contains("self_update_status"));
        assert!(SELF_UPDATE_UI_HTML.contains("io.github.devnoname120/codexify/debug"));
        assert!(SELF_UPDATE_UI_HTML.contains("round trip ${statusTiming.roundTripMs} ms"));
        assert!(SELF_UPDATE_UI_HTML.contains("60_000"));
        assert!(SELF_UPDATE_UI_HTML.contains("setWidgetState"));
        assert!(SELF_UPDATE_UI_HTML.contains("Check again"));
        assert!(SELF_UPDATE_UI_HTML.contains("Refresh"));
        assert!(SELF_UPDATE_UI_HTML.contains("let initialized = false"));
        assert!(SELF_UPDATE_UI_HTML.contains("if (!initialized ||"));
        assert!(SELF_UPDATE_UI_HTML.contains("initialized = true"));
        assert!(SELF_UPDATE_UI_HTML.contains("waitingForUpdatedService"));
        assert!(
            SELF_UPDATE_UI_HTML
                .contains("const succeeded = state === \"succeeded\" && !waitingForUpdatedService")
        );
        assert!(SELF_UPDATE_UI_HTML.contains("payload.status === \"up_to_date\""));
        assert!(SELF_UPDATE_UI_HTML.contains("payload.status === \"ahead_of_latest\""));
        assert!(SELF_UPDATE_UI_HTML.contains("statusLine.setAttribute(\"role\", \"status\")"));
        assert!(!SELF_UPDATE_UI_HTML.contains("class=\"card\" aria-live="));
        assert!(SELF_UPDATE_UI_HTML.contains("textContent"));
        assert!(!SELF_UPDATE_UI_HTML.contains("innerHTML"));
        assert!(!SELF_UPDATE_UI_HTML.contains("fetch("));
    }

    #[test]
    fn current_and_only_resource_uri_is_readable() {
        assert!(contents_for_uri(SELF_UPDATE_UI_URI).is_some());
        assert!(contents_for_uri("ui://codexify/self-update/v0/mcp-app.html").is_none());
    }
}
