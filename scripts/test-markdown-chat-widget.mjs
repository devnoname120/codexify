import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { createRequire } from "node:module";
import test from "node:test";
import vm from "node:vm";
import { chatHtml as html, setupChatHtml } from "./chat-widget-source.mjs";

const { chromium, webkit } = createRequire(import.meta.url)("playwright");
const META = "io.github.devnoname120/codexify/markdown-chat";
const ENABLED = "io.github.devnoname120/codexify/markdown-chat-enabled";
const DUPLICATE_WARNING = "ChatGPT started a duplicated agent on this same project. This is a ChatGPT bug and it\u2019s problematic because then two agents can fight to do edits and overwrite each other. The duplicated agent was asked to stop in order to let the other agent work without interference";
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

const presenceDetails = vm.runInNewContext("(" + html.slice(html.indexOf("function presenceDetails("), html.indexOf("  function toolCallLabel(")) + ")");
test("presence: exact three/five-minute boundaries, unknown activity, and clock skew", () => {
  for (const [age, state, label] of [
    [0, "online", "online"], [179999, "online", "online"],
    [180000, "away", "last seen 3 mins ago"], [239999, "away", "last seen 3 mins ago"],
    [240000, "away", "last seen 4 mins ago"], [299999, "away", "last seen 4 mins ago"],
    [300000, "offline", "offline"], [3600000, "offline", "offline"], [-5000, "online", "online"]
  ]) {
    const actual = presenceDetails(1000000, 1000000 + age);
    assert.equal(actual.state, state, `age ${age}`);
    assert.equal(actual.label, label, `age ${age}`);
    if (actual.delay !== null) assert(actual.delay > 0);
  }
  for (const missing of [null, undefined, NaN]) assert.equal(presenceDetails(missing, 1000000).state, "offline");
});

test("presence: waiting overrides age only until its deadline, then uses ordinary presence", () => {
  const now = 1000000;
  for (const [lastCall, fallback] of [[now, "online"], [now - 240000, "away"], [now - 360000, "offline"], [null, "offline"]]) {
    assert.equal(presenceDetails(lastCall, now, now + 20000).state, "waiting");
    assert.equal(presenceDetails(lastCall, now, now + 20000).delay, 20000);
    assert.equal(presenceDetails(lastCall, now + 19999, now + 20000).state, "waiting");
    assert.equal(presenceDetails(lastCall, now + 20000, now + 20000).state, fallback);
    for (const invalid of [null, undefined, NaN, Infinity, -1]) {
      assert.equal(presenceDetails(lastCall, now, invalid).state, fallback);
    }
  }
});

function setupPayload(selected = true) {
  return {
    serverVersion:"1.4.0", worktreeMode:"never",
    project:selected ? { status:"selected", name:"codexify", activePath:"/worktrees/codexify", sourcePath:"/projects/codexify", managedWorktree:true } : { status:"unselected", selectionAvailable:true },
    update:{ status:"up_to_date", currentVersion:"1.4.0", latestVersion:"1.4.0" },
    connectorSchema:{ status:"current", advertisedVersion:"1.4.0+markdown-chat-v2", observedVersion:"1.4.0+markdown-chat-v2", connectorVersion:"1.4.0+markdown-chat-v2", refreshRecommended:false }
  };
}

class ChatBackend {
  messages = [];
  calls = [];
  delivered = 0;
  read = 0;
  lastAgentCall = null;
  agentWaitingUntil = null;
  totalToolCalls = 0;
  serverTime = null;
  revision = 0;
  end = 180;
  failSends = 0;
  toolErrorSends = 0;
  failAfterSave = false;
  failState = false;
  pageSize = 50;
  setup = setupPayload();
  chatEnabled = true;
  downloadsSupported = true;
  add(role, markdown, id = `fixture-${this.messages.length}`) {
    const start = this.end;
    this.end += markdown.length + 150;
    const message = { id, role, markdown, start, end:this.end, created_at_ms:Date.now(), tool_call_count:this.totalToolCalls };
    this.messages.push(message); this.revision++;
    return message;
  }
  async call(name, args) {
    this.calls.push({ name, args });
    if (name === "chat_ui_file") return { _meta:{ [META]:{ file:{ type:"resource_link", uri:"codexify://artifact/" + "a".repeat(43), name:"report one.png", mimeType:"image/png" } } } };
    if (name === "setup_status") return { structuredContent:this.setup, _meta:{ [ENABLED]:this.chatEnabled } };
    if (name === "doctor") return { structuredContent:{ ok:true, checks:[], summary:{ passed:1, failures:0, warnings:0, skipped:0 } } };
    if (name === "setup_ui_list_projects") return { structuredContent:{ projects:[{ selector:"codexify", name:"codexify" }], total:1, warnings:[] } };
    if (name === "setup_ui_select_project") {
      const scratch = args.withoutProject === true;
      this.setup.project = { status:scratch ? "without_project" : "selected", name:scratch ? "Chat without a project" : "codexify", activePath:scratch ? "/private/scratch" : "/worktrees/codexify", managedWorktree:!scratch };
      return { structuredContent:{ mode:scratch ? "without_project" : "project", active_root:this.setup.project.activePath, project_name:this.setup.project.name, managed_worktree:!scratch } };
    }
    if (name === "chat_ui_send") {
      if (this.toolErrorSends-- > 0) return { isError:true, content:[{ type:"text", text:"The message was not saved." }] };
      if (this.failSends-- > 0) throw new Error("Temporary send failure");
      let message = this.messages.find(message => message.id === args.request_id);
      if (message) assert.equal(message.markdown, args.message);
      else { message = this.add("user", args.message, args.request_id); this.agentWaitingUntil = null; }
      if (this.failAfterSave) { this.failAfterSave = false; throw new Error("Response lost after save"); }
      return { content:[{ type:"text", text:"Message saved." }], _meta:{ [META]:{ sent:{ id:message.id, end:message.end, created_at_ms:message.created_at_ms, tool_call_count:message.tool_call_count } } } };
    }
    assert.equal(name, "chat_ui_state", "UI must use only app-only chat tools");
    if (this.failState) throw new Error("Temporary state failure");
    const revision = `${this.revision}-${this.delivered}-${this.read}`;
    const all = this.messages.filter(message => args.before === undefined || message.start < args.before);
    const unchanged = args.before === undefined && revision === args.revision;
    const messages = unchanged ? [] : all.slice(-this.pageSize);
    return {
      content:[{ type:"text", text:"Widget state updated." }],
      _meta:{ [META]:{
        chat_file:"/private/project/chats/conversation/CHAT.md", revision,
        delivered_through:this.delivered, read_through:this.read,
        last_agent_call_at_ms:this.lastAgentCall, agent_waiting_until_ms:this.agentWaitingUntil, total_tool_calls:this.totalToolCalls, server_time_ms:this.serverTime ?? Date.now(), messages,
        has_more:all.length > messages.length && !unchanged,
        before:messages[0]?.start ?? null, unchanged
      } }
    };
  }
}

