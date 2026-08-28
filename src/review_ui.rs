use rmcp::model::{MetaObject, Resource, ResourceContents};
use serde_json::json;

use crate::review::ReviewResult;

pub const REVIEW_UI_URI: &str = "ui://codexify/review/v3/mcp-app.html";
pub const PREVIOUS_REVIEW_UI_URI: &str = "ui://codexify/review/v2/mcp-app.html";
pub const LEGACY_REVIEW_UI_URI: &str = "ui://codexify/review/mcp-app.html";
pub const REVIEW_UI_MIME_TYPE: &str = "text/html;profile=mcp-app";
pub const MCP_APPS_EXTENSION_ID: &str = "io.modelcontextprotocol/ui";
pub const REVIEW_RESULT_META_KEY: &str = "io.github.devnoname120/codexify/review";

pub fn tool_meta() -> MetaObject {
    serde_json::from_value(json!({
        "ui": { "resourceUri": REVIEW_UI_URI },
        "ui/resourceUri": REVIEW_UI_URI
    }))
    .expect("static review tool metadata must be an object")
}

pub fn resource_meta() -> MetaObject {
    serde_json::from_value(json!({
        "ui": { "prefersBorder": false }
    }))
    .expect("static review resource metadata must be an object")
}

pub fn result_meta(result: &ReviewResult) -> MetaObject {
    let mut meta = MetaObject::new();
    meta.0.insert(
        REVIEW_RESULT_META_KEY.to_string(),
        serde_json::to_value(result).expect("review result must serialize"),
    );
    meta
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
    contents_for_uri(REVIEW_UI_URI).expect("current review UI URI must be supported")
}

pub fn contents_for_uri(uri: &str) -> Option<ResourceContents> {
    if uri != REVIEW_UI_URI && uri != PREVIOUS_REVIEW_UI_URI && uri != LEGACY_REVIEW_UI_URI {
        return None;
    }
    Some(
        ResourceContents::text(REVIEW_UI_HTML, uri)
            .with_mime_type(REVIEW_UI_MIME_TYPE)
            .with_meta(resource_meta()),
    )
}

