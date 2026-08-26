use rmcp::model::{MetaObject, Resource, ResourceContents};
use serde_json::json;

pub const REVIEW_UI_URI: &str = "ui://codexify/review/mcp-app.html";
pub const REVIEW_UI_MIME_TYPE: &str = "text/html;profile=mcp-app";
pub const MCP_APPS_EXTENSION_ID: &str = "io.modelcontextprotocol/ui";

pub fn tool_meta() -> MetaObject {
    serde_json::from_value(json!({
        "ui": { "resourceUri": REVIEW_UI_URI },
        "ui/resourceUri": REVIEW_UI_URI
    }))
    .expect("static review tool metadata must be an object")
}

pub fn resource_meta() -> MetaObject {
    serde_json::from_value(json!({
        "ui": { "prefersBorder": true }
    }))
    .expect("static review resource metadata must be an object")
}

pub fn resource() -> Resource {
    Resource::new(REVIEW_UI_URI, "codexify-review")
        .with_title("Code review")
        .with_description("Interactive rendering of Codexify review checkpoints")
        .with_mime_type(REVIEW_UI_MIME_TYPE)
        .with_size(REVIEW_UI_HTML.len() as u64)
        .with_meta(resource_meta())
}

pub fn contents() -> ResourceContents {
    ResourceContents::text(REVIEW_UI_HTML, REVIEW_UI_URI)
        .with_mime_type(REVIEW_UI_MIME_TYPE)
        .with_meta(resource_meta())
}