async function mount(browser, backend, { width = 390, theme = "light", count = 1, bridge = "legacy", nested = false, saved = {}, combined = false, clock = null, scale = 1, timezone } = {}) {
  const page = await browser.newPage({ viewport:{ width, height:1400 }, colorScheme:theme, deviceScaleFactor:scale, timezoneId:timezone });
  page.setDefaultTimeout(8000);
  if (clock !== null) await page.clock.install({ time:clock });
  const errors = [], hostMessages = [], widgetStates = [], downloads = [];
  page.on("pageerror", error => errors.push(error.message));
  await page.exposeFunction("mockTool", async (name, args) => {
    const result = await backend.call(name, args);
    return nested ? { mcp_tool_result:result } : result;
  });
  await page.exposeFunction("saveWidget", state => { widgetStates.push(state); });
  await page.exposeFunction("hostMessage", message => { hostMessages.push(message); });
  await page.exposeFunction("hostDownload", params => { downloads.push(params); });
  await page.route("https://codexify-widget.test/", route => route.fulfill({
    contentType:"text/html", body:'<!doctype html><html><body style="margin:0"></body></html>'
  }));
  await page.goto("https://codexify-widget.test/");
  await page.evaluate(({ html, count, bridge, theme, saved, initial, downloadsSupported }) => {
    window.addEventListener("message", async event => {
      const message = event.data;
      if (message?.jsonrpc !== "2.0") return;
      if (message.method === "ui/notifications/size-changed") return;
      if (message.method === "ui/notifications/initialized" && initial) {
        event.source.postMessage({ jsonrpc:"2.0", method:"ui/notifications/tool-result", params:initial }, "*");
        return;
      }
      if (!message.method || message.id === undefined) return;
      window.hostMessage(message.method);
      let result;
      try {
        if (message.method === "ui/initialize") result = { hostContext:{ theme }, protocolVersion:"2026-01-26", hostCapabilities:downloadsSupported ? { downloadFile:{} } : {} };
        else if (message.method === "tools/call") result = await window.mockTool(message.params.name, message.params.arguments);
        else if (message.method === "ui/download-file") { await window.hostDownload(message.params); result = {}; }
        else if (message.method === "ui/open-link") result = {};
        else throw new Error(`Unexpected host request ${message.method}`);
        event.source.postMessage({ jsonrpc:"2.0", id:message.id, result }, "*");
      } catch (error) { event.source.postMessage({ jsonrpc:"2.0", id:message.id, error:{ message:error.message } }, "*"); }
    });
    for (let i = 0; i < count; i++) {
      const frame = document.createElement("iframe");
      frame.id = `widget-${i}`; frame.title = `Chat widget ${i}`;
      frame.style.cssText = "display:block;border:0;width:100%;height:620px";
      const bootstrap = bridge === "legacy"
        ? `<script>window.openai={theme:${JSON.stringify(theme)},toolOutput:${JSON.stringify(initial)},widgetState:${JSON.stringify(saved)},callTool:(name,args)=>parent.mockTool(name,args),setWidgetState:value=>parent.saveWidget(value),openExternal:()=>Promise.resolve({})};<\/script>`
        : "";
      frame.srcdoc = html.replace("<head>", "<head>" + bootstrap);
      document.body.append(frame);
    }
  }, { html:combined ? setupChatHtml : html, count, bridge, theme, saved, downloadsSupported:backend.downloadsSupported, initial:combined ? { structuredContent:backend.setup, _meta:{ [ENABLED]:backend.chatEnabled } } : null });
  const frames = Array.from({ length:count }, (_, i) => page.frameLocator(`#widget-${i}`));
  await frames[0].getByText("Loading this conversation...", { exact:true }).waitFor({ state:"hidden" });
  return { page, frames, errors, hostMessages, widgetStates, downloads };
}

async function refresh(frame) {
  await frame.locator("body").evaluate(() => window.dispatchEvent(new Event("focus")));
}

async function timeline(frame) {
  return frame.locator("#messages > .message, #messages > .tool-call-marker")
    .evaluateAll(nodes => nodes.map(node => node.classList.contains("tool-call-marker") ? node.textContent : node.querySelector(".markdown").textContent));
}

async function expectTimeline(frame, expected) {
  let actual;
  for (let attempt = 0; attempt < 80; attempt++) {
    actual = await timeline(frame);
    if (JSON.stringify(actual) === JSON.stringify(expected)) return;
    await sleep(50);
  }
  assert.deepEqual(actual, expected);
}

