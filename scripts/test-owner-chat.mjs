import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import test from "node:test";
import { chatHtml } from "./chat-widget-source.mjs";

const { chromium } = createRequire(import.meta.url)("playwright");
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

test("standalone owner view lists and switches live conversations using the shared widget", { timeout:30000 }, async () => {
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", error => errors.push(error.message));
    const now = Date.now();
    const chats = [
      { id:"a".repeat(64), title:"Fix tests", workspace:"project-a", last_entry_end:200, last_entry_at_ms:now, last_agent_call_at_ms:now, total_tool_calls:3 },
      { id:"b".repeat(64), title:"Review files", workspace:"project-b", last_entry_end:100, last_entry_at_ms:now - 60000, last_agent_call_at_ms:now - 300000, total_tool_calls:1 },
      { id:"c".repeat(64), title:"Old task", workspace:"project-c", last_entry_end:75, last_entry_at_ms:now - 120000, last_agent_call_at_ms:null, total_tool_calls:0 }
    ];
    const messages = new Map(chats.map(chat => [chat.id, []]));
    const seenRequests = [];
    await page.route("http://127.0.0.1:43210/**", async route => {
      const request = route.request();
      const url = new URL(request.url());
      if (url.pathname === "/") return route.fulfill({ contentType:"text/html", body:ownerHtml, headers:{
        "content-security-policy":"default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src blob: data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
      } });
      seenRequests.push(request.headers().authorization);
      if (url.pathname === "/api/chats") return route.fulfill({ json:{ chats, serverTimeMs:Date.now() } });
      const id = url.pathname.split("/")[3];
      if (url.pathname.endsWith("/send")) {
        const body = request.postDataJSON();
        const end = chats.find(chat => chat.id === id).last_entry_end += 100;
        messages.get(id).push({ id:body.request_id, role:"user", markdown:body.message, start:end - 100, end, created_at_ms:Date.now(), tool_call_count:null });
        return route.fulfill({ json:{ _meta:{ [META]:{ sent:{ id:body.request_id, end, created_at_ms:Date.now() } } } } });
      }
      if (url.pathname.startsWith("/api/chats/")) {
        const rows = messages.get(id);
        return route.fulfill({ json:{ _meta:{ [META]:{
          chat_file:`/private/${id}/CHAT.md`, revision:String(chats.find(chat => chat.id === id).last_entry_end),
          delivered_through:0, read_through:0, last_agent_call_at_ms:chats.find(chat => chat.id === id).last_agent_call_at_ms,
          total_tool_calls:chats.find(chat => chat.id === id).total_tool_calls, server_time_ms:Date.now(),
          messages:rows, has_more:false, before:rows[0]?.start ?? null, unchanged:false
        } } } });
      }
      return route.fulfill({ status:404 });
    });
    await page.goto("http://127.0.0.1:43210/#test-token");
    await page.getByRole("button", { name:/Fix tests/ }).waitFor();
    assert.deepEqual(await page.locator(".conversation-title").allTextContents(), ["Fix tests", "Review files", "Old task"]);
    assert.deepEqual(await page.locator(".presence").evaluateAll(nodes => nodes.map(node => [...node.classList].at(-1))), ["online", "away", "offline"]);
    assert.equal(await page.locator(".unread:not([hidden])").count(), 3);
    await page.getByRole("button", { name:/Fix tests/ }).click();
    const chat = page.locator("#chat-host").locator("div").first().locator("#draft");
    await chat.fill("Please run the test suite");
    await chat.press("Enter");
    await page.locator("#chat-host").getByText("Please run the test suite").waitFor();
    await page.getByRole("button", { name:/Review files/ }).click();
    await page.locator("#chat-host").getByText("Please run the test suite").waitFor({ state:"hidden" });
    await page.getByRole("button", { name:/Fix tests/ }).click();
    await page.locator("#chat-host").getByText("Please run the test suite").waitFor();
    assert(seenRequests.length > 0 && seenRequests.every(value => value === "Bearer test-token"));
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});
