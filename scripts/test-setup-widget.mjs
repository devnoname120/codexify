import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const html = readFileSync(new URL("../src/setup_ui.html", import.meta.url), "utf8");
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
const drain = () => new Promise(resolve => setImmediate(resolve));

class Events {
  listeners = new Map();
  addEventListener(name, callback) {
    this.listeners.set(name, [...(this.listeners.get(name) || []), callback]);
  }
  emit(name, event = {}) {
    for (const callback of this.listeners.get(name) || []) callback(event);
  }
}

class Element extends Events {
  children = [];
  className = "";
  id = "";
  ownText = "";
  disabled = false;
  checked = false;
  dataset = {};
  attributes = {};
  constructor(tagName) { super(); this.tagName = tagName; }
  get textContent() { return [this.ownText, ...this.children.map(node => node.textContent)].filter(Boolean).join(" "); }
  set textContent(value) { this.ownText = String(value); this.replaceChildren(); }
  append(...nodes) {
    for (let node of nodes) {
      if (typeof node === "string") { const text = new Element("#text"); text.ownText = node; node = text; }
      node.remove();
      node.parentElement = this;
      this.children.push(node);
    }
  }
  replaceChildren(...nodes) {
    for (const node of this.children) node.parentElement = null;
    this.children = [];
    this.append(...nodes);
  }
  remove() {
    if (this.parentElement) this.parentElement.children = this.parentElement.children.filter(node => node !== this);
    this.parentElement = null;
  }
  setAttribute(name, value) { this.attributes[name] = String(value); }
  get classList() {
    return { toggle: (name, enabled) => {
      const classes = new Set(this.className.split(/\s+/).filter(Boolean));
      if (enabled) classes.add(name); else classes.delete(name);
      this.className = [...classes].join(" ");
    } };
  }
  descendants() { return this.children.flatMap(node => [node, ...node.descendants()]); }
  querySelector(selector) {
    return this.descendants().find(node => selector.startsWith(".")
      ? node.className.split(/\s+/).includes(selector.slice(1))
      : selector.startsWith("#") ? node.id === selector.slice(1) : node.tagName === selector) || null;
  }
}

function payload(status, conversation = "1.2.3") {
  return {
    serverVersion: "1.2.4", worktreeMode: "auto",
    project: { status: "static", selectionAvailable: false },
    update: { status: "up_to_date", currentVersion: "1.2.4", latestVersion: "1.2.4" },
    connectorSchema: {
      status, advertisedVersion: "1.2.4", observedVersion: conversation,
      connectorVersion: status === "stale" ? "1.2.3" : status === "unknown" ? null : "1.2.4",
      refreshRecommended: status === "stale"
    }
  };
}

function harness(initial, live = initial) {
  const root = new Element("div"); root.id = "root";
  const body = new Element("body"); body.append(root);
  const document = new Events();
  Object.assign(document, {
    body, activeElement: body, hidden: false,
    documentElement: { clientWidth: 370, scrollHeight: 200, dataset: {} },
    createElement: tag => new Element(tag),
    createTextNode: text => { const node = new Element("#text"); node.ownText = text; return node; },
    getElementById: id => body.querySelector(`#${id}`)
  });
  const state = { live: structuredClone(live), fail: false, calls: [], links: [], now: 0 };
  const timers = new Map(); let nextTimer = 1;
  const timer = (callback, delay, interval = false) => {
    const id = nextTimer++; timers.set(id, { callback, delay, interval }); return id;
  };
  const window = new Events();
  const callTool = async (name, args) => {
    state.calls.push({ name, args: structuredClone(args) });
    if (name === "setup_status") {
      if (state.fail) throw new Error("offline");
      return { structuredContent: structuredClone(state.live) };
    }
    if (name === "doctor") return { structuredContent: { ok: true, summary: { failures: 0, warnings: 0 }, checks: [] } };
    if (name === "list_projects") return { structuredContent: { projects: [{ name: "Demo", selector: "demo" }], total: 1 } };
    if (name === "set_project_root") return { structuredContent: { mode: "project", active_root: "/demo", managed_worktree: args.createWorktree } };
    throw new Error(`Unexpected tool: ${name}`);
  };
  const parent = {
    postMessage: message => {
      if (message.id === undefined || !message.method) return;
      queueMicrotask(async () => {
        try {
          const result = message.method === "tools/call"
            ? await callTool(message.params.name, message.params.arguments)
            : {};
          window.emit("message", { source: parent, data: { jsonrpc: "2.0", id: message.id, result } });
        } catch (error) {
          window.emit("message", { source: parent, data: { jsonrpc: "2.0", id: message.id, error: { message: error.message } } });
        }
      });
    }
  };
  Object.assign(window, {
    parent, location: { hostname: "asdk_app_test.web-sandbox.oaiusercontent.com" },
    openai: {
      toolOutput: { structuredContent: structuredClone(initial) }, callTool,
      openExternal: async options => { state.links.push(options); return {}; }
    }
  });
  vm.runInNewContext(script, {
    window, document, console, URL, performance,
    Date: class extends Date { static now() { return state.now; } },
    setTimeout: (callback, delay) => timer(callback, delay), clearTimeout: id => timers.delete(id),
    setInterval: (callback, delay) => timer(callback, delay, true), clearInterval: id => timers.delete(id),
    requestAnimationFrame: callback => queueMicrotask(callback)
  });
  const buttons = () => root.descendants().filter(node => node.tagName === "button");
  return {
    state, root, document,
    text: () => root.textContent,
    hasButton: label => buttons().some(node => node.textContent === label),
    async click(label) {
      const button = buttons().find(node => node.textContent === label);
      assert.ok(button, `Missing button ${label}: ${root.textContent}`);
      assert.equal(button.disabled, false);
      button.emit("click", { target: button }); await drain();
    },
    async tick() {
      state.now += 30_000;
      for (const entry of [...timers.values()]) if (entry.interval) entry.callback();
      await drain();
    },
    notify(method, params) { window.emit("message", { source: parent, data: { jsonrpc: "2.0", method, params } }); },
    teardown() { window.emit("message", { source: parent, data: { jsonrpc: "2.0", id: 999, method: "ui/resource-teardown", params: {} } }); }
  };
}