for (const [engineName, engine] of [["Chromium", chromium], ["WebKit", webkit]]) {
  test(`${engineName}: awaiting UI`, { timeout:120000 }, async t => {
    const browser = await engine.launch();
    try {
      for (const theme of ["light", "dark"]) for (const combined of [false, true]) {
        await t.test(`waiting bubble, draft cue and offline fallback (${theme}, ${combined ? "setup" : "standalone"})`, async () => {
          const now = Date.UTC(2026, 8, 25, 12);
          const backend = new ChatBackend();
          backend.serverTime = now; backend.lastAgentCall = now - 360000;
          backend.add("agent", "The implementation is ready. What would you like changed?");
          backend.agentWaitingUntil = now + 20000;
          const { page, frames:[frame], errors } = await mount(browser, backend, { clock:now, combined, theme, width:390 });
          await frame.locator('#presence[data-state="waiting"]').waitFor();
          assert.equal(await frame.locator(".presence-symbol circle").count(), 3);
          assert.equal(await frame.locator(".presence-symbol path").getAttribute("fill"), "currentColor");
          assert.deepEqual(await frame.locator(".presence-symbol circle").evaluateAll(nodes => nodes.map(node => node.getAttribute("fill"))), ["var(--bg)", "var(--bg)", "var(--bg)"]);
          assert.equal(await frame.locator("#draft").getAttribute("placeholder"), "Message the agent...");
          assert.equal(await frame.locator("#draft").getAttribute("aria-describedby"), "awaiting-hint");
          await frame.locator("#awaiting-hint").waitFor();
          const border = await frame.locator("#draft").evaluate(node => getComputedStyle(node).borderColor);
          assert.equal(border, theme === "dark" ? "rgb(255, 133, 142)" : "rgb(196, 49, 59)");
          assert.notEqual(await frame.locator("#draft").evaluate(node => getComputedStyle(node, "::placeholder").color), border);
          assert.equal(await frame.locator("html").evaluate(node => node.scrollWidth > innerWidth), false);
          mkdirSync(new URL("../target/awaiting-previews/", import.meta.url), { recursive:true });
          await frame.locator("#chat").screenshot({ path:new URL(`../target/awaiting-previews/${engineName.toLowerCase()}-${theme}-${combined ? "setup" : "chat"}.png`, import.meta.url).pathname });
          await frame.getByRole("textbox").fill("Keep this unsent draft");
          await frame.locator("#awaiting-hint").waitFor();
          backend.failState = true;
          await page.clock.fastForward(20000);
          await frame.locator('#presence[data-state="offline"]').waitFor();
          assert.equal(await frame.locator("#awaiting-hint").isVisible(), false);
          assert.equal(await frame.getByRole("textbox").inputValue(), "Keep this unsent draft");
          assert.equal(await frame.locator("#draft").getAttribute("placeholder"), "Message the agent...");
          assert.equal(await frame.locator("#draft").getAttribute("aria-describedby"), null);
          assert.deepEqual(errors, []); await page.close();
        });
      }
      await t.test("successful reply clears waiting despite stale polling; a new await restores it", async () => {
        const backend = new ChatBackend();
        backend.lastAgentCall = Date.now(); backend.agentWaitingUntil = Date.now() + 120000;
        const { page, frames:[frame], errors } = await mount(browser, backend);
        await frame.locator('#presence[data-state="waiting"]').waitFor();
        backend.failState = true;
        await frame.getByRole("textbox").fill("Please continue");
        await frame.getByRole("textbox").press("Enter");
        await frame.locator('#presence[data-state="online"]').waitFor();
        await frame.locator("#refresh").waitFor();
        backend.failState = false; backend.agentWaitingUntil = Date.now() + 120000;
        backend.totalToolCalls = 1;
        await frame.locator("#refresh").click();
        await frame.locator("#tool-total").getByText("1 tool call", { exact:true }).waitFor();
        assert.equal(await frame.locator("#presence").getAttribute("data-state"), "online", "a pre-reply poll must not resurrect the bubble");
        backend.read = backend.messages[0].end; backend.totalToolCalls = 2;
        await page.evaluate(() => document.querySelector("iframe").contentWindow.dispatchEvent(new Event("focus")));
        await frame.locator('#presence[data-state="waiting"]').waitFor();
        backend.failSends = 1;
        await frame.getByRole("textbox").fill("Another message");
        await frame.getByRole("textbox").press("Enter");
        await frame.getByRole("button", { name:"Retry", exact:true }).waitFor();
        assert.equal(await frame.locator("#presence").getAttribute("data-state"), "waiting", "a failed send must retain waiting");
        assert.deepEqual(errors, []); await page.close();
      });
    } finally { await browser.close(); }
  });
  test(`${engineName}: composer sizing`, { timeout:180000 }, async t => {
    const browser = await engine.launch();
    try {
      for (const width of [370, 440, 640]) {
        for (const combined of [false, true]) {
          await t.test(`composer scrolls only at its height limit (${width}px, ${combined ? "setup" : "standalone"})`, async t => {
            const backend = new ChatBackend();
            const { page, frames:[frame], errors } = await mount(browser, backend, { width, combined });
            t.after(() => page.close());
            const input = frame.getByRole("textbox", { name:"Message the agent" });
            const geometry = () => input.evaluate(node => {
              const style = getComputedStyle(node);
              node.scrollTop = node.scrollHeight;
              return {
                height:node.getBoundingClientRect().height,
                limit:parseFloat(style.maxHeight),
                minimum:parseFloat(style.lineHeight) + parseFloat(style.paddingTop) + parseFloat(style.paddingBottom),
                clientHeight:node.clientHeight, scrollHeight:node.scrollHeight, scrollTop:node.scrollTop
              };
            });
            const fits = async label => {
              const box = await geometry();
              assert(box.clientHeight >= Math.floor(box.minimum), `${label}: line is clipped ${JSON.stringify(box)}`);
              assert.equal(box.scrollHeight, box.clientHeight, `${label}: unexpected overflow ${JSON.stringify(box)}`);
              assert.equal(box.scrollTop, 0, `${label}: the composer must not scroll`);
              return box.height;
            };
            const emptyHeight = await fits("initial empty field");
            await input.fill("A short draft");
            assert.equal(await fits("single line"), emptyHeight);
            await input.fill("First line\nSecond line\nThird line");
            assert(await fits("three lines") > emptyHeight);
            await input.fill("A draft with enough words to wrap naturally on a narrow screen without reaching the composer height limit.");
            await fits("wrapped draft");
            const longDraft = Array.from({ length:24 }, (_, index) => `Line ${index + 1}`).join("\n");
            await input.fill(longDraft);
            const long = await geometry();
            assert.equal(long.height, long.limit);
            assert(long.scrollHeight > long.clientHeight);
            assert(long.scrollTop > 0, "Long drafts must remain scrollable");
            await input.fill("Short again");
            assert.equal(await fits("shortened draft"), emptyHeight);
            await input.fill("");
            assert.equal(await fits("cleared draft"), emptyHeight);
            await input.fill(longDraft);
            await frame.getByRole("button", { name:"Send message", exact:true }).click();
            await frame.getByRole("img", { name:"Saved to CHAT.md", exact:true }).waitFor();
            assert.equal(await input.inputValue(), "");
            assert.equal(await fits("empty after sending"), emptyHeight);
            assert.equal(backend.messages.length, 1);
            assert.equal(backend.messages[0].markdown, longDraft);
            assert.deepEqual(errors, []);
          });
        }
      }
    } finally { await browser.close(); }
  });
  test(`${engineName}: tool-call chronology`, { timeout:120000 }, async t => {
    const browser = await engine.launch();
    try {
      await t.test("send freezes earlier calls above the user; only subsequent calls follow it", async () => {
        const backend = new ChatBackend();
        backend.totalToolCalls = 1;
        backend.add("agent", "I am running the regression tests.");
        backend.totalToolCalls = 20;
        const { page, frames:[frame], errors } = await mount(browser, backend, { combined:true });
        await expectTimeline(frame, ["I am running the regression tests.", "19 tool calls"]);
        const normalCall = backend.call.bind(backend);
        let releaseSend;
        const held = new Promise(resolve => { releaseSend = resolve; });
        backend.call = async (name, args) => {
          if (name === "chat_ui_send") await held;
          return normalCall(name, args);
        };
        await frame.getByRole("textbox").fill("And now?");
        await frame.getByRole("textbox").press("Enter");
        await frame.getByText("Sending...", { exact:true }).waitFor();
        try {
          await expectTimeline(frame, ["I am running the regression tests.", "19 tool calls", "And now?"]);
        } finally { releaseSend(); }
        await frame.getByRole("img", { name:"Saved to CHAT.md", exact:true }).waitFor();
        await expectTimeline(frame, ["I am running the regression tests.", "19 tool calls", "And now?"]);
        backend.totalToolCalls = 22;
        await refresh(frame);
        await expectTimeline(frame, ["I am running the regression tests.", "19 tool calls", "And now?", "2 tool calls"]);
        if (process.env.CODEXIFY_CHAT_MARKER_PREVIEW_DIR) {
          mkdirSync(process.env.CODEXIFY_CHAT_MARKER_PREVIEW_DIR, { recursive:true });
          await frame.locator("#chat").screenshot({ path:`${process.env.CODEXIFY_CHAT_MARKER_PREVIEW_DIR}/${engineName.toLowerCase()}-after-send.png` });
        }
        assert.deepEqual(errors, []);
        await page.close();
      });
      await t.test("several user messages preserve chronological intervals across cards and reloads", async () => {
        const backend = new ChatBackend();
        backend.totalToolCalls = 2; backend.add("agent", "Starting.");
        backend.totalToolCalls = 5; backend.add("user", "First instruction.");
        backend.totalToolCalls = 7; backend.add("user", "Second instruction.");
        backend.add("user", "One more detail.");
        backend.totalToolCalls = 8; backend.add("agent", "Acknowledged.");
        backend.totalToolCalls = 10;
        const expected = ["Starting.", "3 tool calls", "First instruction.", "2 tool calls", "Second instruction.", "One more detail.", "1 tool call", "Acknowledged.", "2 tool calls"];
        for (const bridge of ["legacy", "mcp"]) {
          const { page, frames, errors } = await mount(browser, backend, { count:2, bridge });
          for (const frame of frames) {
            await expectTimeline(frame, expected);
            await refresh(frame);
            await expectTimeline(frame, expected);
          }
          assert.deepEqual(errors, []); await page.close();
        }
        backend.pageSize = 3;
        const { page, frames:[frame] } = await mount(browser, backend);
        await expectTimeline(frame, expected.slice(4));
        await frame.getByRole("button", { name:"Load earlier messages", exact:true }).click();
        await expectTimeline(frame, expected);
        await page.close();
      });
      await t.test("lost response retries retain the saved counter even when history is unavailable", async () => {
        const backend = new ChatBackend();
        backend.totalToolCalls = 1; backend.add("agent", "Working.");
        backend.totalToolCalls = 4;
        const { page, frames:[frame], errors } = await mount(browser, backend);
        await expectTimeline(frame, ["Working.", "3 tool calls"]);
        backend.failAfterSave = true; backend.failState = true;
        await frame.getByRole("textbox").fill("Status?");
        await frame.getByRole("textbox").press("Enter");
        await frame.getByRole("button", { name:"Retry", exact:true }).waitFor();
        await expectTimeline(frame, ["Working.", "3 tool calls", "Status?"]);
        backend.totalToolCalls = 8;
        await frame.getByRole("button", { name:"Retry", exact:true }).click();
        await frame.getByRole("img", { name:"Saved to CHAT.md", exact:true }).waitFor();
        await expectTimeline(frame, ["Working.", "3 tool calls", "Status?"]);
        assert.equal(backend.messages.length, 2);
        backend.failState = false; await refresh(frame);
        await expectTimeline(frame, ["Working.", "3 tool calls", "Status?", "4 tool calls"]);
        assert.deepEqual(errors, []); await page.close();
      });
      await t.test("legacy gaps do not invent a call split; new boundaries resume counting", async () => {
        const backend = new ChatBackend();
        backend.totalToolCalls = 1; backend.add("agent", "Older report.");
        backend.totalToolCalls = 5; backend.add("user", "Old message without a counter.").tool_call_count = null;
        backend.totalToolCalls = 7; backend.add("agent", "New report.");
        backend.totalToolCalls = 8;
        const { page, frames:[frame], errors } = await mount(browser, backend);
        await expectTimeline(frame, ["Older report.", "Old message without a counter.", "New report.", "1 tool call"]);
        assert.deepEqual(errors, []); await page.close();
      });
    } finally { await browser.close(); }
  });
}

