use rmcp::model::{MetaObject, Resource, ResourceContents};
use serde_json::json;

pub const SETUP_UI_URI: &str = "ui://codexify/setup/v1/mcp-app.html";
pub const SETUP_UI_MIME_TYPE: &str = "text/html;profile=mcp-app";

pub fn connector_settings_url(connector_id: &str) -> Option<String> {
    let connector_id = connector_id.trim();
    let (plugin_id, suffix) = if let Some(suffix) = connector_id.strip_prefix("plugin_") {
        (connector_id.to_string(), suffix)
    } else {
        let suffix = connector_id.strip_prefix("asdk_app_")?;
        (format!("plugin_{connector_id}"), suffix)
    };
    if suffix.is_empty()
        || !plugin_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    Some(format!("https://chatgpt.com/#settings/Plugins/{plugin_id}"))
}

pub fn tool_meta() -> MetaObject {
    serde_json::from_value(json!({
        "ui": {
            "resourceUri": SETUP_UI_URI,
            "visibility": ["model", "app"]
        },
        "ui/resourceUri": SETUP_UI_URI,
        "openai/outputTemplate": SETUP_UI_URI,
        "openai/widgetAccessible": true
    }))
    .expect("static setup tool metadata must be an object")
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
        "openai/widgetCSP": {
            "connect_domains": [],
            "resource_domains": [],
            "redirect_domains": ["https://chatgpt.com"]
        }
    }))
    .expect("static setup resource metadata must be an object")
}

pub fn resource() -> Resource {
    Resource::new(SETUP_UI_URI, "codexify-setup")
        .with_title("Codexify status")
        .with_description("Codexify setup, update, connector-schema, and diagnostic status")
        .with_mime_type(SETUP_UI_MIME_TYPE)
        .with_size(SETUP_UI_HTML.len() as u64)
        .with_meta(resource_meta())
}

pub fn contents_for_uri(uri: &str) -> Option<ResourceContents> {
    if uri != SETUP_UI_URI {
        return None;
    }
    Some(
        ResourceContents::text(SETUP_UI_HTML, uri)
            .with_mime_type(SETUP_UI_MIME_TYPE)
            .with_meta(resource_meta()),
    )
}

