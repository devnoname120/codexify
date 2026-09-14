import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { createRequire } from "node:module";
import test from "node:test";
import vm from "node:vm";
import { chatHtml as html, setupChatHtml } from "./chat-widget-source.mjs";

const { chromium, webkit } = createRequire(import.meta.url)("playwright");
const META = "io.github.devnoname120/codexify/markdown-chat";
const ENABLED = "io.github.devnoname120/codexify/markdown-chat-enabled";
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

const presenceDetails = vm.runInNewContext("(" + html.slice(html.indexOf("function presenceDetails("), html.indexOf("  function updatePresence(")) + ")");
test("presence: exact four/ten-minute boundaries, unknown activity, and clock skew", () => {
  for (const [age, state, label] of [
    [0, "online", "online"], [239999, "online", "online"],
    [240000, "away", "last seen 4 mins ago"], [299999, "away", "last seen 4 mins ago"],
    [300000, "away", "last seen 5 mins ago"], [599999, "away", "last seen 9 mins ago"],
    [600000, "offline", "offline"], [3600000, "offline", "offline"], [-5000, "online", "online"]
  ]) {
    const actual = presenceDetails(1000000, 1000000 + age);
    assert.equal(actual.state, state, `age ${age}`);
    assert.equal(actual.label, label, `age ${age}`);
    if (actual.delay !== null) assert(actual.delay > 0);
  }
  for (const missing of [null, undefined, NaN]) assert.equal(presenceDetails(missing, 1000000).state, "offline");
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
  add(role, markdown, id = `fixture-${this.messages.length}`) {
    const start = this.end;
    this.end += markdown.length + 150;
    const message = { id, role, markdown, start, end:this.end };
    this.messages.push(message); this.revision++;
    return message;
  }
  async call(name, args) {
    this.calls.push({ name, args });
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
      else message = this.add("user", args.message, args.request_id);
      if (this.failAfterSave) { this.failAfterSave = false; throw new Error("Response lost after save"); }
      return { content:[{ type:"text", text:"Message saved." }], _meta:{ [META]:{ sent:{ id:message.id, end:message.end } } } };
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
        last_agent_call_at_ms:this.lastAgentCall, server_time_ms:this.serverTime ?? Date.now(), messages,
        has_more:all.length > messages.length && !unchanged,
        before:messages[0]?.start ?? null, unchanged
      } }
    };
  }
}

async function mount(browser, backend, { width = 390, theme = "light", count = 1, bridge = "legacy", nested = false, saved = {}, combined = false, clock = null, scale = 1 } = {}) {
  const page = await browser.newPage({ viewport:{ width, height:1400 }, colorScheme:theme, deviceScaleFactor:scale });
  page.setDefaultTimeout(8000);
  if (clock !== null) await page.clock.install({ time:clock });
  const errors = [], hostMessages = [], widgetStates = [];
  page.on("pageerror", error => errors.push(error.message));
  await page.exposeFunction("mockTool", async (name, args) => {
    const result = await backend.call(name, args);
    return nested ? { mcp_tool_result:result } : result;
  });
  await page.exposeFunction("saveWidget", state => { widgetStates.push(state); });
  await page.exposeFunction("hostMessage", message => { hostMessages.push(message); });
  await page.route("https://codexify-widget.test/", route => route.fulfill({
    contentType:"text/html", body:'<!doctype html><html><body style="margin:0"></body></html>'
  }));
  await page.goto("https://codexify-widget.test/");
  await page.evaluate(({ html, count, bridge, theme, saved, initial }) => {
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
        if (message.method === "ui/initialize") result = { hostContext:{ theme }, protocolVersion:"2026-01-26", hostCapabilities:{} };
        else if (message.method === "tools/call") result = await window.mockTool(message.params.name, message.params.arguments);
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
  }, { html:combined ? setupChatHtml : html, count, bridge, theme, saved, initial:combined ? { structuredContent:backend.setup, _meta:{ [ENABLED]:backend.chatEnabled } } : null });
  const frames = Array.from({ length:count }, (_, i) => page.frameLocator(`#widget-${i}`));
  await frames[0].getByText("Loading this conversation...", { exact:true }).waitFor({ state:"hidden" });
  return { page, frames, errors, hostMessages, widgetStates };
}

async function refresh(frame) {
  await frame.locator("body").evaluate(() => window.dispatchEvent(new Event("focus")));
}

for (const [engineName, engine] of [["Chromium", chromium], ["WebKit", webkit]]) {
  test(`${engineName}: Markdown chat composer, synchronization and receipts`, { timeout:180000 }, async t => {
    const browser = await engine.launch();
    try {
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
        await page.clock.fastForward(241000);
        await frame.getByText("last seen 4 mins ago", { exact:true }).waitFor();
        assert.equal(await frame.locator(".presence-symbol path").getAttribute("d"), "M12 5.5V12l5.6 3.2");
        await page.clock.fastForward(60000);
        await frame.getByText("last seen 5 mins ago", { exact:true }).waitFor();
        await page.clock.fastForward(300000);
        await frame.locator("#presence[data-state='offline']").waitFor();
        assert.equal(await frame.locator(".presence-symbol circle").getAttribute("fill"), "#fff");
        assert.equal(await frame.locator(".presence-symbol path").count(), 2);
        assert.equal(backend.lastAgentCall, now, "UI calls must not change activity");
        assert.deepEqual(errors, []); await page.close();
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
          for (const [state, minutes] of [["online", 3], ["away", 6], ["offline", 11]]) {
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