pub const REVIEW_UI_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Code review</title>
<style>
:root {
  color-scheme: light dark;
  --bg: var(--color-background-primary, light-dark(#ffffff, #171717));
  --panel: var(--color-background-secondary, light-dark(#f6f7f8, #222222));
  --panel-hover: light-dark(#eef0f2, #292929);
  --text: var(--color-text-primary, light-dark(#171717, #f4f4f4));
  --muted: var(--color-text-secondary, light-dark(#62666d, #a7a7a7));
  --border: var(--color-border-primary, light-dark(#d9dce1, #3a3a3a));
  --added-bg: light-dark(#e7f7ed, #163321);
  --added-text: light-dark(#145c2e, #8ee4aa);
  --deleted-bg: light-dark(#fceaea, #421d1d);
  --deleted-text: light-dark(#8a1f1f, #f2a0a0);
  --accent: light-dark(#2457c5, #8db4ff);
  --file-row-height: 28px;
  --diff-font-size: 9.5px;
  font-family: var(--font-sans, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif);
}
* { box-sizing: border-box; }
html, body { width: 100%; max-width: 100%; overflow-x: hidden; -webkit-text-size-adjust: 100%; text-size-adjust: 100%; }
body { margin: 0; background: var(--bg); color: var(--text); }
button, summary { color: inherit; font: inherit; }
main { display: grid; width: 100%; min-width: 0; gap: 8px; padding: 10px; }
header { display: flex; flex-wrap: wrap; align-items: flex-start; justify-content: space-between; gap: 6px 12px; }
h1 { margin: 0; font-size: 14px; line-height: 1.25; }
.subhead { margin-top: 2px; color: var(--muted); font-size: 10px; line-height: 1.35; overflow-wrap: anywhere; }
.badge { border: 1px solid var(--border); border-radius: 999px; padding: 3px 7px; color: var(--muted); font-size: 9px; line-height: 1.25; white-space: nowrap; }
.review { width: 100%; min-width: 0; max-width: 100%; border: 1px solid var(--border); border-radius: 10px; overflow: hidden; }
.review-summary, .file-summary { cursor: pointer; list-style: none; -webkit-tap-highlight-color: transparent; }
.review-summary::-webkit-details-marker, .file-summary::-webkit-details-marker { display: none; }
.review-summary { display: grid; grid-template-columns: minmax(0, 1fr) auto 9px; align-items: center; gap: 8px; min-height: 32px; padding: 6px 9px; background: var(--panel); font-size: 11px; font-weight: 650; }
.review-summary::after, .file-summary::after { content: ""; width: 7px; height: 7px; border-right: 1.5px solid var(--muted); border-bottom: 1.5px solid var(--muted); transform: rotate(-45deg); transition: transform 120ms ease; }
.review[open] > .review-summary::after, .file-entry[open] > .file-summary::after { transform: rotate(45deg); }
.review-summary:focus-visible, .file-summary:focus-visible, .show-more:focus-visible { outline: 2px solid var(--accent); outline-offset: -2px; }
.summary-stats { display: flex; align-items: baseline; gap: 6px; font-size: 10px; font-weight: 500; font-variant-numeric: tabular-nums; white-space: nowrap; }
.binary-count { color: var(--muted); }
.files { display: grid; width: 100%; min-width: 0; max-width: 100%; }
.file-entry { width: 100%; min-width: 0; max-width: 100%; overflow: hidden; border-top: 1px solid var(--border); }
.file-summary, .file-row { display: grid; width: 100%; min-width: 0; max-width: 100%; grid-template-columns: 12px minmax(0, 1fr) auto 8px; align-items: center; gap: 6px; min-height: var(--file-row-height); padding: 3px 9px; font-size: 10.5px; line-height: 1.25; }
.file-row { grid-template-columns: 12px minmax(0, 1fr) auto; border-top: 1px solid var(--border); }
.file-entry[open] > .file-summary, .file-summary:hover, .show-more:hover { background: var(--panel-hover); }
.status { width: 12px; color: var(--muted); font-family: var(--font-sans, ui-sans-serif, system-ui, sans-serif); font-size: 8.5px; font-weight: 650; line-height: 1; text-align: center; }
.path { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-family: var(--font-mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace); }
.stats { font-size: 9.5px; font-variant-numeric: tabular-nums; white-space: nowrap; }
.add { color: var(--added-text); }
.del { color: var(--deleted-text); margin-left: 5px; }
.show-more { width: 100%; min-height: var(--file-row-height); padding: 4px 9px; border: 0; border-top: 1px solid var(--border); background: transparent; color: var(--muted); font-size: 10px; line-height: 1.25; text-align: left; cursor: pointer; }
.empty, .notice { padding: 12px 9px; color: var(--muted); font-size: 10px; text-align: center; }
.omitted { border-top: 1px solid var(--border); }
.warning { border: 1px solid var(--border); border-radius: 9px; padding: 7px 9px; color: var(--muted); font-size: 10px; line-height: 1.35; }
.diff-body { width: 100%; min-width: 0; max-width: 100%; overflow: hidden; border-top: 1px solid var(--border); }
pre { width: 100%; min-width: 0; max-width: 100%; margin: 0; overflow-x: auto; overflow-y: hidden; overscroll-behavior-x: contain; -webkit-overflow-scrolling: touch; font-family: var(--font-mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace); font-size: var(--diff-font-size); font-variant-ligatures: none; line-height: 1.4; tab-size: 4; }
.line { display: block; width: max-content; min-width: 100%; padding: 0 8px; white-space: pre; }
.line.added { background: var(--added-bg); color: var(--added-text); }
.line.deleted { background: var(--deleted-bg); color: var(--deleted-text); }
.line.hunk { color: var(--accent); background: var(--panel); }
.line.meta { color: var(--muted); }
@media (max-width: 520px) {
  :root { --file-row-height: 26px; --diff-font-size: 9px; }
  main { gap: 6px; padding: 6px; }
  header { gap: 4px 8px; }
  h1 { font-size: 13px; }
  .subhead { font-size: 9.5px; }
  .badge { padding: 2px 6px; font-size: 8.5px; }
  .review-summary { min-height: 30px; padding: 5px 7px; }
  .file-summary, .file-row { padding: 2px 7px; font-size: 10px; }
  .show-more { padding: 3px 7px; }
  .line { padding: 0 6px; }
}
@media (prefers-reduced-motion: reduce) {
  .review-summary::after, .file-summary::after { transition: none; }
}
</style>
</head>
<body>
<main id="root" aria-live="polite">
  <div class="notice">Preparing review…</div>
</main>
<script>
(() => {
  "use strict";
  const root = document.getElementById("root");
  const INITIAL_VISIBLE_FILES = 3;
  let nextId = 1;
  const pending = new Map();
  let resizeObserver;

  function post(message) {
    window.parent.postMessage(message, "*");
  }

  function request(method, params) {
    const id = nextId++;
    post({ jsonrpc: "2.0", id, method, params });
    return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
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

  function splitPatch(patch) {
    const chunks = [];
    let current = null;
    for (const line of patch.split("\n")) {
      if (line.startsWith("diff --git ")) {
        current = { heading: line.slice(11), lines: [line] };
        chunks.push(current);
      } else {
        if (!current) {
          current = { heading: "Patch", lines: [] };
          chunks.push(current);
        }
        current.lines.push(line);
      }
    }
    return chunks;
  }

  function displayPath(file, chunk) {
    if (file && file.path) return file.previousPath ? `${file.previousPath} → ${file.path}` : file.path;
    return chunk && chunk.heading ? chunk.heading : "Patch";
  }

  function pathNode(file, chunk) {
    const text = displayPath(file, chunk);
    const node = el("span", "path", text);
    node.title = text;
    return node;
  }

  function statusCode(status) {
    return ({
      added: "A",
      modified: "M",
      deleted: "D",
      renamed: "R",
      copied: "C",
      type_changed: "T",
      unmerged: "U"
    })[status] || "M";
  }

  function statusNode(file) {
    const status = file && file.status ? file.status : "changed";
    const node = el("span", "status", statusCode(status));
    node.title = status.replaceAll("_", " ");
    return node;
  }

  function fileStats(file) {
    const stats = el("span", "stats");
    if (!file) return stats;
    if (file.binary) stats.append(el("span", "binary-count", "bin"));
    else stats.append(
      el("span", "add", `+${file.additions || 0}`),
      el("span", "del", `-${file.deletions || 0}`)
    );
    return stats;
  }

  function renderDiffLines(lines, container) {
    const pre = el("pre");
    for (const text of lines) {
      let className = "line";
      if (text.startsWith("+") && !text.startsWith("+++")) className += " added";
      else if (text.startsWith("-") && !text.startsWith("---")) className += " deleted";
      else if (text.startsWith("@@")) className += " hunk";
      else if (/^(diff --git|index |--- |\+\+\+ |new file|deleted file|similarity|rename )/.test(text)) className += " meta";
      pre.append(el("span", className, text + "\n"));
    }
    container.append(pre);
  }

  function scheduleSizeReport() {
    if (typeof window.requestAnimationFrame === "function") window.requestAnimationFrame(reportSize);
    else reportSize();
  }

  function diffDetails(file, chunk, unavailableReason) {
    const details = el("details", "file-entry");
    details.open = false;
    const summary = el("summary", "file-summary");
    summary.append(statusNode(file), pathNode(file, chunk), fileStats(file));
    details.append(summary);

    let rendered = false;
    details.addEventListener("toggle", () => {
      if (details.open && !rendered) {
        rendered = true;
        const body = el("div", "diff-body");
        if (chunk) renderDiffLines(chunk.lines, body);
        else body.append(el("div", "empty", unavailableReason || "No textual diff is available for this file."));
        details.append(body);
      }
      scheduleSizeReport();
    });
    return details;
  }

  function fileRow(file) {
    const row = el("div", "file-row");
    row.append(statusNode(file), pathNode(file), fileStats(file));
    return row;
  }

  function appendEntries(entries, container) {
    for (const entry of entries) container.append(entry);
    if (entries.length <= INITIAL_VISIBLE_FILES) return;

    let expanded = false;
    const button = el("button", "show-more");
    button.type = "button";
    const sync = () => {
      entries.forEach((entry, index) => {
        entry.hidden = !expanded && index >= INITIAL_VISIBLE_FILES;
      });
      button.textContent = expanded
        ? "Show fewer files"
        : `View ${entries.length - INITIAL_VISIBLE_FILES} more file${entries.length - INITIAL_VISIBLE_FILES === 1 ? "" : "s"}`;
      button.setAttribute("aria-expanded", String(expanded));
      scheduleSizeReport();
    };
    button.addEventListener("click", () => {
      expanded = !expanded;
      sync();
    });
    container.append(button);
    sync();
  }

  function renderFiles(data, container) {
    const files = Array.isArray(data.files) ? data.files : [];
    const patchAvailable = Boolean(data.patchIncluded && typeof data.patch === "string" && data.patch);
    const chunks = patchAvailable ? splitPatch(data.patch) : [];
    const count = patchAvailable ? Math.max(files.length, chunks.length) : files.length;
    const entries = [];

    for (let index = 0; index < count; index += 1) {
      entries.push(patchAvailable
        ? diffDetails(files[index], chunks[index], data.patchOmittedReason)
        : fileRow(files[index]));
    }
    appendEntries(entries, container);

    if (!entries.length) {
      const hasChanges = data.summary && data.summary.files;
      container.append(el("div", "empty", hasChanges
        ? "File metadata was omitted from this result."
        : "The scoped working tree matches the selected checkpoint."));
    }
    if (data.filesOmitted) {
      container.append(el("div", "empty omitted", `${data.filesOmitted} additional file${data.filesOmitted === 1 ? "" : "s"} omitted from structured metadata.`));
    }
  }

  function render(data) {
    if (!data || typeof data !== "object") return;
    root.replaceChildren();
    const summary = data.summary || {};
    const header = el("header");
    const heading = el("div");
    heading.append(
      el("h1", "", "Code review"),
      el("div", "subhead", `Since ${String(data.since || "last_review").replaceAll("_", " ")} · scope ${data.scope || "."}`)
    );
    const badgeText = data.advanceRequested
      ? (data.checkpointAdvanced ? "Checkpoint advanced" : "Checkpoint unchanged")
      : "Read-only review";
    header.append(heading, el("div", "badge", badgeText));
    root.append(header);

    const review = el("details", "review");
    review.open = true;
    review.addEventListener("toggle", scheduleSizeReport);
    const reviewSummary = el("summary", "review-summary");
    const count = summary.files || 0;
    const summaryStats = el("span", "summary-stats");
    summaryStats.append(
      el("span", "add", `+${summary.additions || 0}`),
      el("span", "del", `-${summary.deletions || 0}`)
    );
    if (summary.binaryFiles) {
      summaryStats.append(el("span", "binary-count", `${summary.binaryFiles} binary`));
    }
    reviewSummary.append(
      el("span", "", count ? `${count} file${count === 1 ? "" : "s"} changed` : "No files changed"),
      summaryStats
    );
    review.append(reviewSummary);
    const files = el("div", "files");
    renderFiles(data, files);
    review.append(files);
    root.append(review);

    const patchAvailable = Boolean(data.patchIncluded && typeof data.patch === "string" && data.patch);
    if (!patchAvailable && summary.files) {
      root.append(el("div", "warning", `Patch not shown: ${data.patchOmittedReason || "no textual patch was returned"}`));
    }
    for (const warning of data.warnings || []) root.append(el("div", "warning", warning));
    reportSize();
  }

  function toolResultPayload(params) {
    return params && (params.structuredContent || params.structured_content);
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
      hasError ? waiter.reject(message.error) : waiter.resolve(message.result);
      return;
    }
    if (message.method === "ui/notifications/tool-result") {
      render(toolResultPayload(message.params));
    } else if (message.method === "ui/notifications/host-context-changed") {
      applyHostContext(message.params);
    } else if (message.method === "ui/resource-teardown" && message.id !== undefined) {
      post({ jsonrpc: "2.0", id: message.id, result: {} });
    }
  });

  function applyHostContext(context) {
    if (context && context.theme) document.documentElement.dataset.theme = context.theme;
  }

  function reportSize() {
    notify("ui/notifications/size-changed", {
      width: Math.ceil(document.documentElement.clientWidth || root.getBoundingClientRect().width),
      height: Math.ceil(document.documentElement.scrollHeight)
    });
  }

  function startSizeReporting() {
    if ("ResizeObserver" in window) {
      resizeObserver = new ResizeObserver(reportSize);
      resizeObserver.observe(document.documentElement);
    }
    reportSize();
  }

  const legacy = window.openai && window.openai.toolOutput;
  if (legacy) render(legacy);
  window.addEventListener("openai:set_globals", event => {
    const output = event.detail && event.detail.globals && event.detail.globals.toolOutput;
    if (output) render(output);
  });

  request("ui/initialize", {
    protocolVersion: "2026-01-26",
    appInfo: { name: "codexify-review", version: "1.0.0" },
    appCapabilities: {}
  }).then(result => {
    applyHostContext(result && result.hostContext);
    notify("ui/notifications/initialized", {});
    startSizeReporting();
  }).catch(error => {
    if (!legacy) root.replaceChildren(el("div", "notice", `Review UI could not initialize: ${error && error.message ? error.message : String(error)}`));
  });
})();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_metadata_carries_current_and_compatibility_resource_keys() {
        let meta = tool_meta();
        assert_eq!(
            meta.get("ui")
                .and_then(|value| value.get("resourceUri"))
                .and_then(serde_json::Value::as_str),
            Some(REVIEW_UI_URI)
        );
        assert_eq!(
            meta.get("ui/resourceUri")
                .and_then(serde_json::Value::as_str),
            Some(REVIEW_UI_URI)
        );
    }

    #[test]
    fn resource_uses_the_mcp_apps_mime_type() {
        let resource = resource();
        assert_eq!(resource.uri, REVIEW_UI_URI);
        assert_eq!(resource.mime_type.as_deref(), Some(REVIEW_UI_MIME_TYPE));
        let contents = contents();
        let value = serde_json::to_value(contents).unwrap();
        assert_eq!(value["uri"], REVIEW_UI_URI);
        assert_eq!(value["mimeType"], REVIEW_UI_MIME_TYPE);
    }

    #[test]
    fn embedded_view_implements_the_standard_handshake_and_safe_rendering() {
        assert!(REVIEW_UI_HTML.contains("ui/initialize"));
        assert!(REVIEW_UI_HTML.contains("ui/notifications/initialized"));
        assert!(REVIEW_UI_HTML.contains("ui/notifications/tool-result"));
        assert!(REVIEW_UI_HTML.contains("ui/notifications/size-changed"));
        assert!(REVIEW_UI_HTML.contains("event.source !== window.parent"));
        assert!(REVIEW_UI_HTML.contains("hasOwnProperty.call(message, \"result\")"));
        assert!(REVIEW_UI_HTML.contains("textContent"));
        assert!(!REVIEW_UI_HTML.contains("<script src="));
        let initialized = REVIEW_UI_HTML
            .find("notify(\"ui/notifications/initialized\", {});")
            .unwrap();
        let size_reporting = REVIEW_UI_HTML.find("startSizeReporting();").unwrap();
        assert!(initialized < size_reporting);
    }

    #[test]
    fn embedded_view_is_compact_and_collapses_file_diffs_lazily() {
        assert!(REVIEW_UI_HTML.contains("--file-row-height: 28px"));
        assert!(REVIEW_UI_HTML.contains("--diff-font-size: 9.5px"));
        assert!(REVIEW_UI_HTML.contains("text-size-adjust: 100%"));
        assert!(REVIEW_UI_HTML.contains("const INITIAL_VISIBLE_FILES = 3"));
        assert!(REVIEW_UI_HTML.contains("el(\"details\", \"file-entry\")"));
        assert!(REVIEW_UI_HTML.contains("details.open = false"));
        assert!(REVIEW_UI_HTML.contains("details.addEventListener(\"toggle\""));
        assert!(REVIEW_UI_HTML.contains("renderDiffLines(chunk.lines, body)"));
        assert!(REVIEW_UI_HTML.contains("document.documentElement.clientWidth"));
        assert!(!REVIEW_UI_HTML.contains("document.documentElement.scrollWidth"));
        assert!(!REVIEW_UI_HTML.contains("font: 11px/1.55"));
    }
}