pub const SETUP_UI_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Codexify status</title>
<style>
:root {
  color-scheme: light dark;
  --bg: var(--color-background-primary, light-dark(#ffffff, #171717));
  --panel: var(--color-background-secondary, light-dark(#f5f6f7, #222222));
  --panel-strong: light-dark(#eceef0, #292929);
  --text: var(--color-text-primary, light-dark(#171717, #f4f4f4));
  --muted: var(--color-text-secondary, light-dark(#62666d, #a7a7a7));
  --border: var(--color-border-primary, light-dark(#d9dce1, #3a3a3a));
  --accent: light-dark(#111111, #f5f5f5);
  --accent-text: light-dark(#ffffff, #171717);
  --good: light-dark(#18794e, #4ac28a);
  --warn: light-dark(#8a5a00, #e8b44f);
  --bad: light-dark(#b42318, #ff7b72);
  --focus: light-dark(#4c8bf5, #70a5ff);
}
* { box-sizing: border-box; }
html, body { margin: 0; padding: 0; background: transparent; color: var(--text); }
body {
  font-family: var(--font-sans, ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif);
  font-size: 14px;
  line-height: 1.45;
}
button, a { font: inherit; }
#root { width: 100%; }
.card {
  overflow: hidden;
  border: 1px solid var(--border);
  border-radius: 14px;
  background: var(--bg);
  box-shadow: 0 1px 2px light-dark(rgba(0,0,0,.04), rgba(0,0,0,.20));
}
.header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 15px 16px 13px;
  border-bottom: 1px solid var(--border);
}
.identity { min-width: 0; }
.title { margin: 0; font-size: 15px; font-weight: 650; letter-spacing: -.01em; }
.subtitle { margin-top: 2px; color: var(--muted); font-size: 12px; }
.version {
  flex: none;
  padding: 4px 8px;
  border: 1px solid var(--border);
  border-radius: 999px;
  background: var(--panel);
  color: var(--muted);
  font: 600 11px/1.2 var(--font-mono, ui-monospace, SFMono-Regular, Menlo, monospace);
}
.body { display: grid; gap: 12px; padding: 14px 16px 16px; }
.status-grid { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 9px; }
.status {
  min-width: 0;
  padding: 10px 11px;
  border: 1px solid var(--border);
  border-radius: 10px;
  background: var(--panel);
}
.status-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.status-label { color: var(--muted); font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: .04em; }
.status-value { margin-top: 5px; font-size: 13px; font-weight: 600; overflow-wrap: anywhere; }
.dot { width: 7px; height: 7px; border-radius: 50%; background: var(--muted); flex: none; }
.dot.good { background: var(--good); }
.dot.warn { background: var(--warn); }
.dot.bad { background: var(--bad); }
.notice {
  padding: 11px 12px;
  border: 1px solid color-mix(in srgb, var(--warn) 42%, var(--border));
  border-radius: 10px;
  background: color-mix(in srgb, var(--warn) 8%, var(--bg));
}
.notice strong { display: block; margin-bottom: 3px; font-size: 13px; }
.notice p { margin: 0; color: var(--muted); font-size: 12px; }
.actions { display: flex; flex-wrap: wrap; gap: 8px; }
.button {
  display: inline-flex;
  min-height: 34px;
  align-items: center;
  justify-content: center;
  gap: 7px;
  padding: 7px 11px;
  border: 1px solid var(--border);
  border-radius: 9px;
  background: var(--panel);
  color: var(--text);
  cursor: pointer;
  font-size: 12px;
  font-weight: 600;
  text-decoration: none;
}
.button:hover:not(:disabled) { background: var(--panel-strong); }
.button:focus-visible { outline: 2px solid var(--focus); outline-offset: 2px; }
.button.primary { border-color: var(--accent); background: var(--accent); color: var(--accent-text); }
.button.primary:hover:not(:disabled) { opacity: .88; background: var(--accent); }
.button:disabled { cursor: wait; opacity: .55; }
.output {
  display: none;
  padding: 10px 11px;
  border: 1px solid var(--border);
  border-radius: 10px;
  background: var(--panel);
  color: var(--muted);
  font-size: 12px;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
.output.visible { display: block; }
.doctor {
  display: none;
  max-height: 340px;
  margin: 0;
  padding: 11px 12px;
  overflow: auto;
  border: 1px solid var(--border);
  border-radius: 10px;
  background: var(--panel);
  color: var(--text);
  font: 11px/1.55 var(--font-mono, ui-monospace, SFMono-Regular, Menlo, monospace);
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
.doctor.visible { display: block; }
.next { color: var(--muted); font-size: 12px; }
.debug {
  display: none;
  padding-top: 9px;
  border-top: 1px solid var(--border);
  color: var(--muted);
  font: 10px/1.45 var(--font-mono, ui-monospace, SFMono-Regular, Menlo, monospace);
  overflow-wrap: anywhere;
}
.debug.visible { display: block; }
@media (max-width: 480px) {
  .status-grid { grid-template-columns: 1fr; }
  .button { flex: 1 1 auto; }
}
</style>
</head>
<body>
<div id="root" aria-live="polite"></div>
<script>
(() => {
  "use strict";
  const DEBUG_META_KEY = "io.github.devnoname120/codexify/debug";
  const root = document.getElementById("root");
  const pending = new Map();
  let nextId = 1;
  let resizeObserver;
  let currentData = null;
  let currentMetadata = null;
  let currentHostContext = null;
  let debugEntries = [];
  let initialized = false;

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

  function objectFrom(value, selector) {
    const queue = [value];
    const seen = new Set();
    const nestedKeys = [
      "_meta", "meta", "result", "call_tool_result", "callToolResult",
      "mcp_tool_result", "mcpToolResult", "toolOutput", "toolResponseMetadata",
      "invoked_resource", "invokedResource"
    ];
    while (queue.length) {
      const candidate = queue.shift();
      if (!candidate || typeof candidate !== "object" || seen.has(candidate)) continue;
      seen.add(candidate);
      const selected = selector(candidate);
      if (selected) return selected;
      for (const key of nestedKeys) {
        if (candidate[key] && typeof candidate[key] === "object") queue.push(candidate[key]);
      }
    }
    return null;
  }

  function setupPayload(value) {
    return objectFrom(value, candidate => {
      const payload = candidate.structuredContent || candidate.structured_content;
      if (payload && payload.update && payload.connectorSchema) return payload;
      if (candidate.update && candidate.connectorSchema) return candidate;
      return null;
    });
  }

  function debugPayload(value) {
    return objectFrom(value, candidate => {
      const payload = candidate[DEBUG_META_KEY];
      return payload && typeof payload === "object" ? payload : null;
    });
  }

  function normalizePluginSettingsId(value) {
    if (typeof value !== "string") return null;
    const trimmed = value.trim();
    if (/^plugin_[A-Za-z0-9_-]+$/.test(trimmed)) return trimmed;
    if (/^asdk_app_[A-Za-z0-9_-]+$/.test(trimmed)) return `plugin_${trimmed}`;
    return null;
  }

  function connectorPluginId(value) {
    return objectFrom(value, candidate => {
      for (const key of ["connector_id", "connectorId", "plugin_id", "pluginId"]) {
        const normalized = normalizePluginSettingsId(candidate[key]);
        if (normalized) return normalized;
      }
      for (const key of ["uri", "resourceUri"]) {
        if (typeof candidate[key] !== "string") continue;
        const match = candidate[key].match(/(?:^|\/)(asdk_app_[A-Za-z0-9_-]+)(?:\/|$)/);
        const normalized = normalizePluginSettingsId(match && match[1]);
        if (normalized) return normalized;
      }
      return null;
    });
  }

  function connectorSettingsUrl(schema, metadata) {
    if (schema && typeof schema.settingsUrl === "string" && schema.settingsUrl) {
      return schema.settingsUrl;
    }
    const pluginId = connectorPluginId(metadata)
      || connectorPluginId(currentHostContext)
      || connectorPluginId(window.openai);
    return pluginId
      ? `https://chatgpt.com/#settings/Plugins/${encodeURIComponent(pluginId)}`
      : null;
  }

  function toolText(value) {
    return objectFrom(value, candidate => {
      if (!Array.isArray(candidate.content)) return null;
      const text = candidate.content
        .map(item => item && typeof item.text === "string" ? item.text : "")
        .filter(Boolean)
        .join("\n");
      return text || null;
    }) || "The tool returned no text output.";
  }

  function statusCard(label, value, tone) {
    const card = el("div", "status");
    const head = el("div", "status-head");
    head.append(el("span", "status-label", label), el("span", `dot ${tone || ""}`));
    card.append(head, el("div", "status-value", value));
    return card;
  }

  function statusPresentation(data) {
    const update = data.update || {};
    const schema = data.connectorSchema || {};
    const settingsUrl = connectorSettingsUrl(schema, currentMetadata);
    const updateView = {
      update_available: [`${update.latestVersion || "New version"} available`, "warn"],
      up_to_date: ["Up to date", "good"],
      ahead_of_latest: ["Ahead of latest release", "good"],
      check_failed: ["Check unavailable", "warn"]
    }[update.status] || ["Unknown", "bad"];
    const schemaView = {
      current: ["Current", "good"],
      stale: ["Refresh required", "warn"],
      unknown: ["Refresh recommended", "warn"]
    }[schema.status] || ["Unknown", "bad"];
    return { updateView, schemaView, settingsUrl };
  }

  function addDebugEntry(entry) {
    if (!entry || debugEntries.includes(entry)) return;
    debugEntries.push(entry);
  }

  function captureTiming(name, result, startedAt) {
    if (!currentData || !currentData.debug) return;
    const metadata = debugPayload(result);
    if (metadata && Number.isFinite(metadata.durationMs)) {
      addDebugEntry(`${name}: server ${metadata.durationMs} ms`);
    }
    const roundTrip = Math.max(0, Math.round(performance.now() - startedAt));
    addDebugEntry(`${name}: widget round trip ${roundTrip} ms`);
  }

  async function callTool(name, args) {
    if (initialized) {
      try {
        return await request("tools/call", { name, arguments: args || {} });
      } catch (error) {
        if (!(window.openai && typeof window.openai.callTool === "function")) throw error;
      }
    }
    if (window.openai && typeof window.openai.callTool === "function") {
      return window.openai.callTool(name, args || {});
    }
    throw new Error("This host does not expose widget tool calls.");
  }

  async function openSettings(url) {
    if (window.openai && typeof window.openai.openExternal === "function") {
      await window.openai.openExternal({ href: url, redirectUrl: false });
      return;
    }
    window.open(url, "_blank", "noopener,noreferrer");
  }

  function reportSize() {
    notify("ui/notifications/size-changed", {
      width: Math.ceil(document.documentElement.clientWidth || root.getBoundingClientRect().width),
      height: Math.ceil(document.documentElement.scrollHeight)
    });
  }

  function applyHostContext(context) {
    if (!context || typeof context !== "object") return;
    currentHostContext = context;
    if (context.theme) document.documentElement.dataset.theme = context.theme;
  }

  function render(data, metadata) {
    if (!data || typeof data !== "object") return;
    currentData = data;
    currentMetadata = metadata || currentMetadata;
    debugEntries = [];

    if (data.debug && Number.isFinite(data.debug.updateCheckMs)) {
      addDebugEntry(`setup release check: ${data.debug.updateCheckMs} ms`);
    }
    const initialTiming = debugPayload(currentMetadata);
    if (initialTiming && Number.isFinite(initialTiming.durationMs)) {
      addDebugEntry(`setup: server ${initialTiming.durationMs} ms`);
    }

    root.replaceChildren();
    const card = el("section", "card");
    const header = el("header", "header");
    const identity = el("div", "identity");
    identity.append(el("h2", "title", "Codexify"), el("div", "subtitle", "Connector status and diagnostics"));
    header.append(identity, el("span", "version", `v${data.serverVersion || "unknown"}`));

    const body = el("div", "body");
    const views = statusPresentation(data);
    const grid = el("div", "status-grid");
    grid.append(
      statusCard("Codexify", views.updateView[0], views.updateView[1]),
      statusCard("Connector schema", views.schemaView[0], views.schemaView[1])
    );
    body.append(grid);

    const schema = data.connectorSchema || {};
    if (schema.refreshRecommended) {
      const notice = el("div", "notice");
      notice.append(
        el("strong", "", "Refresh the connector tools"),
        el("p", "", "Open ChatGPT Settings, select the Codexify connector, scroll to the bottom of its tool list, and click Refresh.")
      );
      body.append(notice);
    }

    const actions = el("div", "actions");
    const output = el("div", "output");
    const doctorOutput = el("pre", "doctor");

    if (data.update && data.update.status === "update_available") {
      const updateButton = el("button", "button primary", `Update to ${data.update.latestVersion || "latest"}`);
      updateButton.type = "button";
      updateButton.addEventListener("click", async () => {
        updateButton.disabled = true;
        output.classList.add("visible");
        output.textContent = "Preparing verified update…";
        const startedAt = performance.now();
        try {
          const result = await callTool("self_update", { confirm: true });
          captureTiming("self_update", result, startedAt);
          output.textContent = toolText(result);
          updateButton.textContent = "Update scheduled";
        } catch (error) {
          output.textContent = `Update failed: ${error && error.message ? error.message : String(error)}`;
          updateButton.disabled = false;
        }
        refreshDebugFooter();
        reportSize();
      });
      actions.append(updateButton);
    }

    const doctorButton = el("button", "button", "Run doctor");
    doctorButton.type = "button";
    doctorButton.addEventListener("click", async () => {
      doctorButton.disabled = true;
      doctorButton.textContent = "Running doctor…";
      const startedAt = performance.now();
      try {
        const result = await callTool("doctor", {});
        captureTiming("doctor", result, startedAt);
        doctorOutput.textContent = toolText(result);
        doctorOutput.classList.add("visible");
        doctorButton.textContent = "Run doctor again";
      } catch (error) {
        doctorOutput.textContent = `Doctor failed: ${error && error.message ? error.message : String(error)}`;
        doctorOutput.classList.add("visible");
        doctorButton.textContent = "Run doctor again";
      } finally {
        doctorButton.disabled = false;
      }
      refreshDebugFooter();
      reportSize();
    });
    actions.append(doctorButton);

    if (schema.refreshRecommended && views.settingsUrl) {
      const settingsButton = el("button", "button", "Open connector settings");
      settingsButton.type = "button";
      settingsButton.addEventListener("click", async () => {
        try {
          await openSettings(views.settingsUrl);
        } catch (error) {
          output.textContent = `Could not open settings: ${error && error.message ? error.message : String(error)}`;
          output.classList.add("visible");
          reportSize();
        }
      });
      actions.append(settingsButton);
    }

    body.append(actions, output, doctorOutput);
    if (data.nextStep) body.append(el("div", "next", data.nextStep));
    const debug = el("div", "debug");
    debug.id = "debug";
    body.append(debug);
    card.append(header, body);
    root.append(card);
    refreshDebugFooter();
    reportSize();
  }

  function refreshDebugFooter() {
    const debug = document.getElementById("debug");
    if (!debug || !currentData || !currentData.debug) return;
    debug.textContent = debugEntries.join(" · ");
    debug.classList.toggle("visible", debugEntries.length > 0);
  }

  function acceptToolResult(value) {
    const payload = setupPayload(value);
    if (payload) render(payload, value);
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
      acceptToolResult(message.params);
    } else if (message.method === "ui/notifications/host-context-changed") {
      applyHostContext(message.params);
    } else if (message.method === "ui/resource-teardown" && message.id !== undefined) {
      post({ jsonrpc: "2.0", id: message.id, result: {} });
    }
  });

  const legacyPayload = setupPayload(window.openai && (
    window.openai.toolResponseMetadata || window.openai.toolOutput
  ));
  if (legacyPayload) {
    render(legacyPayload, window.openai && window.openai.toolResponseMetadata);
  }
  window.addEventListener("openai:set_globals", event => {
    const globals = event.detail && event.detail.globals;
    if (!globals) return;
    const payload = setupPayload(globals.toolResponseMetadata || globals.toolOutput);
    if (payload) render(payload, globals.toolResponseMetadata);
  });

  request("ui/initialize", {
    protocolVersion: "2026-01-26",
    appInfo: { name: "codexify-setup", version: "1.0.0" },
    appCapabilities: {}
  }).then(result => {
    initialized = true;
    applyHostContext(result && result.hostContext);
    notify("ui/notifications/initialized", {});
    if ("ResizeObserver" in window) {
      resizeObserver = new ResizeObserver(reportSize);
      resizeObserver.observe(document.documentElement);
    }
    reportSize();
  }).catch(error => {
    if (!currentData) {
      root.replaceChildren(el("div", "notice", `Setup UI could not initialize: ${error && error.message ? error.message : String(error)}`));
    }
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
    fn setup_tool_metadata_links_an_app_accessible_widget() {
        let meta = tool_meta();
        assert_eq!(
            meta.get("ui").and_then(|value| value.get("resourceUri")),
            Some(&json!(SETUP_UI_URI))
        );
        assert_eq!(
            meta.get("openai/outputTemplate"),
            Some(&json!(SETUP_UI_URI))
        );
        assert_eq!(meta.get("openai/widgetAccessible"), Some(&json!(true)));
    }

    #[test]
    fn connector_ids_map_to_the_generic_chatgpt_settings_route() {
        assert_eq!(
            connector_settings_url("asdk_app_abc123").as_deref(),
            Some("https://chatgpt.com/#settings/Plugins/plugin_asdk_app_abc123")
        );
        assert_eq!(
            connector_settings_url("plugin_asdk_app_abc123").as_deref(),
            Some("https://chatgpt.com/#settings/Plugins/plugin_asdk_app_abc123")
        );
        assert!(connector_settings_url("plugin_").is_none());
        assert!(connector_settings_url("asdk_app_").is_none());
        assert!(connector_settings_url("../../settings").is_none());
    }

    #[test]
    fn setup_resource_contains_update_doctor_refresh_and_debug_paths() {
        let contents = contents_for_uri(SETUP_UI_URI).unwrap();
        let serialized = serde_json::to_value(contents).unwrap();
        let text = serialized["text"].as_str().unwrap();
        for expected in [
            "tools/call",
            "window.openai.callTool",
            "window.openai.openExternal",
            "self_update",
            "doctor",
            "click Refresh",
            "io.github.devnoname120/codexify/debug",
            "connector_id",
            "https://chatgpt.com/#settings/Plugins/",
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
        assert!(contents_for_uri("ui://codexify/setup/unknown").is_none());
    }
}
