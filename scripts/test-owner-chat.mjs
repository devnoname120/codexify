import assert from "node:assert/strict";
import { readFileSync, mkdirSync } from "node:fs";
import { createRequire } from "node:module";
import test from "node:test";
import { chatHtml } from "./chat-widget-source.mjs";

const { chromium, webkit } = createRequire(import.meta.url)("playwright");
const META = "io.github.devnoname120/codexify/markdown-chat";
const section = (text, opening, closing) => text.split(opening)[1].split(closing)[0];
const style = section(chatHtml, "<style>", "</style>")
  .replace(':root:not([data-theme="light"])', ':host(:not([data-theme="light"]))')
  .replace(':root[data-theme="dark"]', ':host([data-theme="dark"])')
  .replaceAll(":root", ":host")
  .replace("body {", ":host { display:block;");
const widgetBody = section(chatHtml, "<body>", "<script>");
const widgetScript = section(chatHtml, "<script>", "</script>");
const ownerHtml = readFileSync(new URL("../src/owner_chat.html", import.meta.url), "utf8")
  .replace("<!-- CHAT_TEMPLATE -->", `<template id="chat-template"><style>${style}\n#chat { max-width:none; height:100%; border:0; border-radius:0; display:flex; flex-direction:column; } #panel { display:flex; flex:1; min-height:0; flex-direction:column; } #panel[hidden] { display:none; } #messages { flex:1; max-height:none; min-height:0; }\n</style>${widgetBody}</template>`)
  .replace("/* CHAT_SCRIPT */", widgetScript);