pub const REVIEW_UI_HTML: &str = concat!(
    r##"<!doctype html>
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
  --added-text: light-dark(#1a7f37, #3fb950);
  --deleted-text: light-dark(#cf222e, #f85149);
  --diff-line-number: light-dark(#57606a, #8c959f);
  --diff-addition-line: light-dark(#e6ffec, rgba(46, 160, 67, 0.15));
  --diff-addition-number: light-dark(#ccffd8, rgba(46, 160, 67, 0.30));
  --diff-addition-word: light-dark(#abf2bc, rgba(46, 160, 67, 0.40));
  --diff-deletion-line: light-dark(#ffebe9, rgba(248, 81, 73, 0.15));
  --diff-deletion-number: light-dark(#ffd7d5, rgba(248, 81, 73, 0.30));
  --diff-deletion-word: light-dark(#ffcecb, rgba(248, 81, 73, 0.40));
  --diff-hunk-line: light-dark(#ddf4ff, rgba(56, 139, 253, 0.15));
  --diff-hunk-number: light-dark(#b6e3ff, rgba(56, 139, 253, 0.40));
  --diff-hunk-text: light-dark(#0550ae, #58a6ff);
  --syntax-comment: light-dark(#6e7781, #8b949e);
  --syntax-keyword: light-dark(#cf222e, #ff7b72);
  --syntax-string: light-dark(#0a3069, #a5d6ff);
  --syntax-number: light-dark(#0550ae, #79c0ff);
  --syntax-function: light-dark(#8250df, #d2a8ff);
  --syntax-variable: light-dark(#953800, #ffa657);
  --syntax-type: light-dark(#116329, #7ee787);
  --accent: light-dark(#2457c5, #8db4ff);
  --file-row-height: 28px;
  --diff-font-size: 9.5px;
  font-family: var(--font-sans, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif);
}
:root[data-theme="light"] { color-scheme: light; }
:root[data-theme="dark"] { color-scheme: dark; }
* { box-sizing: border-box; }
html, body { width: 100%; max-width: 100%; overflow-x: hidden; background: transparent; -webkit-text-size-adjust: 100%; text-size-adjust: 100%; }
body { margin: 0; color: var(--text); }
button, summary { color: inherit; font: inherit; }
main { display: grid; width: 100%; min-width: 0; gap: 6px; padding: 6px; }
.review { width: 100%; min-width: 0; max-width: 100%; border: 1px solid var(--border); border-radius: 10px; overflow: hidden; background: var(--bg); }
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
.diff-table { display: grid; width: 100%; min-width: 0; max-width: 100%; grid-template-columns: minmax(3.5em, auto) minmax(3.5em, auto) minmax(0, 1fr); font-family: var(--font-mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace); font-size: var(--diff-font-size); font-variant-ligatures: none; line-height: 1.4; tab-size: 4; }
.diff-row { display: contents; }
.diff-cell { min-width: 0; min-height: 1.4em; padding-block: 1px; }
.line-number { padding-inline: 4px; border-right: 1px solid var(--border); color: var(--diff-line-number); font-variant-numeric: tabular-nums; text-align: right; user-select: none; white-space: nowrap; }
.code { padding-left: 0.55em; padding-right: 8px; white-space: pre-wrap; overflow-wrap: anywhere; word-break: break-word; }
.diff-row.added > .line-number { background: var(--diff-addition-number); color: var(--text); }
.diff-row.added > .code { background: var(--diff-addition-line); }
.diff-row.deleted > .line-number { background: var(--diff-deletion-number); color: var(--text); }
.diff-row.deleted > .code { background: var(--diff-deletion-line); }
.diff-row.hunk > .line-number { background: var(--diff-hunk-number); border-right-color: transparent; }
.diff-row.hunk > .code { background: var(--diff-hunk-line); color: var(--diff-hunk-text); }
.diff-row.hunk > .diff-cell { padding-block: 4px; }
.diff-row.meta > .line-number, .diff-row.meta > .code,
.diff-row.no-newline > .line-number, .diff-row.no-newline > .code { background: var(--panel); color: var(--muted); }
.diff-row.meta > .code, .diff-row.no-newline > .code { padding-block: 3px; }
.diff-row.no-newline > .code { font-style: italic; }
.word-change { border-radius: 2px; box-decoration-break: clone; -webkit-box-decoration-break: clone; }
.diff-row.added .word-change { background: var(--diff-addition-word); }
.diff-row.deleted .word-change { background: var(--diff-deletion-word); }
.syntax-comment, .syntax-prolog, .syntax-doctype, .syntax-cdata { color: var(--syntax-comment); }
.syntax-keyword, .syntax-atrule, .syntax-important { color: var(--syntax-keyword); }
.syntax-string, .syntax-char, .syntax-attr-value, .syntax-regex, .syntax-inserted { color: var(--syntax-string); }
.syntax-number, .syntax-boolean, .syntax-constant, .syntax-symbol, .syntax-property { color: var(--syntax-number); }
.syntax-function, .syntax-class-name { color: var(--syntax-function); }
.syntax-variable { color: var(--syntax-variable); }
.syntax-builtin, .syntax-tag, .syntax-selector, .syntax-attr-name { color: var(--syntax-type); }
.binary-diff { padding: 14px 10px; background: var(--panel); color: var(--muted); font-family: var(--font-mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace); font-size: var(--diff-font-size); line-height: 1.4; text-align: center; }
@media (max-width: 520px) {
  :root { --file-row-height: 26px; --diff-font-size: 9px; }
  main { gap: 4px; padding: 4px; }
  .review-summary { min-height: 30px; padding: 5px 7px; }
  .file-summary, .file-row { padding: 2px 7px; font-size: 10px; }
  .show-more { padding: 3px 7px; }
  .diff-table { grid-template-columns: minmax(3.25em, auto) minmax(3.25em, auto) minmax(0, 1fr); }
  .line-number { padding-inline: 3px; }
  .code { padding-left: 0.45em; padding-right: 6px; }
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
window.Prism = { manual: true };
"##,
    include_str!("review_prism.js"),
    r##"
(() => {
  "use strict";
  const root = document.getElementById("root");
  const INITIAL_VISIBLE_FILES = 3;
  const MAX_PAIRING_CELLS = 4096;
  const MAX_PAIRING_LINES = 256;
  const MAX_SIMILARITY_CHARS = 4096;
  const MAX_INTRALINE_CHARS = 8192;
  const MAX_INTRALINE_TOKENS = 180;
  const MAX_INTRALINE_CELLS = 20000;
  const MAX_INTRALINE_PAIRS = 512;
  const MAX_SYNTAX_CHARS = 256000;
  const MAX_SYNTAX_LINES = 4000;
  const LANGUAGE_BY_EXTENSION = Object.freeze({
    c: "c",
    h: "c",
    cc: "cpp",
    cp: "cpp",
    cpp: "cpp",
    cxx: "cpp",
    hh: "cpp",
    hpp: "cpp",
    hxx: "cpp",
    m: "objectivec",
    mm: "cpp",
    rs: "rust",
    json: "json",
    jsonc: "javascript",
    js: "javascript",
    mjs: "javascript",
    cjs: "javascript",
    jsx: "jsx",
    ts: "typescript",
    mts: "typescript",
    cts: "typescript",
    tsx: "tsx",
    py: "python",
    pyw: "python",
    sh: "bash",
    bash: "bash",
    zsh: "bash",
    fish: "bash",
    ps1: "powershell",
    psm1: "powershell",
    psd1: "powershell",
    yaml: "yaml",
    yml: "yaml",
    toml: "toml",
    md: "markdown",
    markdown: "markdown",
    html: "markup",
    htm: "markup",
    xml: "markup",
    svg: "markup",
    css: "css",
    scss: "scss",
    java: "java",
    kt: "kotlin",
    kts: "kotlin",
    go: "go",
    rb: "ruby",
    cs: "csharp",
    swift: "swift",
    lua: "lua",
    sql: "sql"
  });
  const REVIEW_META_KEY = "io.github.devnoname120/codexify/review";
  const WIDGET_STATE_VERSION = 1;
  let nextId = 1;
  const pending = new Map();
  let resizeObserver;
  let currentData;
  let uiState = normalizeWidgetState(window.openai && window.openai.widgetState);

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

  function normalizeWidgetState(value) {
    const source = value && typeof value === "object" && value.privateContent && typeof value.privateContent === "object"
      ? value.privateContent
      : value;
    const expandedFiles = source && Array.isArray(source.expandedFiles)
      ? source.expandedFiles.filter(item => typeof item === "string")
      : [];
    return {
      reviewOpen: !(source && source.reviewOpen === false),
      showAllFiles: Boolean(source && source.showAllFiles),
      expandedFiles: new Set(expandedFiles)
    };
  }

  function widgetStatesEqual(left, right) {
    if (left.reviewOpen !== right.reviewOpen || left.showAllFiles !== right.showAllFiles) return false;
    if (left.expandedFiles.size !== right.expandedFiles.size) return false;
    for (const key of left.expandedFiles) {
      if (!right.expandedFiles.has(key)) return false;
    }
    return true;
  }

  function persistWidgetState() {
    const api = window.openai;
    if (!api || typeof api.setWidgetState !== "function") return;
    try {
      api.setWidgetState({
        privateContent: {
          version: WIDGET_STATE_VERSION,
          reviewOpen: uiState.reviewOpen,
          showAllFiles: uiState.showAllFiles,
          expandedFiles: Array.from(uiState.expandedFiles)
        }
      });
    } catch (_) {}
  }

  function reviewPayloadFromMetadata(value) {
    const queue = [value];
    const seen = new Set();
    const nestedKeys = ["_meta", "meta", "call_tool_result", "callToolResult", "mcp_tool_result", "mcpToolResult", "result"];
    while (queue.length) {
      const candidate = queue.shift();
      if (!candidate || typeof candidate !== "object" || seen.has(candidate)) continue;
      seen.add(candidate);
      const payload = candidate[REVIEW_META_KEY];
      if (payload && typeof payload === "object") return payload;
      for (const key of nestedKeys) {
        if (candidate[key] && typeof candidate[key] === "object") queue.push(candidate[key]);
      }
    }
    return null;
  }

  function legacyStructuredPayload(value) {
    if (!value || typeof value !== "object") return null;
    const payload = value.structuredContent || value.structured_content;
    if (payload && typeof payload === "object") return payload;
    return value.summary && Array.isArray(value.files) ? value : null;
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

  function entryStateKey(file, chunk, index) {
    const previous = file && file.previousPath ? file.previousPath : "";
    const path = file && file.path ? file.path : (chunk && chunk.heading ? chunk.heading : "Patch");
    return `${index}:${previous}→${path}`;
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

  function parseHunkHeader(text) {
    const match = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(?: ?(.*))?$/.exec(text);
    if (!match) return null;
    return { oldLine: Number(match[1]), newLine: Number(match[3]) };
  }

  function isStructuralMetadata(text) {
    return /^(diff --git |index |--- |\+\+\+ )/.test(text);
  }

  function parseDiffRows(lines, file) {
    const rows = [];
    let oldLine = null;
    let newLine = null;
    let insideHunk = false;

    lines.forEach((rawText, index) => {
      if (rawText === "" && index === lines.length - 1) return;
      const hunk = parseHunkHeader(rawText);
      if (hunk) {
        oldLine = hunk.oldLine;
        newLine = hunk.newLine;
        insideHunk = true;
        rows.push({ kind: "hunk", oldLine: null, newLine: null, text: rawText });
        return;
      }

      if (!insideHunk) {
        if (!rawText || isStructuralMetadata(rawText)) return;
        rows.push({ kind: "meta", oldLine: null, newLine: null, text: rawText });
        return;
      }

      if (rawText.startsWith("\\ No newline at end of file")) {
        rows.push({ kind: "no-newline", oldLine: null, newLine: null, text: rawText });
        return;
      }

      const marker = rawText.charAt(0);
      const text = rawText.slice(1);
      if (marker === " ") {
        rows.push({ kind: "context", oldLine, newLine, text });
        oldLine += 1;
        newLine += 1;
      } else if (marker === "-") {
        rows.push({ kind: "deleted", oldLine, newLine: null, text });
        oldLine += 1;
      } else if (marker === "+") {
        rows.push({ kind: "added", oldLine: null, newLine, text });
        newLine += 1;
      } else if (rawText) {
        rows.push({ kind: "meta", oldLine: null, newLine: null, text: rawText });
      }
    });

    annotateSyntax(rows, file);
    annotateIntraline(rows);
    return rows;
  }

  function syntaxLanguageForFile(file) {
    if (!file || typeof file.path !== "string" || typeof Prism !== "object") return null;
    const basename = file.path.split(/[\\/]/).pop().toLowerCase();
    if (basename === "makefile" || basename.startsWith("makefile.")) return "makefile";
    if (basename === "dockerfile" || basename.startsWith("dockerfile.")) return "docker";
    if (basename === "cargo.lock") return "toml";
    if (basename === ".env" || basename.startsWith(".env.")) return "bash";
    const dot = basename.lastIndexOf(".");
    const extension = dot >= 0 ? basename.slice(dot + 1) : "";
    const language = LANGUAGE_BY_EXTENSION[extension];
    return language && Prism.languages[language] ? language : null;
  }

  function normalizedSyntaxClasses(type, alias, inherited) {
    const classes = inherited.slice();
    const append = value => {
      if (typeof value === "string" && /^[A-Za-z0-9_-]+$/.test(value) && !classes.includes(value)) {
        classes.push(value);
      }
    };
    append(type);
    if (Array.isArray(alias)) alias.forEach(append);
    else append(alias);
    return classes;
  }

  function pushSyntaxSegment(output, text, classes) {
    if (!text) return;
    const key = classes.join(" ");
    const previous = output[output.length - 1];
    if (previous && previous.key === key) previous.text += text;
    else output.push({ text, classes, key });
  }

  function flattenPrismTokens(value, inherited, output) {
    if (typeof value === "string") {
      pushSyntaxSegment(output, value, inherited);
      return;
    }
    if (Array.isArray(value)) {
      for (const child of value) flattenPrismTokens(child, inherited, output);
      return;
    }
    if (!value || typeof value !== "object") return;
    const classes = normalizedSyntaxClasses(value.type, value.alias, inherited);
    flattenPrismTokens(value.content, classes, output);
  }

  function splitSyntaxLines(segments) {
    const lines = [[]];
    for (const segment of segments) {
      let start = 0;
      while (start <= segment.text.length) {
        const newline = segment.text.indexOf("\n", start);
        const end = newline < 0 ? segment.text.length : newline;
        pushSyntaxSegment(lines[lines.length - 1], segment.text.slice(start, end), segment.classes);
        if (newline < 0) break;
        lines.push([]);
        start = newline + 1;
      }
    }
    return lines;
  }

  function tokenizeSyntaxRows(rows, language) {
    if (!rows.length || rows.length > MAX_SYNTAX_LINES) return;
    const source = rows.map(row => row.text).join("\n");
    if (source.length > MAX_SYNTAX_CHARS) return;
    try {
      const flattened = [];
      flattenPrismTokens(Prism.tokenize(source, Prism.languages[language]), [], flattened);
      const lines = splitSyntaxLines(flattened);
      if (lines.length !== rows.length) return;
      rows.forEach((row, index) => {
        row.syntaxSegments = lines[index];
      });
    } catch (_) {}
  }

  function annotateSyntax(rows, file) {
    const language = syntaxLanguageForFile(file);
    if (!language) return;
    let cursor = 0;
    while (cursor < rows.length) {
      if (rows[cursor].kind !== "hunk") {
        cursor += 1;
        continue;
      }
      let end = cursor + 1;
      while (end < rows.length && rows[end].kind !== "hunk") end += 1;
      const hunkRows = rows.slice(cursor + 1, end);
      tokenizeSyntaxRows(
        hunkRows.filter(row => row.kind === "context" || row.kind === "deleted"),
        language
      );
      tokenizeSyntaxRows(
        hunkRows.filter(row => row.kind === "context" || row.kind === "added"),
        language
      );
      cursor = end;
    }
  }

  function similarityTokens(text) {
    return text.match(/[A-Za-z0-9_$]+|[^A-Za-z0-9_$\s]/g) || [];
  }

  function tokenWeight(token) {
    return /^[A-Za-z0-9_$]+$/.test(token) ? Math.min(8, token.length) : 0.35;
  }

  function lineSimilarity(left, right) {
    if (left === right) return 1;
    if (!left.trim() && !right.trim()) {
      const longest = Math.max(left.length, right.length);
      return longest ? Math.min(left.length, right.length) / longest : 1;
    }

    if (left.length > MAX_SIMILARITY_CHARS || right.length > MAX_SIMILARITY_CHARS) {
      const longest = Math.max(left.length, right.length);
      const lengthRatio = longest ? Math.min(left.length, right.length) / longest : 1;
      const edgeLimit = Math.min(128, left.length, right.length);
      let prefix = 0;
      while (prefix < edgeLimit && left.charAt(prefix) === right.charAt(prefix)) prefix += 1;
      let suffix = 0;
      while (
        suffix < edgeLimit - prefix
        && left.charAt(left.length - suffix - 1) === right.charAt(right.length - suffix - 1)
      ) suffix += 1;
      const edgeSimilarity = Math.min(1, (prefix + suffix) / 64);
      return edgeSimilarity * 0.7 + lengthRatio * 0.3;
    }

    const leftTokens = similarityTokens(left);
    const rightTokens = similarityTokens(right);
    const counts = new Map();
    let leftWeight = 0;
    let rightWeight = 0;
    let overlapWeight = 0;

    for (const token of leftTokens) {
      leftWeight += tokenWeight(token);
      counts.set(token, (counts.get(token) || 0) + 1);
    }
    for (const token of rightTokens) {
      const weight = tokenWeight(token);
      rightWeight += weight;
      const remaining = counts.get(token) || 0;
      if (remaining > 0) {
        overlapWeight += weight;
        counts.set(token, remaining - 1);
      }
    }

    const overlap = leftWeight + rightWeight
      ? (2 * overlapWeight) / (leftWeight + rightWeight)
      : 0;
    const longest = Math.max(left.length, right.length);
    const lengthRatio = longest ? Math.min(left.length, right.length) / longest : 1;
    return overlap * 0.7 + lengthRatio * 0.3;
  }

  function alignChangedLines(deletedRows, addedRows) {
    const deletedCount = deletedRows.length;
    const addedCount = addedRows.length;
    if (!deletedCount || !addedCount) return [];

    if (
      deletedCount * addedCount > MAX_PAIRING_CELLS
      || deletedCount + addedCount > MAX_PAIRING_LINES
    ) {
      const pairs = [];
      let nextAdded = 0;
      for (const deleted of deletedRows) {
        let bestIndex = -1;
        let bestScore = 0.28;
        const searchEnd = Math.min(addedCount, nextAdded + 8);
        for (let index = nextAdded; index < searchEnd; index += 1) {
          const score = lineSimilarity(deleted.text, addedRows[index].text);
          if (score > bestScore) {
            bestScore = score;
            bestIndex = index;
          }
        }
        if (bestIndex >= 0) {
          pairs.push([deleted, addedRows[bestIndex]]);
          nextAdded = bestIndex + 1;
        }
      }
      return pairs;
    }

    const scores = Array.from({ length: deletedCount + 1 }, () => new Float32Array(addedCount + 1));
    const directions = Array.from({ length: deletedCount + 1 }, () => new Uint8Array(addedCount + 1));
    for (let deletedIndex = 1; deletedIndex <= deletedCount; deletedIndex += 1) {
      for (let addedIndex = 1; addedIndex <= addedCount; addedIndex += 1) {
        let best = scores[deletedIndex - 1][addedIndex];
        let direction = 1;
        if (scores[deletedIndex][addedIndex - 1] > best) {
          best = scores[deletedIndex][addedIndex - 1];
          direction = 2;
        }
        const similarity = lineSimilarity(
          deletedRows[deletedIndex - 1].text,
          addedRows[addedIndex - 1].text
        );
        const paired = similarity >= 0.28
          ? scores[deletedIndex - 1][addedIndex - 1] + similarity
          : -1;
        if (paired > best + 0.0001) {
          best = paired;
          direction = 3;
        }
        scores[deletedIndex][addedIndex] = best;
        directions[deletedIndex][addedIndex] = direction;
      }
    }

    const pairs = [];
    let deletedIndex = deletedCount;
    let addedIndex = addedCount;
    while (deletedIndex > 0 && addedIndex > 0) {
      const direction = directions[deletedIndex][addedIndex];
      if (direction === 3) {
        pairs.push([deletedRows[deletedIndex - 1], addedRows[addedIndex - 1]]);
        deletedIndex -= 1;
        addedIndex -= 1;
      } else if (direction === 1) deletedIndex -= 1;
      else addedIndex -= 1;
    }
    pairs.reverse();
    return pairs;
  }

  function intralineTokens(text) {
    return text.match(/\s+|[A-Za-z0-9_$]+|[^A-Za-z0-9_$\s]+/g) || [];
  }

  function edgeDiffSegments(before, after) {
    let prefix = 0;
    const prefixLimit = Math.min(before.length, after.length);
    while (prefix < prefixLimit && before.charAt(prefix) === after.charAt(prefix)) prefix += 1;

    let suffix = 0;
    const suffixLimit = Math.min(before.length - prefix, after.length - prefix);
    while (
      suffix < suffixLimit
      && before.charAt(before.length - suffix - 1) === after.charAt(after.length - suffix - 1)
    ) suffix += 1;

    const build = text => {
      const segments = [];
      if (prefix) segments.push({ text: text.slice(0, prefix), changed: false });
      const changedEnd = suffix ? text.length - suffix : text.length;
      if (changedEnd > prefix) segments.push({ text: text.slice(prefix, changedEnd), changed: true });
      if (suffix) segments.push({ text: text.slice(text.length - suffix), changed: false });
      return segments;
    };
    return { before: build(before), after: build(after) };
  }

  function mergeTokenSegments(tokens, changed) {
    const segments = [];
    tokens.forEach((text, index) => {
      const isChanged = Boolean(changed[index]);
      const previous = segments[segments.length - 1];
      if (previous && previous.changed === isChanged) previous.text += text;
      else segments.push({ text, changed: isChanged });
    });
    return segments;
  }

  function intralineSegments(before, after) {
    if (before === after) return null;
    if (before.length > MAX_INTRALINE_CHARS || after.length > MAX_INTRALINE_CHARS) {
      return edgeDiffSegments(before, after);
    }
    const beforeTokens = intralineTokens(before);
    const afterTokens = intralineTokens(after);
    if (
      beforeTokens.length > MAX_INTRALINE_TOKENS
      || afterTokens.length > MAX_INTRALINE_TOKENS
      || beforeTokens.length * afterTokens.length > MAX_INTRALINE_CELLS
    ) return edgeDiffSegments(before, after);

    const matrix = Array.from(
      { length: beforeTokens.length + 1 },
      () => new Uint16Array(afterTokens.length + 1)
    );
    for (let beforeIndex = 1; beforeIndex <= beforeTokens.length; beforeIndex += 1) {
      for (let afterIndex = 1; afterIndex <= afterTokens.length; afterIndex += 1) {
        matrix[beforeIndex][afterIndex] = beforeTokens[beforeIndex - 1] === afterTokens[afterIndex - 1]
          ? matrix[beforeIndex - 1][afterIndex - 1] + 1
          : Math.max(matrix[beforeIndex - 1][afterIndex], matrix[beforeIndex][afterIndex - 1]);
      }
    }

    const beforeChanged = new Array(beforeTokens.length).fill(true);
    const afterChanged = new Array(afterTokens.length).fill(true);
    let beforeIndex = beforeTokens.length;
    let afterIndex = afterTokens.length;
    while (beforeIndex > 0 && afterIndex > 0) {
      if (beforeTokens[beforeIndex - 1] === afterTokens[afterIndex - 1]) {
        beforeChanged[beforeIndex - 1] = false;
        afterChanged[afterIndex - 1] = false;
        beforeIndex -= 1;
        afterIndex -= 1;
      } else if (matrix[beforeIndex - 1][afterIndex] >= matrix[beforeIndex][afterIndex - 1]) {
        beforeIndex -= 1;
      } else afterIndex -= 1;
    }

    return {
      before: mergeTokenSegments(beforeTokens, beforeChanged),
      after: mergeTokenSegments(afterTokens, afterChanged)
    };
  }

  function annotateIntraline(rows) {
    let index = 0;
    let attemptedPairs = 0;
    while (index < rows.length) {
      if (rows[index].kind !== "deleted" && rows[index].kind !== "added") {
        index += 1;
        continue;
      }
      const changedRows = [];
      while (
        index < rows.length
        && (rows[index].kind === "deleted" || rows[index].kind === "added" || rows[index].kind === "no-newline")
      ) {
        if (rows[index].kind !== "no-newline") changedRows.push(rows[index]);
        index += 1;
      }
      const deletedRows = changedRows.filter(row => row.kind === "deleted");
      const addedRows = changedRows.filter(row => row.kind === "added");
      for (const [deleted, added] of alignChangedLines(deletedRows, addedRows)) {
        if (attemptedPairs >= MAX_INTRALINE_PAIRS) return;
        attemptedPairs += 1;
        const segments = intralineSegments(deleted.text, added.text);
        if (!segments) continue;
        deleted.segments = segments.before;
        added.segments = segments.after;
      }
    }
  }

  function segmentsCoverText(segments, text) {
    return segments.reduce((length, segment) => length + segment.text.length, 0) === text.length;
  }

  function appendCodeSegments(node, row) {
    let syntax = row.syntaxSegments && segmentsCoverText(row.syntaxSegments, row.text)
      ? row.syntaxSegments
      : [{ text: row.text, classes: [] }];
    let changes = row.segments && segmentsCoverText(row.segments, row.text)
      ? row.segments
      : [{ text: row.text, changed: false }];
    let syntaxIndex = 0;
    let changeIndex = 0;
    let syntaxOffset = 0;
    let changeOffset = 0;

    while (syntaxIndex < syntax.length && changeIndex < changes.length) {
      const syntaxSegment = syntax[syntaxIndex];
      const changeSegment = changes[changeIndex];
      const length = Math.min(
        syntaxSegment.text.length - syntaxOffset,
        changeSegment.text.length - changeOffset
      );
      const text = syntaxSegment.text.slice(syntaxOffset, syntaxOffset + length);
      const classes = (syntaxSegment.classes || []).map(name => `syntax-${name}`);
      if (changeSegment.changed) classes.push("word-change");
      if (classes.length) node.append(el("span", classes.join(" "), text));
      else node.append(document.createTextNode(text));

      syntaxOffset += length;
      changeOffset += length;
      if (syntaxOffset === syntaxSegment.text.length) {
        syntaxIndex += 1;
        syntaxOffset = 0;
      }
      if (changeOffset === changeSegment.text.length) {
        changeIndex += 1;
        changeOffset = 0;
      }
    }
  }

  function lineNumberCell(value, side) {
    const node = el("span", "diff-cell line-number", value == null ? "" : value);
    node.setAttribute("aria-hidden", "true");
    if (value != null) node.title = `${side} line ${value}`;
    return node;
  }

  function rowLabel(row) {
    if (row.kind === "added") return `Added line ${row.newLine}`;
    if (row.kind === "deleted") return `Deleted line ${row.oldLine}`;
    if (row.kind === "context") return `Old line ${row.oldLine}, new line ${row.newLine}`;
    if (row.kind === "hunk") return "Diff hunk header";
    return "Diff metadata";
  }

  function binaryDiffLabel(file) {
    if (!file) return "Binary file changed.";
    if (file.status === "added") return "Binary file added.";
    if (file.status === "deleted") return "Binary file deleted.";
    if (file.status === "renamed") return "Binary file renamed.";
    return "Binary file changed.";
  }

  function renderDiffLines(lines, container, file) {
    const binary = Boolean(file && file.binary)
      || lines.some(text => text === "GIT binary patch" || /^Binary files .* differ$/.test(text));
    if (binary) {
      container.append(el("div", "binary-diff", binaryDiffLabel(file)));
      return;
    }

    const rows = parseDiffRows(lines, file);
    if (!rows.length) {
      const message = file && file.status === "renamed"
        ? "File renamed without textual changes."
        : "No textual changes.";
      container.append(el("div", "empty", message));
      return;
    }

    const table = el("div", "diff-table");
    table.setAttribute("role", "table");
    for (const row of rows) {
      const rowNode = el("div", `diff-row ${row.kind}`);
      rowNode.setAttribute("role", "row");
      rowNode.setAttribute("aria-label", rowLabel(row));
      const code = el("span", "diff-cell code");
      appendCodeSegments(code, row);
      rowNode.append(
        lineNumberCell(row.oldLine, "Old"),
        lineNumberCell(row.newLine, "New"),
        code
      );
      table.append(rowNode);
    }
    container.append(table);
  }

  function scheduleSizeReport() {
    if (typeof window.requestAnimationFrame === "function") window.requestAnimationFrame(reportSize);
    else reportSize();
  }

  function diffDetails(file, chunk, unavailableReason, stateKey) {
    const details = el("details", "file-entry");
    const summary = el("summary", "file-summary");
    summary.append(statusNode(file), pathNode(file, chunk), fileStats(file));
    details.append(summary);

    let rendered = false;
    const ensureRendered = () => {
      if (rendered) return;
      rendered = true;
      const body = el("div", "diff-body");
      if (chunk) renderDiffLines(chunk.lines, body, file);
      else body.append(el("div", "empty", unavailableReason || "No textual diff is available for this file."));
      details.append(body);
    };
    details.open = uiState.expandedFiles.has(stateKey);
    if (details.open) ensureRendered();
    details.addEventListener("toggle", () => {
      const persistedOpen = uiState.expandedFiles.has(stateKey);
      if (details.open === persistedOpen) {
        scheduleSizeReport();
        return;
      }
      if (details.open) {
        ensureRendered();
        uiState.expandedFiles.add(stateKey);
      } else uiState.expandedFiles.delete(stateKey);
      persistWidgetState();
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

    let expanded = uiState.showAllFiles;
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
      uiState.showAllFiles = expanded;
      persistWidgetState();
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
        ? diffDetails(files[index], chunks[index], data.patchOmittedReason, entryStateKey(files[index], chunks[index], index))
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
      container.append(el("div", "empty omitted", `${data.filesOmitted} additional file${data.filesOmitted === 1 ? "" : "s"} omitted from widget file metadata.`));
    }
  }

  function render(data) {
    if (!data || typeof data !== "object") return;
    currentData = data;
    root.replaceChildren();
    const summary = data.summary || {};
    const review = el("details", "review");
    review.open = uiState.reviewOpen;
    review.addEventListener("toggle", () => {
      if (uiState.reviewOpen === review.open) {
        scheduleSizeReport();
        return;
      }
      uiState.reviewOpen = review.open;
      persistWidgetState();
      scheduleSizeReport();
    });
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
    return reviewPayloadFromMetadata(params) || legacyStructuredPayload(params);
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
      const output = toolResultPayload(message.params);
      if (output) render(output);
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

  const legacy = window.openai && (
    reviewPayloadFromMetadata(window.openai.toolResponseMetadata)
    || legacyStructuredPayload(window.openai.toolOutput)
  );
  if (legacy) render(legacy);
  window.addEventListener("openai:set_globals", event => {
    const globals = event.detail && event.detail.globals;
    if (!globals) return;
    let shouldRender = false;
    if (Object.prototype.hasOwnProperty.call(globals, "widgetState")) {
      const nextState = normalizeWidgetState(globals.widgetState);
      shouldRender = Boolean(currentData) && !widgetStatesEqual(uiState, nextState);
      uiState = nextState;
    }
    const output = reviewPayloadFromMetadata(globals.toolResponseMetadata)
      || (!currentData ? legacyStructuredPayload(globals.toolOutput) : null);
    if (output && !currentData) {
      currentData = output;
      shouldRender = true;
    }
    if (shouldRender) render(currentData);
  });

  request("ui/initialize", {
    protocolVersion: "2026-01-26",
    appInfo: { name: "codexify-review", version: "3.0.0" },
    appCapabilities: {}
  }).then(result => {
    applyHostContext(result && result.hostContext);
    notify("ui/notifications/initialized", {});
    startSizeReporting();
  }).catch(error => {
    if (!currentData) root.replaceChildren(el("div", "notice", `Review UI could not initialize: ${error && error.message ? error.message : String(error)}`));
  });
})();
</script>
</body>
</html>
"##
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::{ReviewBaseline, ReviewFile, ReviewSummary};

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
        assert_eq!(
            resource
                .meta
                .as_ref()
                .and_then(|meta| meta.get("ui"))
                .and_then(|value| value.get("prefersBorder"))
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
        let contents = contents();
        let value = serde_json::to_value(contents).unwrap();
        assert_eq!(value["uri"], REVIEW_UI_URI);
        assert_eq!(value["mimeType"], REVIEW_UI_MIME_TYPE);
        let previous =
            serde_json::to_value(contents_for_uri(PREVIOUS_REVIEW_UI_URI).unwrap()).unwrap();
        assert_eq!(previous["uri"], PREVIOUS_REVIEW_UI_URI);
        let legacy = serde_json::to_value(contents_for_uri(LEGACY_REVIEW_UI_URI).unwrap()).unwrap();
        assert_eq!(legacy["uri"], LEGACY_REVIEW_UI_URI);
        assert!(contents_for_uri("ui://codexify/review/unknown.html").is_none());
    }

    #[test]
    fn result_metadata_contains_the_complete_widget_payload() {
        let result = ReviewResult {
            since: ReviewBaseline::LastReview,
            advance_requested: true,
            checkpoint_advanced: true,
            scope: ".".to_string(),
            summary: ReviewSummary {
                files: 1,
                additions: 1,
                deletions: 0,
                binary_files: 0,
            },
            files: vec![ReviewFile {
                path: "src/lib.rs".to_string(),
                previous_path: None,
                status: "modified".to_string(),
                additions: Some(1),
                deletions: Some(0),
                binary: false,
            }],
            files_omitted: 0,
            patch: "diff --git a/src/lib.rs b/src/lib.rs\n+new\n".to_string(),
            patch_included: true,
            patch_bytes: Some(45),
            patch_omitted_reason: None,
            warnings: Vec::new(),
        };
        let meta = result_meta(&result);
        let payload = meta.get(REVIEW_RESULT_META_KEY).unwrap();
        assert_eq!(payload["patch"], result.patch);
        assert_eq!(payload["checkpointAdvanced"], true);
        assert_eq!(payload["files"][0]["path"], "src/lib.rs");
    }

    #[test]
    fn embedded_view_implements_the_standard_handshake_and_safe_rendering() {
        assert!(REVIEW_UI_HTML.contains("ui/initialize"));
        assert!(REVIEW_UI_HTML.contains("ui/notifications/initialized"));
        assert!(REVIEW_UI_HTML.contains("ui/notifications/tool-result"));
        assert!(REVIEW_UI_HTML.contains("ui/notifications/size-changed"));
        assert!(REVIEW_UI_HTML.contains("event.source !== window.parent"));
        assert!(REVIEW_UI_HTML.contains("hasOwnProperty.call(message, \"result\")"));
        assert!(REVIEW_UI_HTML.contains("window.openai.toolResponseMetadata"));
        assert!(REVIEW_UI_HTML.contains(REVIEW_RESULT_META_KEY));
        assert!(
            REVIEW_UI_HTML
                .contains("reviewPayloadFromMetadata(params) || legacyStructuredPayload(params)")
        );
        assert!(
            REVIEW_UI_HTML.contains("value.summary && Array.isArray(value.files) ? value : null")
        );
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
        assert!(REVIEW_UI_HTML.contains("details.open = uiState.expandedFiles.has(stateKey)"));
        assert!(REVIEW_UI_HTML.contains("details.addEventListener(\"toggle\""));
        assert!(REVIEW_UI_HTML.contains("renderDiffLines(chunk.lines, body, file)"));
        assert!(REVIEW_UI_HTML.contains("document.documentElement.clientWidth"));
        assert!(!REVIEW_UI_HTML.contains("document.documentElement.scrollWidth"));
        assert!(!REVIEW_UI_HTML.contains("font: 11px/1.55"));
    }

    #[test]
    fn embedded_view_renders_github_style_wrapped_diff_rows() {
        assert!(REVIEW_UI_HTML.contains("--diff-addition-line: light-dark(#e6ffec"));
        assert!(REVIEW_UI_HTML.contains("--diff-addition-number: light-dark(#ccffd8"));
        assert!(REVIEW_UI_HTML.contains("--diff-addition-word: light-dark(#abf2bc"));
        assert!(REVIEW_UI_HTML.contains("--diff-deletion-line: light-dark(#ffebe9"));
        assert!(REVIEW_UI_HTML.contains("--diff-deletion-number: light-dark(#ffd7d5"));
        assert!(REVIEW_UI_HTML.contains("--diff-deletion-word: light-dark(#ffcecb"));
        assert!(REVIEW_UI_HTML.contains("--diff-hunk-line: light-dark(#ddf4ff"));
        assert!(REVIEW_UI_HTML.contains("--diff-hunk-number: light-dark(#b6e3ff"));
        assert!(REVIEW_UI_HTML.contains(":root[data-theme=\"dark\"] { color-scheme: dark; }"));
        assert!(REVIEW_UI_HTML.contains("grid-template-columns: minmax(3.5em, auto)"));
        assert!(REVIEW_UI_HTML.contains("white-space: pre-wrap"));
        assert!(REVIEW_UI_HTML.contains("overflow-wrap: anywhere"));
        assert!(REVIEW_UI_HTML.contains("background: transparent"));
        assert!(REVIEW_UI_HTML.contains("background: var(--bg);"));
        assert!(!REVIEW_UI_HTML.contains("overflow-x: auto"));
        assert!(REVIEW_UI_HTML.contains("PrismJS 1.30.0"));
        assert!(REVIEW_UI_HTML.contains("Prism.tokenize(source, Prism.languages[language])"));
        assert!(REVIEW_UI_HTML.contains("function syntaxLanguageForFile(file)"));
        assert!(REVIEW_UI_HTML.contains("row.syntaxSegments = lines[index]"));
        assert!(REVIEW_UI_HTML.contains("function parseHunkHeader(text)"));
        assert!(REVIEW_UI_HTML.contains("kind: \"deleted\", oldLine"));
        assert!(REVIEW_UI_HTML.contains("kind: \"added\", oldLine: null, newLine"));
        assert!(REVIEW_UI_HTML.contains("function alignChangedLines(deletedRows, addedRows)"));
        assert!(REVIEW_UI_HTML.contains("function intralineSegments(before, after)"));
        assert!(REVIEW_UI_HTML.contains("const MAX_PAIRING_LINES = 256"));
        assert!(REVIEW_UI_HTML.contains("const MAX_INTRALINE_PAIRS = 512"));
        assert!(REVIEW_UI_HTML.contains("if (attemptedPairs >= MAX_INTRALINE_PAIRS) return"));
        assert!(REVIEW_UI_HTML.contains("classes.push(\"word-change\")"));
        assert!(REVIEW_UI_HTML.contains("lineNumberCell(row.oldLine, \"Old\")"));
        assert!(REVIEW_UI_HTML.contains("lineNumberCell(row.newLine, \"New\")"));
        assert!(!REVIEW_UI_HTML.contains("diff-cell marker"));
        assert!(REVIEW_UI_HTML.contains("color: var(--text); }"));
        assert!(!REVIEW_UI_HTML.contains("const header = el(\"header\")"));
    }

    #[test]
    fn embedded_view_persists_private_interaction_state() {
        assert!(REVIEW_UI_HTML.contains("window.openai.widgetState"));
        assert!(REVIEW_UI_HTML.contains("api.setWidgetState"));
        assert!(REVIEW_UI_HTML.contains("privateContent"));
        assert!(REVIEW_UI_HTML.contains("uiState.reviewOpen = review.open"));
        assert!(REVIEW_UI_HTML.contains("uiState.showAllFiles = expanded"));
        assert!(REVIEW_UI_HTML.contains("uiState.expandedFiles.add(stateKey)"));
        assert!(REVIEW_UI_HTML.contains("hasOwnProperty.call(globals, \"widgetState\")"));
        assert!(REVIEW_UI_HTML.contains("output && !currentData"));
    }
}