for (const [engineName, engine] of [["Chromium", chromium], ["WebKit", webkit]]) {
  test(`${engineName}: Markdown chat composer, synchronization and receipts`, { timeout:180000 }, async t => {
    const browser = await engine.launch();
    try {
      await t.test("chat history is taller without moving the composer into its scroller", async () => {
        for (const [width, expected] of [[390, 420], [640, 480]]) {
          const backend = new ChatBackend();
          for (let i = 0; i < 14; i++) backend.add("agent", `Message ${i}\n\nEnough text to exercise the scrolling history.`);
          const { page, frames:[frame], errors } = await mount(browser, backend, { width, combined:true });
          const layout = await frame.locator("#messages").evaluate(node => ({
            limit:parseFloat(getComputedStyle(node).maxHeight),
            height:node.getBoundingClientRect().height,
            scrolls:node.scrollHeight > node.clientHeight,
            ownsComposer:node.contains(node.getRootNode().getElementById("composer"))
          }));
          assert.equal(layout.limit, expected);
          assert.equal(layout.height, expected);
          assert.equal(layout.scrolls, true);
          assert.equal(layout.ownsComposer, false);
          assert.equal(await frame.locator("html").evaluate(node => node.scrollWidth > innerWidth), false);
          assert.deepEqual(errors, []); await page.close();
        }
      });
      await t.test("Markdown tables, reference links, balanced URLs and exported files", async () => {
        const backend = new ChatBackend();
        backend.add("agent", [
          "| Case | Result |", "| :--- | ---: |", "| **Prose** | [Docs][docs] |", "| Escaped \\| pipe | ~~old~~ |", "",
          "[Parentheses](https://example.com/a_(b)) and https://example.com/help", "",
          "[Screenshot](sandbox:/mnt/data/report%20one.png)", "![Image reference][image]", "",
          "- Parent", "  - Nested **child**", "",
          "[docs]: https://example.com/docs \"Documentation\"", "[image]: sandbox:/mnt/data/report%20one.png", "",
          "[Unsafe](javascript:alert(1)) <script>alert(1)</script>"
        ].join("\n"));
        const { page, frames:[frame], errors, downloads, hostMessages } = await mount(browser, backend, { combined:true, bridge:"mcp" });
        await frame.locator(".markdown table").waitFor();
        assert.equal(await frame.locator(".markdown th").count(), 2);
        assert.equal(await frame.locator(".markdown td").count(), 4);
        assert.equal(await frame.locator(".markdown td").nth(2).textContent(), "Escaped | pipe");
        assert.equal(await frame.locator(".markdown s").textContent(), "old");
        assert.equal(await frame.locator(".markdown ul ul strong").textContent(), "child");
        assert.equal(await frame.getByRole("link", { name:"Docs", exact:true }).getAttribute("href"), "https://example.com/docs");
        assert.equal(await frame.getByRole("link", { name:"Parentheses", exact:true }).getAttribute("href"), "https://example.com/a_(b)");
        assert.equal(await frame.locator(".markdown script, .markdown a[href^='javascript:']").count(), 0);
        assert.equal(await frame.getByRole("link", { name:"Image reference", exact:true }).count(), 1);
        await frame.getByRole("link", { name:"Screenshot", exact:true }).click();
        await frame.getByText("File download requested.", { exact:true }).waitFor();
        await refresh(frame);
        assert.equal(await frame.getByText("File download requested.", { exact:true }).isVisible(), true);
        assert(backend.calls.some(call => call.name === "chat_ui_file" && call.args.href === "sandbox:/mnt/data/report%20one.png"));
        assert.equal(downloads.length, 1);
        assert.equal(downloads[0].contents[0].type, "resource_link");
        assert.equal(downloads[0].contents[0].name, "report one.png");
        assert(!hostMessages.includes("ui/message"));
        assert.equal(backend.delivered, 0); assert.equal(backend.read, 0);
        assert.equal(await frame.locator("html").evaluate(node => node.scrollWidth > innerWidth), false);
        mkdirSync(new URL("../target/chat-markdown-previews/", import.meta.url), { recursive:true });
        await frame.locator("#chat").screenshot({ path:new URL(`../target/chat-markdown-previews/${engineName.toLowerCase()}-markdown.png`, import.meta.url).pathname });
        assert.deepEqual(errors, []); await page.close();
      });
      await t.test("unsupported file downloads explain the limitation without opening a sandbox URL", async () => {
        const backend = new ChatBackend(); backend.downloadsSupported = false;
        backend.add("agent", "[Report](sandbox:/mnt/data/report%20one.png)");
        const { page, frames:[frame], downloads, hostMessages, errors } = await mount(browser, backend, { combined:true, bridge:"mcp" });
        await frame.getByRole("link", { name:"Report", exact:true }).click();
        await frame.getByText("This host cannot download files from a widget. Open the exported attachment in the ChatGPT conversation.", { exact:true }).waitFor();
        assert.equal(downloads.length, 0);
        assert(!hostMessages.includes("ui/open-link")); assert(!hostMessages.includes("ui/message"));
        assert.deepEqual(errors, []); await page.close();
      });
      await t.test("Enter, Shift+Enter, button, IME, and grey/blue receipt transitions", async () => {
        const backend = new ChatBackend();
        const { page, frames:[frame], errors, hostMessages, widgetStates } = await mount(browser, backend);
        const input = frame.getByRole("textbox", { name:"Message the agent" });
        const send = frame.getByRole("button", { name:"Send message", exact:true });
        assert.equal(await send.isDisabled(), true);
        await input.fill("First line"); await input.press("Shift+Enter"); await input.pressSequentially("Second");
        assert.equal(await input.inputValue(), "First line\nSecond");
        assert.equal(backend.calls.filter(call => call.name === "chat_ui_send").length, 0);
        await input.dispatchEvent("keydown", { key:"Enter", code:"Enter", isComposing:true });
        assert.equal(backend.calls.filter(call => call.name === "chat_ui_send").length, 0);
        await input.press("Enter");
        await frame.getByRole("img", { name:"Saved to CHAT.md", exact:true }).waitFor();
        assert.equal(backend.messages[0].markdown, "First line\nSecond");
        assert.equal(await input.inputValue(), "");
        await refresh(frame);
        assert.equal(await frame.getByRole("img", { name:"Delivered to agent", exact:true }).count(), 0);
        backend.delivered = backend.messages[0].end;
        await refresh(frame);
        await frame.getByRole("img", { name:"Delivered to agent", exact:true }).waitFor();
        assert.equal(await frame.locator(".ticks.delivered path").count(), 2);
        const deliveredColor = await frame.locator(".ticks.delivered").evaluate(node => getComputedStyle(node).color);
        const savedColor = await frame.locator(".ticks.delivered").evaluate(node => getComputedStyle(node).getPropertyValue("--tick").trim());
        assert.equal(await frame.locator(".ticks.read").count(), 0);
        backend.read = backend.delivered;
        await refresh(frame);
        await frame.getByRole("img", { name:"Read by agent", exact:true }).waitFor();
        assert.equal(await frame.locator(".ticks.read path").count(), 2);
        assert.notEqual(await frame.locator(".ticks.read").evaluate(node => getComputedStyle(node).color), deliveredColor);
        assert(savedColor);
        await input.fill("From the arrow button"); await send.click();
        await frame.getByRole("img", { name:"Saved to CHAT.md", exact:true }).waitFor();
        assert.equal(backend.messages.length, 2);
        assert.equal(await frame.locator(".ticks.sent path").count(), 1);
        assert(!hostMessages.includes("ui/message"));
        assert(widgetStates.every(state => Object.keys(state).join() === "privateContent"));
        assert.deepEqual(errors, []); await page.close();
      });
      await t.test("two instances share history, preserve drafts, and do not consume user input", async () => {
        const backend = new ChatBackend(); backend.add("agent", "## Question\n\nWhich build should I test?");
        const { page, frames:[first, second], errors } = await mount(browser, backend, { width:640, count:2 });
        const secondInput = second.getByRole("textbox");
        await secondInput.fill("Keep my unfinished draft");
        await first.getByRole("textbox").fill("Use release mode"); await first.getByRole("button", { name:"Send message", exact:true }).click();
        await first.getByText("Use release mode", { exact:true }).waitFor();
        await refresh(second); await second.getByText("Use release mode", { exact:true }).waitFor();
        assert.equal(await secondInput.inputValue(), "Keep my unfinished draft");
        assert.equal(backend.delivered, 0);
        backend.delivered = backend.messages.at(-1).end;
        await refresh(first); await refresh(second);
        await first.getByRole("img", { name:"Delivered to agent" }).waitFor();
        await second.getByRole("img", { name:"Delivered to agent" }).waitFor();
        backend.add("agent", "The release build passed.");
        await refresh(first); await refresh(second);
        await second.getByText("The release build passed.", { exact:true }).waitFor();
        assert.equal(await first.locator(".message").count(), 3);
        assert.equal(await secondInput.inputValue(), "Keep my unfinished draft");
        assert.deepEqual(errors, []); await page.close();
      });
      await t.test("tool-call total and between-agent marker update without a history revision", async () => {
        const backend = new ChatBackend();
        backend.totalToolCalls = 1;
        backend.add("agent", "First progress report.");
        const { page, frames:[frame], errors } = await mount(browser, backend, { combined:true });
        const total = frame.locator("#tool-total");
        const markers = frame.locator(".tool-call-marker");
        assert.equal(await total.textContent(), "1 tool call");
        assert.equal(await markers.count(), 0);

        backend.totalToolCalls = 2;
        await refresh(frame);
        await frame.getByText("2 tool calls", { exact:true }).waitFor();
        assert.deepEqual(await markers.allTextContents(), ["1 tool call"]);
        backend.totalToolCalls = 4;
        await refresh(frame);
        await frame.getByText("4 tool calls", { exact:true }).waitFor();
        assert.equal(await total.textContent(), "4 tool calls");
        assert.deepEqual(await markers.allTextContents(), ["3 tool calls"]);

        backend.add("user", "Keep going.");
        backend.add("agent", "Second progress report.");
        await refresh(frame);
        await frame.getByText("Second progress report.", { exact:true }).waitFor();
        assert.deepEqual(await markers.allTextContents(), ["3 tool calls"]);
        const order = await frame.locator("#messages > .message, #messages > .tool-call-marker")
          .evaluateAll(nodes => nodes.map(node => node.classList.contains("tool-call-marker") ? node.textContent : node.querySelector(".markdown")?.textContent));
        assert.deepEqual(order, ["First progress report.", "3 tool calls", "Keep going.", "Second progress report."]);

        backend.totalToolCalls = 5;
        await refresh(frame);
        await frame.getByText("5 tool calls", { exact:true }).waitFor();
        assert.equal(await total.textContent(), "5 tool calls");
        assert.deepEqual(await markers.allTextContents(), ["3 tool calls", "1 tool call"]);
        assert(!backend.calls.some(call => call.name === "chat_ui_send"));
        assert.deepEqual(errors, []); await page.close();
      });
      await t.test("failed send retries the same ID, including an ambiguous saved response", async () => {
        const backend = new ChatBackend(); backend.failSends = 1;
        const { page, frames:[frame], errors } = await mount(browser, backend);
        await frame.getByRole("textbox").fill("Do not duplicate me");
        await frame.getByRole("button", { name:"Send message", exact:true }).click();
        await frame.getByRole("button", { name:"Retry", exact:true }).waitFor();
        assert.equal(await frame.getByRole("img").count(), 0);
        const firstId = backend.calls.find(call => call.name === "chat_ui_send").args.request_id;
        await frame.getByRole("button", { name:"Retry", exact:true }).click();
        await frame.getByRole("img", { name:"Saved to CHAT.md" }).waitFor();
        assert.equal(backend.calls.filter(call => call.name === "chat_ui_send")[1].args.request_id, firstId);
        assert.equal(backend.messages.length, 1);
        backend.failAfterSave = true; backend.failState = true;
        await frame.getByRole("textbox").fill("Saved even if the response was lost");
        await frame.getByRole("button", { name:"Send message", exact:true }).click();
        await frame.getByRole("button", { name:"Retry", exact:true }).waitFor();
        await frame.getByRole("button", { name:"Retry", exact:true }).click();
        await frame.getByText("Not confirmed", { exact:true }).waitFor({ state:"hidden" });
        assert.equal(backend.messages.length, 2);
        assert.deepEqual(errors, []); await page.close();
      });
      await t.test("MCP Apps bridge works before the invoking tool returns", async () => {
        const backend = new ChatBackend();
        const { page, frames:[frame], errors, hostMessages } = await mount(browser, backend, { bridge:"mcp" });
        await frame.getByRole("textbox").fill("Reply while chat_await is pending");
        await frame.getByRole("button", { name:"Send message", exact:true }).click();
        await frame.getByRole("img", { name:"Saved to CHAT.md" }).waitFor();
        assert(hostMessages.includes("tools/call"));
        assert(!hostMessages.includes("ui/message"));
        assert.equal(backend.messages[0].markdown, "Reply while chat_await is pending");
        assert.deepEqual(errors, []); await page.close();
      });
      await t.test("nested host result envelopes remain usable", async () => {
        const backend = new ChatBackend(); backend.add("agent", "Nested metadata works");
        const { page, frames:[frame], errors } = await mount(browser, backend, { nested:true });
        await frame.getByText("Nested metadata works", { exact:true }).waitFor();
        backend.toolErrorSends = 1;
        await frame.getByRole("textbox").fill("Nested send"); await frame.getByRole("button", { name:"Send message", exact:true }).click();
        await frame.getByRole("button", { name:"Retry", exact:true }).waitFor();
        assert.equal(await frame.getByRole("img").count(), 0);
        assert.equal(backend.messages.length, 1);
        await frame.getByRole("button", { name:"Retry", exact:true }).click();
        await frame.getByRole("img", { name:"Saved to CHAT.md" }).waitFor();
        assert.deepEqual(errors, []); await page.close();
      });
      await t.test("widget-state updates do not trigger chat polling", async () => {
        const backend = new ChatBackend();
        const { page, frames:[frame], errors } = await mount(browser, backend);
        await sleep(100);
        const count = backend.calls.length;
        await frame.locator("body").evaluate(() => window.dispatchEvent(new CustomEvent("openai:set_globals", {
          detail:{ globals:{ widgetState:{ privateContent:{ draft:"Local state" } }, theme:"dark" } }
        })));
        await sleep(100);
        assert.equal(backend.calls.length, count, "echoing widget state must not create a call/save/refresh loop");
        assert.equal(await frame.locator("html").getAttribute("data-theme"), "dark");
        assert.deepEqual(errors, []); await page.close();
      });
      await t.test("paged history fills gaps after many new messages", async () => {
        const backend = new ChatBackend(); backend.pageSize = 3;
        for (let i = 0; i < 5; i++) backend.add("agent", `Original ${i}`);
        const { page, frames:[frame], errors } = await mount(browser, backend);
        await frame.getByRole("button", { name:"Load earlier messages" }).click();
        await frame.getByText("Original 0", { exact:true }).waitFor();
        for (let i = 0; i < 7; i++) backend.add("agent", `New ${i}`);
        await refresh(frame); await frame.getByText("New 6", { exact:true }).waitFor();
        for (let i = 0; i < 3; i++) {
          const older = frame.getByRole("button", { name:"Load earlier messages" });
          if (!await older.isVisible()) break;
          await older.click(); await sleep(80);
        }
        await frame.getByText("New 0", { exact:true }).waitFor();
        assert.equal(await frame.locator(".message").count(), 12);
        assert.deepEqual(errors, []); await page.close();
      });
      await t.test("collapse and teardown stop polling; saved drafts survive remount", async () => {
        const backend = new ChatBackend();
        const { page, frames:[frame], widgetStates } = await mount(browser, backend);
        await frame.getByRole("textbox").fill("Draft retained");
        await frame.getByRole("button", { name:"Collapse chat", exact:true }).click();
        const calls = backend.calls.length; await sleep(2300);
        assert.equal(backend.calls.length, calls);
        const saved = widgetStates.at(-1);
        assert.equal(saved.privateContent.draft, "Draft retained");
        await frame.getByRole("button", { name:"Expand chat", exact:true }).click();
        await frame.getByRole("textbox").waitFor();
        await page.evaluate(() => document.querySelector("iframe").contentWindow.postMessage({ jsonrpc:"2.0", id:9999, method:"ui/resource-teardown", params:{} }, "*"));
        await sleep(150); const stopped = backend.calls.length; await sleep(2300);
        assert.equal(backend.calls.length, stopped);
        await page.close();
        const mounted = await mount(browser, backend, { saved:{ privateContent:{ ...saved.privateContent, collapsed:false } } });
        assert.equal(await mounted.frames[0].getByRole("textbox").inputValue(), "Draft retained");
        await mounted.page.close();
      });
      await t.test("activity ages locally without new messages or successful polling", async () => {
        const backend = new ChatBackend();
        const now = Date.UTC(2026, 8, 14, 12);
        backend.serverTime = now; backend.lastAgentCall = now;
        const { page, frames:[frame], errors } = await mount(browser, backend, { clock:now });
        await frame.locator("#presence[data-state='online']").waitFor();
        backend.failState = true;
        await page.clock.fastForward(181000);
        await frame.getByText("last seen 3 mins ago", { exact:true }).waitFor();
        assert.equal(await frame.locator(".presence-symbol path").getAttribute("d"), "M12 5.5V12l5.6 3.2");
        await page.clock.fastForward(60000);
        await frame.getByText("last seen 4 mins ago", { exact:true }).waitFor();
        await page.clock.fastForward(60000);
        await frame.locator("#presence[data-state='offline']").waitFor();
        assert.equal(await frame.locator(".presence-symbol circle").getAttribute("fill"), "#fff");
        assert.equal(await frame.locator(".presence-symbol path").count(), 2);
        assert.equal(backend.lastAgentCall, now, "UI calls must not change activity");
        assert.deepEqual(errors, []); await page.close();
      });
      await t.test("ticket warnings render between messages without agent bubbles", async () => {
        const backend = new ChatBackend();
        backend.add("user", "Continue.");
        backend.add("warning", DUPLICATE_WARNING);
        backend.add("agent", "I stopped after the rejection.");
        const { page, frames:[frame], errors } = await mount(browser, backend);
        const alert = frame.locator(".warning-banner[role='alert']");
        await alert.waitFor();
        assert.equal(await alert.locator(".markdown").textContent(), DUPLICATE_WARNING);
        assert.equal(await alert.evaluate(node => node.className), "warning-banner");
        assert.equal(await frame.locator(".message.agent").count(), 1);
        assert.equal(await alert.evaluate(node => getComputedStyle(node).backgroundColor), "rgb(255, 243, 196)");
        assert.deepEqual(errors, []);
        await page.close();
      });
      for (const bridge of ["legacy", "mcp"]) {
        await t.test(`setup-only panel preserves its draft across status updates (${bridge})`, async () => {
          const backend = new ChatBackend(); backend.lastAgentCall = Date.now();
          const { page, frames:[frame], errors, hostMessages } = await mount(browser, backend, { combined:true, bridge });
          await frame.locator("#chat").waitFor();
          assert.equal(await frame.locator("#chat").count(), 1);
          await frame.getByRole("textbox", { name:"Message the agent" }).fill("Keep this draft");
          await frame.locator("#draft").evaluate(node => { node.dataset.original = "yes"; });
          await frame.getByRole("button", { name:"Check for updates", exact:true }).click();
          await frame.getByRole("button", { name:"Check for updates", exact:true }).waitFor();
          assert.equal(await frame.locator("#draft").inputValue(), "Keep this draft");
          assert.equal(await frame.locator("#draft").getAttribute("data-original"), "yes");
          await frame.getByRole("button", { name:"Send message", exact:true }).click();
          await frame.getByRole("img", { name:"Saved to CHAT.md", exact:true }).waitFor();
          backend.delivered = backend.messages[0].end;
          await refresh(frame);
          await frame.getByRole("img", { name:"Delivered to agent", exact:true }).waitFor();
          backend.read = backend.delivered;
          await refresh(frame);
          await frame.getByRole("img", { name:"Read by agent", exact:true }).waitFor();
          backend.add("agent", "Answer delivered into the existing panel.");
          await refresh(frame);
          await frame.getByText("Answer delivered into the existing panel.", { exact:true }).waitFor();
          assert.equal(await frame.locator("#chat").count(), 1);
          assert(!hostMessages.includes("ui/message"));
          assert.deepEqual(errors, []); await page.close();
        });
      }
      await t.test("setup picker uses private actions and activates the existing panel", async () => {
        const backend = new ChatBackend(); backend.setup = setupPayload(false);
        const { page, frames:[frame], errors } = await mount(browser, backend, { combined:true });
        assert(await frame.locator("#draft").isDisabled());
        assert.equal(backend.calls.filter(call => call.name === "chat_ui_state").length, 0);
        await frame.getByRole("button", { name:/Chat without a project Use/ }).click();
        await frame.locator("#draft:enabled").waitFor();
        await frame.getByRole("textbox", { name:"Message the agent" }).fill("Scratch chat works");
        await frame.getByRole("button", { name:"Send message", exact:true }).click();
        await frame.getByRole("img", { name:"Saved to CHAT.md" }).waitFor();
        assert(backend.calls.some(call => call.name === "setup_ui_select_project" && call.args.withoutProject));
        assert(!backend.calls.some(call => ["set_project_root", "list_projects"].includes(call.name)));
        assert.equal(await frame.locator("#chat").count(), 1);
        assert.equal(backend.lastAgentCall, null);
        assert.deepEqual(errors, []); await page.close();
      });
      await t.test("historical setup events cannot re-enable a chat disabled by live status", async () => {
        const backend = new ChatBackend();
        const { page, frames:[frame], errors } = await mount(browser, backend, { combined:true });
        await frame.locator("#draft").fill("Preserved when disabled");
        backend.chatEnabled = false;
        await frame.getByRole("button", { name:"Check for updates", exact:true }).click();
        await frame.locator("#markdown-chat-host").waitFor({ state:"hidden" });
        await page.evaluate(initial => document.querySelector("iframe").contentWindow.postMessage({
          jsonrpc:"2.0", method:"ui/notifications/tool-result", params:initial
        }, "*"), { structuredContent:setupPayload(), _meta:{ [ENABLED]:true } });
        await sleep(100);
        assert.equal(await frame.locator("#markdown-chat-host").isVisible(), false);
        backend.chatEnabled = true;
        await frame.getByRole("button", { name:"Check for updates", exact:true }).click();
        await frame.locator("#draft:enabled").waitFor();
        assert.equal(await frame.locator("#draft").inputValue(), "Preserved when disabled");
        assert.equal(await frame.locator("#chat").count(), 1);
        assert.deepEqual(errors, []); await page.close();
      });
      for (const theme of ["light", "dark"]) {
        await t.test(`${theme} setup renders all three receipts and activity icons`, async () => {
          for (const [state, minutes] of [["online", 2], ["away", 4], ["offline", 6]]) {
            const backend = new ChatBackend();
            backend.lastAgentCall = Date.now() - minutes * 60000;
            backend.read = backend.add("user", "Use the existing worktree.").end;
            backend.delivered = backend.add("user", "Also check the mobile layout.").end;
            backend.add("user", "Do not commit these changes yet.");
            const { page, frames:[frame], errors } = await mount(browser, backend, { combined:true, width:390, theme, scale:2 });
            await page.locator("iframe").evaluate(node => { node.style.height = "1100px"; });
            await frame.locator(`#presence[data-state='${state}']`).waitFor();
            for (const receipt of ["Saved to CHAT.md", "Delivered to agent", "Read by agent"]) await frame.getByRole("img", { name:receipt, exact:true }).waitFor();
            const colors = await frame.locator(".ticks").evaluateAll(nodes => nodes.map(node => getComputedStyle(node).color));
            assert.notEqual(colors[0], colors[1]); assert.equal(colors[1], colors[2]);
            assert.equal(await frame.locator(".ticks.sent path").count(), 1);
            assert.equal(await frame.locator(".ticks.delivered path").count(), 2);
            assert.equal(await frame.locator(".ticks.read path").count(), 2);
            assert.equal(await frame.locator(".presence-symbol").evaluate(node => node.getBoundingClientRect().width), 14);
            assert.equal(await frame.locator("html").evaluate(node => node.scrollWidth > innerWidth), false);
            mkdirSync(new URL("../target/chat-v2-previews/", import.meta.url), { recursive:true });
            const stem = `../target/chat-v2-previews/${engineName.toLowerCase()}-${theme}-${state}`;
            await frame.locator("#chat").screenshot({ path:new URL(`${stem}.png`, import.meta.url).pathname });
            if (state === "online") await frame.locator("body").screenshot({ path:new URL(`${stem}-setup.png`, import.meta.url).pathname });
            assert.deepEqual(errors, []); await page.close();
          }
        });
        await t.test(`${theme} mobile layout and safe Markdown`, async () => {
          const backend = new ChatBackend();
          backend.add("agent", "## Build complete\n\n**All tests passed.** Should I commit?\n\n```rust\n" + "long_identifier_".repeat(18) + "\n```\n\n<img src=x onerror=alert(1)>\n[Unsafe](javascript:alert(1))\n[Docs](https://example.com/docs)");
          const message = backend.add("user", "Commit the implementation.\nDo not create a release yet."); backend.delivered = message.end; backend.read = message.end;
          backend.add("agent", "I will commit and verify CI, without releasing.");
          backend.add("user", "Thanks.");
          const { page, frames:[frame], errors } = await mount(browser, backend, { width:370, theme });
          await frame.getByText("I will commit and verify CI, without releasing.", { exact:true }).waitFor();
          assert.equal(await frame.locator(".markdown img").count(), 0);
          assert.equal(await frame.locator("a[href^='javascript:']").count(), 0);
          assert.equal(await frame.locator(".markdown strong").count(), 1);
          assert.equal(await frame.locator(".markdown pre code").count(), 1);
          assert.equal(await frame.locator(".markdown a").count(), 1);
          const layout = await frame.locator("body").evaluate(() => ({
            overflow:document.documentElement.scrollWidth > innerWidth,
            input:getComputedStyle(document.querySelector("textarea")).fontSize,
            ticks:[...document.querySelectorAll(".ticks")].map(node => getComputedStyle(node).color)
          }));
          assert.equal(layout.overflow, false); assert.equal(layout.input, "16px");
          assert.notEqual(layout.ticks[0], layout.ticks[1]);
          mkdirSync(new URL("../target/chat-widget-previews/", import.meta.url), { recursive:true });
          await frame.locator("#chat").screenshot({ path:new URL(`../target/chat-widget-previews/${engineName.toLowerCase()}-${theme}.png`, import.meta.url).pathname });
          assert.deepEqual(errors, []); await page.close();
        });
      }
    } finally { await browser.close(); }
  });
}