for (const [engineName, engine] of [["Chromium", chromium], ["WebKit", webkit]]) {
test(`${engineName}: standalone owner view restores its URL selection`, { timeout:30000 }, async () => {
  const browser = await engine.launch();
  try {
    const page = await browser.newPage();
    page.setDefaultTimeout(5000);
    const errors = [];
    const copied = [];
    const markdownRequests = [];
    await page.exposeFunction("recordClipboard", text => copied.push(text));
    await page.addInitScript(() => {
      window.__writeTestClipboard = async text => {
        await window.recordClipboard(text);
        window.__lastCopied = text;
      };
      Object.defineProperty(navigator, "clipboard", {
        configurable:true,
        value:{ writeText:text => window.__writeTestClipboard(text) }
      });
    });
    page.on("pageerror", error => errors.push(error.message));
    const now = Date.now();
    const rawSource = "**Rendered**  \n\n```js\nconst x = 1;\n```";
    const wholeTranscript = `## Agent\n\nOlder server-only entry\n\n## Agent\n\n${rawSource}`;
    const chats = [
      { id:"a".repeat(64), title:"Fix tests", workspace:"project-a", lastEntryEnd:200, lastEntryAtMs:now, lastAgentCallAtMs:now, totalToolCalls:3 },
      { id:"b".repeat(64), title:"Review files", workspace:"project-b", lastEntryEnd:100, lastEntryAtMs:now - 60000, lastAgentCallAtMs:now - 240000, totalToolCalls:1 },
      { id:"c".repeat(64), title:"Old task", workspace:"project-c", lastEntryEnd:75, lastEntryAtMs:now - 120000, lastAgentCallAtMs:null, totalToolCalls:0 }
    ];
    const messages = new Map(chats.map(chat => [chat.id, []]));
    messages.get(chats[0].id).push({ id:"raw-one", role:"agent", markdown:rawSource, start:1, end:100, created_at_ms:now, tool_call_count:3 });
    const seenRequests = [];
    let failPolling = false;
    await page.route("http://127.0.0.1:43210/**", async route => {
      const request = route.request();
      const url = new URL(request.url());
      if (url.pathname === "/") return route.fulfill({ contentType:"text/html", body:ownerHtml, headers:{
        "content-security-policy":"default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src blob: data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
      } });
      seenRequests.push(request.headers().authorization);
      if (failPolling) return route.fulfill({ status:503, body:"Preview connection unavailable" });
      if (url.pathname === "/api/chats") return route.fulfill({ json:{ chats, serverTimeMs:Date.now() } });
      const id = url.pathname.split("/")[3];
      if (url.pathname.endsWith("/markdown")) {
        markdownRequests.push(url.pathname);
        return route.fulfill({ contentType:"text/markdown; charset=utf-8", body:wholeTranscript });
      }
      if (url.pathname.endsWith("/send")) {
        const body = request.postDataJSON();
        const end = chats.find(chat => chat.id === id).lastEntryEnd += 100;
        chats.find(chat => chat.id === id).agentWaitingUntilMs = null;
        const tool_call_count = chats.find(chat => chat.id === id).totalToolCalls;
        messages.get(id).push({ id:body.request_id, role:"user", markdown:body.message, start:end - 100, end, created_at_ms:Date.now(), tool_call_count });
        return route.fulfill({ json:{ _meta:{ [META]:{ sent:{ id:body.request_id, end, created_at_ms:Date.now(), tool_call_count } } } } });
      }
      if (url.pathname.startsWith("/api/chats/")) {
        const rows = messages.get(id);
        return route.fulfill({ json:{ _meta:{ [META]:{
          chat_file:`/private/${id}/CHAT.md`, revision:String(chats.find(chat => chat.id === id).lastEntryEnd),
          delivered_through:0, read_through:0, last_agent_call_at_ms:chats.find(chat => chat.id === id).lastAgentCallAtMs,
          agent_waiting_until_ms:chats.find(chat => chat.id === id).agentWaitingUntilMs ?? null,
          total_tool_calls:chats.find(chat => chat.id === id).totalToolCalls, server_time_ms:Date.now(),
          messages:rows, has_more:false, before:rows[0]?.start ?? null, unchanged:false
        } } } });
      }
      return route.fulfill({ status:404 });
    });
    await page.goto("http://127.0.0.1:43210/#test-token");
    await page.getByRole("button", { name:/Fix tests/ }).waitFor();
    assert.equal(new URL(page.url()).hash, "#test-token", "the access token must remain in the address bar");
    assert.deepEqual(await page.locator(".conversation-title").allTextContents(), ["Fix tests", "Review files", "Old task"]);
    assert.deepEqual(await page.locator(".presence").evaluateAll(nodes => nodes.map(node => [...node.classList].at(-1))), ["online", "away", "offline"]);
    assert.equal(await page.locator(".unread:not([hidden])").count(), 3);
    for (const size of await page.locator(".presence").evaluateAll(nodes => nodes.map(node => node.getBoundingClientRect().width))) assert(size <= 14);
    assert(!((await page.locator(".conversation-detail").allTextContents()).some(text => text.includes("No messages"))));
    await page.getByRole("button", { name:/Fix tests/ }).click();
    chats[0].agentWaitingUntilMs = Date.now() + 120000;
    await page.locator(".conversation:first-child .presence.waiting").waitFor();
    await page.locator('#chat-host #presence[data-state="waiting"]').waitFor();
    assert.equal(await page.locator(".conversation:first-child .presence circle").count(), 3);
    assert.equal(await page.locator(".conversation:first-child .presence path").getAttribute("fill"), "currentColor");
    assert.equal(new URL(page.url()).searchParams.get("chat"), chats[0].id);
    assert.equal(new URL(page.url()).hash, "#test-token");
    const chatHost = page.locator("#chat-host");
    await chatHost.getByRole("button", { name:"Copy message Markdown", exact:true }).first().click();
    await page.waitForFunction(expected => window.__lastCopied === expected, rawSource);
    assert.equal(copied.at(-1), rawSource);
    await chatHost.getByRole("button", { name:"Copy whole chat as Markdown", exact:true }).click();
    await page.waitForFunction(expected => window.__lastCopied === expected, wholeTranscript);
    assert.equal(markdownRequests.at(-1), `/api/chats/${chats[0].id}/markdown`);
    assert.equal(copied.at(-1), wholeTranscript);
    await page.evaluate(() => {
      navigator.clipboard.writeText = async () => { throw new Error("Clipboard denied"); };
    });
    await chatHost.getByRole("button", { name:"Copy message Markdown", exact:true }).first().click();
    await chatHost.getByText("Could not copy message Markdown.", { exact:true }).waitFor();
    await page.evaluate(() => {
      navigator.clipboard.writeText = text => window.__writeTestClipboard(text);
    });
    const chat = page.locator("#chat-host").locator("div").first().locator("#draft");
    await chat.fill("Please run the test suite");
    await page.locator("#chat-host #send:enabled").waitFor();
    await chat.press("Enter");
    await page.locator("#chat-host").getByText("Please run the test suite").waitFor();
    await page.locator('#chat-host #presence[data-state="online"]').waitFor();
    await page.locator(".conversation:first-child .presence.online").waitFor();
    if (process.env.CODEXIFY_WORKSPACE_SCREENSHOTS) {
      mkdirSync(process.env.CODEXIFY_WORKSPACE_SCREENSHOTS, { recursive:true });
      await page.screenshot({ path:`${process.env.CODEXIFY_WORKSPACE_SCREENSHOTS}/${engineName.toLowerCase()}-sidebar.png`, fullPage:true });
    }
    await page.getByRole("button", { name:/Review files/ }).click();
    assert.equal(new URL(page.url()).searchParams.get("chat"), chats[1].id);
    await page.locator("#chat-host").getByText("Please run the test suite").waitFor({ state:"hidden" });
    await page.goBack();
    await page.locator("#chat-host").getByText("Please run the test suite").waitFor();
    assert.equal(new URL(page.url()).searchParams.get("chat"), chats[0].id);
    await page.goBack();
    await page.locator("#placeholder").waitFor();
    assert.equal(new URL(page.url()).searchParams.get("chat"), null);
    await page.goForward();
    await page.locator("#chat-host").getByText("Please run the test suite").waitFor();
    await page.goForward();
    await page.locator("#chat-host").getByText("Please run the test suite").waitFor({ state:"hidden" });
    await page.reload();
    await page.locator('.conversation[aria-current="true"] .conversation-title').getByText("Review files").waitFor();
    await page.locator("#chat-host #draft").waitFor();
    assert.equal(new URL(page.url()).searchParams.get("chat"), chats[1].id);
    assert.equal(new URL(page.url()).hash, "#test-token");
    await page.evaluate(() => sessionStorage.clear());
    await page.goto(`http://127.0.0.1:43210/?chat=${chats[0].id}#test-token`);
    await page.locator("#chat-host").getByText("Please run the test suite").waitFor();
    assert.equal(new URL(page.url()).searchParams.get("chat"), chats[0].id);
    assert.equal(new URL(page.url()).hash, "#test-token");
    assert(seenRequests.length > 0 && seenRequests.every(value => value === "Bearer test-token"));
    const clock = Date.now();
    await page.clock.install({ time:clock });
    chats[1].agentWaitingUntilMs = clock + 20000;
    await page.evaluate(() => window.dispatchEvent(new Event("focus")));
    await page.locator(".conversation:nth-child(2) .presence.waiting").waitFor();
    failPolling = true;
    await page.clock.fastForward(20000);
    await page.locator(".conversation:nth-child(2) .presence.away").waitFor();
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});
}