test("current connector and conversation need no action", async () => {
  const card = harness(payload("current", "1.2.4")); await drain();
  assert.match(card.text(), /Connector schema: v1\.2\.4/);
  assert.equal(card.hasButton("Refresh"), false);
  assert.doesNotMatch(card.text(), /Start a new conversation|unverified/);
});

test("reload elsewhere replaces Refresh with a new-conversation message and clears feedback", async () => {
  const card = harness(payload("stale")); await drain();
  await card.click("Refresh");
  assert.equal(card.state.links.length, 1);
  assert.match(card.state.links[0].href, /plugin_asdk_app_test:~:text=Information-,Refresh,-Connected$/);
  assert.equal(card.hasButton("Refresh"), true);
  assert.match(card.text(), /In ChatGPT settings/);
  card.state.live = payload("conversation_stale"); await card.tick();
  assert.match(card.text(), /This conversation uses schema v1\.2\.3\. Start a new conversation/);
  assert.equal(card.hasButton("Refresh"), false);
  assert.doesNotMatch(card.text(), /In ChatGPT settings/);
  assert.ok(card.state.calls.filter(call => call.name === "setup_status").every(call => call.args.conversationVersion === "1.2.3"));
});

test("legacy conversations without a marker get a readable new-conversation message", async () => {
  const card = harness(payload("conversation_stale", null)); await drain();
  assert.match(card.text(), /This conversation uses an older schema\. Start a new conversation/);
  assert.doesNotMatch(card.text(), /vnull|vundefined/);
  assert.equal(card.hasButton("Refresh"), false);
});

test("replayed snapshots cannot restore old actions or replace the conversation marker", async () => {
  const old = payload("stale");
  old.update = { status: "update_available", currentVersion: "1.2.3", latestVersion: "1.2.4" };
  const card = harness(old, payload("conversation_stale")); await drain();
  card.notify("ui/notifications/tool-result", { structuredContent: old }); await drain();
  assert.equal(card.hasButton("Refresh"), false);
  assert.equal(card.hasButton("Upgrade to v1.2.4"), false);
  card.document.hidden = true; card.document.emit("visibilitychange");
  card.notify("ui/notifications/tool-result", { structuredContent: payload("current", "1.2.4") });
  card.document.hidden = false; card.document.emit("visibilitychange"); await drain();
  assert.equal(card.state.calls.filter(call => call.name === "setup_status").at(-1).args.conversationVersion, "1.2.3");
  assert.match(card.text(), /Start a new conversation/);
});

test("Refresh rechecks the connector before opening settings", async () => {
  const card = harness(payload("stale")); await drain();
  card.state.now += 2_000;
  card.state.live = payload("conversation_stale");
  await card.click("Refresh");
  assert.equal(card.state.links.length, 0);
  assert.equal(card.hasButton("Refresh"), false);
  assert.match(card.text(), /Start a new conversation/);
});

test("unknown or failed status never exposes an unverified label or stale action", async () => {
  const card = harness(payload("stale"), payload("unknown")); await drain();
  assert.doesNotMatch(card.text(), /Connector schema:|unverified/);
  assert.equal(card.hasButton("Refresh"), false);
  card.state.fail = true; await card.tick();
  assert.match(card.text(), /status unavailable/);
  assert.doesNotMatch(card.text(), /Connector schema:|unverified/);
});

test("periodic status checks preserve workspace controls and explicit worktree choice", async () => {
  const initial = payload("current", "1.2.4");
  initial.project = { status: "unselected", selectionAvailable: true };
  const card = harness(initial); await drain();
  const checkbox = card.document.getElementById("create-worktree");
  assert.ok(checkbox); assert.equal(checkbox.checked, true);
  const text = card.text();
  assert.ok(text.indexOf("Connector schema:") < text.indexOf("Choose a workspace"));
  assert.ok(text.indexOf("Chat without a project") < text.indexOf("Create a worktree"));
  checkbox.checked = false; checkbox.emit("change", { target: checkbox });
  await card.tick();
  assert.equal(card.document.getElementById("create-worktree"), checkbox);
  assert.equal(checkbox.checked, false);
  const projectButton = card.root.descendants().find(node => node.tagName === "button" && node.textContent.startsWith("Demo"));
  assert.ok(projectButton);
  projectButton.emit("click"); await drain();
  assert.equal(card.state.calls.find(call => call.name === "set_project_root").args.createWorktree, false);
});

test("teardown stops live status polling", async () => {
  const card = harness(payload("current", "1.2.4")); await drain();
  const count = card.state.calls.length;
  card.teardown(); await card.tick();
  assert.equal(card.state.calls.length, count);
});