for (const [engineName, engine] of [["Chromium", chromium], ["WebKit", webkit]]) {
  test(`${engineName}: timestamp UI`, { timeout:120000 }, async t => {
    const browser = await engine.launch();
    try {
      for (const [width, theme, combined] of [[390, "light", false], [800, "dark", false], [390, "dark", true], [800, "light", true]]) {
        await t.test(`warning timestamps stay inside the bottom-right corner (${width}, ${theme}, combined=${combined})`, async () => {
          const backend = new ChatBackend();
          const now = Date.parse("2026-09-14T22:05:00Z");
          const timestamp = Date.parse("2026-09-14T22:03:00Z");
          backend.add("agent", "Continuing the task.").created_at_ms = timestamp;
          backend.add("warning", DUPLICATE_WARNING).created_at_ms = timestamp;
          const { page, frames:[frame], errors } = await mount(browser, backend, { width, theme, combined, timezone:"Europe/Zurich", clock:now });
          const alert = frame.locator(".warning-banner[role='alert']");
          await alert.waitFor();
          const stamp = alert.locator(".receipt > time.message-time");
          assert.equal(await stamp.count(), 1);
          assert.equal(await stamp.textContent(), "00:03");
          assert.equal(await stamp.textContent(), await frame.locator(".message.agent time").textContent());
          assert.equal(await stamp.getAttribute("datetime"), new Date(timestamp).toISOString());
          assert.equal(await stamp.getAttribute("title"), await frame.locator(".message.agent time").getAttribute("title"));
          assert.equal(await alert.locator(".author, .ticks, .retry").count(), 0);
          const geometry = await alert.evaluate(node => {
            const box = node.getBoundingClientRect(), time = node.querySelector("time"), bounds = time.getBoundingClientRect();
            const body = node.querySelector(".markdown").getBoundingClientRect();
            return { font:getComputedStyle(time).fontSize, inside:bounds.left >= box.left && bounds.right <= box.right && bounds.bottom <= box.bottom,
              belowText:bounds.top >= body.bottom, atBottom:box.bottom - bounds.bottom < 20, rightAligned:box.right - bounds.right < 20 };
          });
          assert.equal(geometry.font, "11px");
          assert(geometry.inside && geometry.belowText && geometry.atBottom && geometry.rightAligned, JSON.stringify(geometry));
          assert.equal(await frame.locator("html").evaluate(node => node.scrollWidth > innerWidth), false);
          mkdirSync(new URL("../target/chat-warning-previews/", import.meta.url), { recursive:true });
          const name = `${engineName.toLowerCase()}-${width}-${theme}-${combined ? "setup" : "chat"}.png`;
          await frame.locator("#chat").screenshot({ path:new URL(`../target/chat-warning-previews/${name}`, import.meta.url).pathname });
          assert.deepEqual(errors, []); await page.close();
        });
      }
      for (const [timezone, theme, expected] of [
        ["Europe/Paris", "light", ["2026-09-14 23:58", "00:03", "2026-09-13 22:30", "2026-09-14 01:02"]],
        ["America/Los_Angeles", "dark", ["14:58", "15:03", "2026-09-13 13:30", "2026-09-13 16:02"]],
        ["Asia/Kolkata", "light", ["03:28", "03:33", "2026-09-14 02:00", "2026-09-14 04:32"]]
      ]) {
        await t.test(`local time, previous calendar dates, and placement in ${timezone}`, async () => {
          const backend = new ChatBackend();
          const fixtures = [
            ["user", "Use the existing worktree.", "2026-09-14T21:58:00Z"],
            ["agent", "I am using the existing worktree.", "2026-09-14T22:03:00Z"],
            ["user", "Check the mobile layout too.", "2026-09-13T20:30:00Z"],
            ["agent", "The layout checks passed.", "2026-09-13T23:02:00Z"]
          ];
          for (const [role, body, time] of fixtures) backend.add(role, body).created_at_ms = Date.parse(time);
          backend.delivered = backend.read = backend.end;
          const now = Date.parse("2026-09-14T22:05:00Z");
          backend.lastAgentCall = now; backend.serverTime = now;
          const { page, frames:[frame], errors } = await mount(browser, backend, { combined:true, timezone, theme, scale:2, clock:now });
          await page.locator("iframe").evaluate(node => { node.style.height = "1100px"; });
          await frame.locator(".message time").first().waitFor();
          assert.deepEqual(await frame.locator(".message time").allTextContents(), expected);
          assert.deepEqual(await frame.locator(".message time").evaluateAll(nodes => nodes.map(node => node.dateTime)), fixtures.map(([, , time]) => new Date(time).toISOString()));
          const geometry = await frame.locator(".message").evaluateAll(nodes => nodes.map(node => {
            const box = node.getBoundingClientRect(), time = node.querySelector("time"), tick = node.querySelector(".ticks");
            const bounds = time.getBoundingClientRect();
            return { role:node.classList.contains("user") ? "user" : "agent", font:getComputedStyle(time).fontSize,
              inside:bounds.bottom <= box.bottom && bounds.right <= box.right,
              atBottom:box.bottom - bounds.bottom < 20,
              beforeTicks:!tick || bounds.right <= tick.getBoundingClientRect().left,
              rightAligned:Boolean(tick) || box.right - bounds.right < 20 };
          }));
          for (const item of geometry) {
            assert.equal(item.font, "11px");
            assert(item.inside && item.atBottom && item.beforeTicks && item.rightAligned, JSON.stringify(item));
          }
          assert.equal(await frame.locator(".message.agent .ticks").count(), 0);
          assert.equal(await frame.locator("html").evaluate(node => node.scrollWidth > innerWidth), false);
          mkdirSync(new URL("../target/chat-time-previews/", import.meta.url), { recursive:true });
          const name = `${engineName.toLowerCase()}-${timezone.split("/")[1].toLowerCase()}-${theme}.png`;
          await frame.locator("#chat").screenshot({ path:new URL(`../target/chat-time-previews/${name}`, import.meta.url).pathname });
          assert.deepEqual(errors, []); await page.close();
        });
      }
      await t.test("dates update at local midnight even when polling fails", async () => {
        const backend = new ChatBackend();
        const now = Date.parse("2026-09-14T21:59:00Z");
        backend.add("user", "Just before midnight.").created_at_ms = now - 30000;
        backend.add("agent", "Still working.").created_at_ms = now;
        backend.add("warning", "Duplicate agent detected.").created_at_ms = now;
        const { page, frames:[frame], errors } = await mount(browser, backend, { combined:true, timezone:"Europe/Paris", clock:now });
        await frame.locator(".message time").first().waitFor();
        assert.deepEqual(await frame.locator(".message time").allTextContents(), ["23:58", "23:59"]);
        assert.deepEqual(await frame.locator(".warning-banner time").allTextContents(), ["23:59"]);
        backend.failState = true;
        await page.clock.fastForward(120000);
        assert.deepEqual(await frame.locator(".message time").allTextContents(), ["2026-09-14 23:58", "2026-09-14 23:59"]);
        assert.deepEqual(await frame.locator(".warning-banner time").allTextContents(), ["2026-09-14 23:59"]);
        assert.equal(await frame.locator(".ticks.sent").count(), 1);
        assert.deepEqual(errors, []); await page.close();
      });
      await t.test("retry uses the server timestamp even without a subsequent history response", async () => {
        const backend = new ChatBackend();
        const { page, frames:[frame], errors } = await mount(browser, backend);
        backend.failAfterSave = true; backend.failState = true;
        await frame.getByRole("textbox").fill("Keep my original timestamp.");
        await frame.getByRole("button", { name:"Send message", exact:true }).click();
        await frame.getByRole("button", { name:"Retry", exact:true }).waitFor();
        const timestamp = backend.messages[0].created_at_ms;
        await frame.getByRole("button", { name:"Retry", exact:true }).click();
        await frame.getByRole("img", { name:"Saved to CHAT.md", exact:true }).waitFor();
        assert.equal(await frame.locator(".message time").getAttribute("datetime"), new Date(timestamp).toISOString());
        assert.equal(backend.messages.length, 1);
        assert.deepEqual(errors, []); await page.close();
      });
      await t.test("unknown or invalid legacy times are not fabricated", async () => {
        const backend = new ChatBackend();
        backend.add("user", "Unknown old user time").created_at_ms = null;
        delete backend.add("agent", "Unknown old agent time").created_at_ms;
        backend.add("agent", "Invalid timestamp").created_at_ms = 8640000000000001;
        backend.add("warning", "Unknown old warning time").created_at_ms = null;
        backend.add("warning", "Invalid warning timestamp").created_at_ms = 8640000000000001;
        backend.add("user", "Known epoch").created_at_ms = 0;
        const { page, frames:[frame], errors } = await mount(browser, backend, { timezone:"UTC" });
        await frame.locator(".message time").waitFor();
        assert.deepEqual(await frame.locator(".message time").allTextContents(), ["1970-01-01 00:00"]);
        assert.equal(await frame.locator(".warning-banner time").count(), 0);
        assert.equal(await frame.getByText("Invalid Date", { exact:true }).count(), 0);
        assert.deepEqual(errors, []); await page.close();
      });
    } finally { await browser.close(); }
  });
}
