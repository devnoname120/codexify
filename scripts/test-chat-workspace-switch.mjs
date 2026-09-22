import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdirSync } from "node:fs";
import test from "node:test";
import { chatHtml } from "./chat-widget-source.mjs";
const { chromium, webkit } = createRequire(import.meta.url)("playwright");
const META = "io.github.devnoname120/codexify/markdown-chat";
const part = (start, end) => chatHtml.split(start)[1].split(end)[0];
const style = part("<style>", "</style>")
  .replaceAll(':root:not([data-theme="light"])', ':host(:not([data-theme="light"]))')
  .replaceAll(':root[data-theme="dark"]', ':host([data-theme="dark"])')
  .replaceAll(":root", ":host").replace("body {", ":host { display:block;");
const body = part("<body>", "<script>");
const script = part("<script>", "</script>");
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));

async function eventually(check, expected) {
  let actual;
  for (let i = 0; i < 100; i++) {
    actual = await check();
    if (JSON.stringify(actual) === JSON.stringify(expected)) return;
    await delay(30);
  }
  assert.deepEqual(actual, expected);
}

class Backend {
  workspace = "/projects/a";
  channels = new Map(); calls = []; held = new Map();
  failSend = false;
  channel(path = this.workspace) {
    if (!this.channels.has(path)) this.channels.set(path, { messages:[], end:100, read:0, delivered:0, count:0 });
    return this.channels.get(path);
  }
  add(path, role, markdown, id = `${role}-${this.channel(path).messages.length}`) {
    const channel = this.channel(path), start = channel.end;
    channel.end += markdown.length + 100;
    const message = { id, role, markdown, start, end:channel.end, tool_call_count:channel.count, created_at_ms:Date.now() };
    channel.messages.push(message); return message;
  }
  hold(name, phase = "after") {
    let release, entered;
    const barrier = { phase, wait:new Promise(resolve => { release = resolve; }), started:new Promise(resolve => { entered = resolve; }), release, entered };
    this.held.set(name, barrier); return barrier;
  }
  async call(name, args) {
    this.calls.push({ name, args:structuredClone(args) });
    const hold = this.held.get(name); this.held.delete(name);
    if (hold) { hold.entered(); if (hold.phase === "before") await hold.wait; }
    const path = this.workspace, file = `${path}/CHAT.md`, c = this.channel();
    let result;
    if ((args.expected_workspace && args.expected_workspace !== path) || (args.expected_chat_file && args.expected_chat_file !== file)) {
      result = { isError:true, content:[{type:"text",text:"Workspace changed; refresh the selected workspace."}], _meta:{[META]:{workspace_changed:true}} };
    } else if (name === "chat_ui_state") {
      const all = c.messages.filter(message => args.before === undefined || message.start < args.before), messages = all.slice(-2);
      result = { _meta:{[META]:{chat_file:file, workspace_path:path, revision:String(c.end), messages:structuredClone(messages), read_through:c.read, delivered_through:c.delivered, total_tool_calls:c.count, last_agent_call_at_ms:Date.now(), server_time_ms:Date.now(), has_more:all.length > messages.length, before:messages[0]?.start ?? null, unchanged:false}} };
    } else if (name === "chat_ui_send") {
      if (this.failSend) { if (hold?.phase === "after") await hold.wait; throw new Error("Connection unavailable"); }
      let row = c.messages.find(message => message.id === args.request_id);
      if (!row) row = this.add(path, "user", args.message, args.request_id);
      result = { _meta:{[META]:{sent:{id:row.id, end:row.end, created_at_ms:row.created_at_ms, tool_call_count:row.tool_call_count}}} };
    } else if (name === "chat_ui_file") {
      result = { _meta:{[META]:{file:{type:"resource_link", uri:"codexify://artifact/test", name:"report.txt"}}} };
    } else throw new Error(`Unexpected ${name}`);
    if (hold?.phase === "after") await hold.wait;
    return result;
  }
}

async function fixture(browser, backend, savedState = {}) {
  const page = await browser.newPage({ viewport:{width:390,height:900} });
  page.setDefaultTimeout(8000);
  const errors = [], downloads = [], saved = [];
  page.on("pageerror", error => errors.push(error.message));
  await page.exposeFunction("hostCall", (name,args) => backend.call(name,args));
  await page.exposeFunction("hostWorkspace", () => backend.workspace);
  await page.exposeFunction("saveState", value => saved.push(value));
  await page.exposeFunction("download", value => downloads.push(value));
  await page.route("https://workspace-chat.test/", route => route.fulfill({contentType:"text/html; charset=utf-8", body:`<!doctype html><html><head><meta charset="utf-8"></head><body><div id="host"></div><template id="template"><style>${style}</style>${body}</template><script>${script}</script></body></html>`}));
  await page.goto("https://workspace-chat.test/");
  await page.evaluate(({path, savedState}) => {
    const scope = document.getElementById("host").attachShadow({mode:"open"});
    scope.append(document.getElementById("template").content.cloneNode(true));
    let state = savedState;
    window.controller = window.mountCodexifyChat(scope, {callTool:window.hostCall, reportSize(){}, getState:() => state, setState:value => { state = value; window.saveState(value); }, downloadFile:window.download, openLink(){}, async refreshWorkspace() { window.changeWorkspace(await window.hostWorkspace()); }});
    window.changeWorkspace = path => {
      if (window.controller.setWorkspace) window.controller.setWorkspace(path);
      else window.controller.setAvailable(path !== null);
    };
    window.changeWorkspace(path);
  }, {path:backend.workspace, savedState});
  await page.getByText("Loading this conversation...", {exact:true}).waitFor({state:"hidden"});
  return {page, errors, downloads, saved};
}
async function change(page, backend, path) {
  backend.workspace = path;
  await page.evaluate(path => window.changeWorkspace(path), path);
}
async function poll(page) { await page.evaluate(() => window.dispatchEvent(new Event("focus"))); }
const messages = page => page.locator(".message .markdown").allTextContents();

for (const [name, engine] of [["Chromium",chromium],["WebKit",webkit]]) {
  test(`${name}: workspace-bound chat lifecycle`, {timeout:120000}, async t => {
    const browser = await engine.launch();
    try {
      await t.test("switch loads new history and resets receipts; same panel keeps its draft", async () => {
        const b = new Backend();
        const aText = "A only " + "x".repeat(1024);
        const a = b.add(b.workspace,"user",aText); b.channel().read = b.channel().delivered = a.end;
        b.add("/projects/b","user","B unread");
        const {page,errors} = await fixture(browser,b);
        await eventually(() => messages(page), [aText]);
        await page.getByRole("textbox").fill("My draft");
        await page.locator("#chat").evaluate(node => { window.originalPanel = node; });
        await change(page,b,"/projects/b");
        await eventually(() => messages(page), ["B unread"]);
        assert.equal(await page.getByRole("textbox").inputValue(),"My draft");
        assert.equal(await page.locator("#chat").evaluate(node => node === window.originalPanel),true);
        assert.equal(await page.getByRole("img",{name:"Saved to CHAT.md",exact:true}).count(),1);
        await page.getByRole("textbox").press("Enter");
        await eventually(() => messages(page),["B unread","My draft"]);
        assert.equal(b.channel("/projects/a").messages.length,1);
        const sent = b.channel().messages.at(-1); b.channel().delivered = sent.end;
        await poll(page); await page.getByRole("img",{name:"Delivered to agent",exact:true}).first().waitFor();
        b.channel().read = sent.end; b.add(b.workspace,"agent","B reply");
        await poll(page); await page.getByText("B reply",{exact:true}).waitFor();
        if (process.env.CODEXIFY_CHAT_SWITCH_PREVIEWS) {
          mkdirSync(process.env.CODEXIFY_CHAT_SWITCH_PREVIEWS,{recursive:true});
          await page.locator("#chat").screenshot({path:`${process.env.CODEXIFY_CHAT_SWITCH_PREVIEWS}/${name.toLowerCase()}-switched.png`});
        }
        await change(page,b,"/projects/a"); await eventually(() => messages(page),[aText]);
        assert.equal(await page.getByRole("img",{name:"Read by agent",exact:true}).count(),1);
        assert.deepEqual(errors,[]); await page.close();
      });
      await t.test("late polls, pagination and errors cannot restore the previous context", async () => {
        for (const history of [false,true]) for (const phase of ["before","after"]) {
          const b = new Backend();
          for (let i=0;i<3;i++) b.add(b.workspace,"agent",`A ${i}`);
          b.add("/projects/b","agent","B current");
          const {page,errors} = await fixture(browser,b);
          await eventually(() => messages(page),["A 1","A 2"]);
          const held = b.hold("chat_ui_state",phase);
          if (history) await page.getByRole("button",{name:"Load earlier messages",exact:true}).click(); else await poll(page);
          await held.started;
          await change(page,b,"/projects/b"); await eventually(() => messages(page),["B current"]);
          held.release(); await delay(100);
          assert.deepEqual(await messages(page),["B current"]);
          assert.equal(await page.locator("#status.error").count(),0);
          await change(page,b,"/projects/a"); await eventually(() => messages(page),["A 1","A 2"]);
          assert.deepEqual(errors,[]); await page.close();
        }
      });
      await t.test("A to B to A ignores an old A result, even with the same transcript path", async () => {
        const b = new Backend(); b.add(b.workspace,"agent","A old");
        const {page} = await fixture(browser,b); await eventually(() => messages(page),["A old"]);
        const held = b.hold("chat_ui_state"); await poll(page); await held.started;
        await change(page,b,"/projects/b"); await eventually(() => messages(page),[]);
        b.channel("/projects/a").messages[0].markdown = "A current";
        await change(page,b,"/projects/a"); await eventually(() => messages(page),["A current"]);
        held.release(); await delay(100);
        assert.deepEqual(await messages(page),["A current"]); await page.close();
      });
      await t.test("a send racing with a switch is not rerouted, and retries stay with A", async () => {
        const b = new Backend(); b.add(b.workspace,"agent","A ready"); b.add("/projects/b","agent","B ready");
        const {page,saved} = await fixture(browser,b); await eventually(() => messages(page),["A ready"]);
        const held = b.hold("chat_ui_send","before");
        await page.getByRole("textbox").fill("For A only"); await page.getByRole("textbox").press("Enter"); await held.started;
        await change(page,b,"/projects/b"); await eventually(() => messages(page),["B ready"]);
        held.release(); await delay(150);
        assert.equal(b.channel("/projects/b").messages.length,1);
        assert.deepEqual(await messages(page),["B ready"]);
        assert(saved.at(-1).privateContent.pending.some(row => row.markdown === "For A only"));
        await change(page,b,"/projects/a");
        await page.getByRole("button",{name:"Retry",exact:true}).waitFor();
        await page.getByRole("button",{name:"Retry",exact:true}).click();
        await page.getByRole("img",{name:"Saved to CHAT.md",exact:true}).waitFor();
        assert.equal(b.channel().messages.filter(row => row.markdown === "For A only").length,1);
        assert.equal(b.calls.filter(call => call.name === "chat_ui_send")[0].args.request_id,b.calls.filter(call => call.name === "chat_ui_send")[1].args.request_id);
        await page.close();
      });
      await t.test("an accepted A send and a delayed download cannot mutate B's panel", async () => {
        const b = new Backend(); b.add(b.workspace,"agent","[Report](report.txt)"); b.add("/projects/b","agent","B ready");
        const {page,downloads} = await fixture(browser,b);
        await page.getByRole("link",{name:"Report",exact:true}).waitFor();
        const download = b.hold("chat_ui_file"); await page.getByRole("link",{name:"Report",exact:true}).click(); await download.started;
        const send = b.hold("chat_ui_send"); await page.getByRole("textbox").fill("Accepted in A"); await page.getByRole("textbox").press("Enter"); await send.started;
        await change(page,b,"/projects/b"); await eventually(() => messages(page),["B ready"]);
        download.release(); send.release(); await delay(150);
        assert.deepEqual(downloads,[]); assert.deepEqual(await messages(page),["B ready"]);
        assert.equal(b.channel("/projects/a").messages.at(-1).markdown,"Accepted in A");
        await page.close();
      });
      await t.test("pending destinations and drafts survive a reload in another workspace", async () => {
        const b = new Backend(); b.add(b.workspace,"agent","A ready");
        const first = await fixture(browser,b); await eventually(() => messages(first.page),["A ready"]);
        b.failSend = true;
        await first.page.getByRole("textbox").fill("Keep this for A"); await first.page.getByRole("textbox").press("Enter");
        await first.page.getByRole("button",{name:"Retry",exact:true}).waitFor();
        await change(first.page,b,"/projects/b");
        await first.page.getByRole("textbox").fill("Draft for B");
        await eventually(() => first.saved.at(-1)?.privateContent.draft,"Draft for B");
        const savedState = first.saved.at(-1); await first.page.close();
        const second = await fixture(browser,b,savedState);
        assert.equal(await second.page.getByRole("textbox").inputValue(),"Draft for B");
        assert.deepEqual(await messages(second.page),[]);
        await second.page.locator("#retained-sends summary").click();
        await second.page.locator("#retained-messages").getByText("Keep this for A",{exact:true}).waitFor();
        assert.equal(b.calls.filter(call => call.name === "chat_ui_send").length,1);
        b.failSend = false; await change(second.page,b,"/projects/a");
        await second.page.getByRole("button",{name:"Retry",exact:true}).waitFor();
        await second.page.getByRole("button",{name:"Retry",exact:true}).click();
        await second.page.getByRole("img",{name:"Saved to CHAT.md",exact:true}).waitFor();
        assert.equal(b.channel().messages.at(-1).markdown,"Keep this for A");
        await second.page.close();
      });
      await t.test("legacy pending messages are retained without assigning them a destination", async () => {
        const b = new Backend(); b.add(b.workspace,"agent","Ready");
        const savedState = {privateContent:{pending:[{id:"legacy",role:"user",markdown:"Do not lose this",failed:true}]}};
        const {page} = await fixture(browser,b,savedState); await eventually(() => messages(page),["Ready"]);
        await page.locator("#retained-sends summary").click();
        await page.getByText("Original destination unknown",{exact:true}).waitFor();
        await page.getByText("Do not lose this",{exact:true}).waitFor();
        assert.equal(await page.getByRole("button",{name:"Retry",exact:true}).count(),0);
        assert.equal(b.calls.filter(call => call.name === "chat_ui_send").length,0);
        await page.close();
      });
      await t.test("same-workspace updates keep the DOM, and collapsed switches load on expansion", async () => {
        const b = new Backend(); b.add(b.workspace,"agent","A ready"); b.add("/projects/b","agent","B ready");
        const {page} = await fixture(browser,b); await eventually(() => messages(page),["A ready"]);
        await page.locator(".message").evaluate(node => { window.previousMessage = node; });
        await change(page,b,"/projects/a");
        assert(await page.locator(".message").evaluate(node => node === window.previousMessage));
        await page.getByRole("button",{name:"Collapse chat",exact:true}).click();
        await change(page,b,"/projects/b");
        assert.equal(await page.getByRole("button",{name:"Expand chat",exact:true}).count(),1);
        await page.getByRole("button",{name:"Expand chat",exact:true}).click();
        await eventually(() => messages(page),["B ready"]);
        await page.close();
      });
      await t.test("another card's switch refreshes the selected workspace without mixing chats", async () => {
        const b = new Backend(); b.add(b.workspace,"agent","A ready"); b.add("/projects/b","agent","B ready");
        const first = await fixture(browser,b), second = await fixture(browser,b);
        await eventually(() => messages(first.page),["A ready"]); await eventually(() => messages(second.page),["A ready"]);
        await second.page.getByRole("textbox").fill("Keep the second card's draft");
        await change(first.page,b,"/projects/b"); await eventually(() => messages(first.page),["B ready"]);
        await poll(second.page); await eventually(() => messages(second.page),["B ready"]);
        assert.equal(await second.page.getByRole("textbox").inputValue(),"Keep the second card's draft");
        assert.equal(await second.page.locator("#status.error").count(),0);
        assert(!b.calls.some(call => call.name === "chat_ui_send"));
        await first.page.close(); await second.page.close();
      });
      await t.test("sending waits for the destination without losing text typed during loading", async () => {
        const b = new Backend(), held = b.hold("chat_ui_state");
        const {page} = await fixture(browser,b); await held.started;
        await page.getByRole("textbox").fill("Wait for the transcript");
        await page.getByRole("textbox").press("Enter");
        assert.equal(await page.getByRole("textbox").inputValue(),"Wait for the transcript");
        assert(await page.getByRole("button",{name:"Send message",exact:true}).isDisabled());
        assert(!b.calls.some(call => call.name === "chat_ui_send"));
        held.release(); await page.locator("#send:enabled").waitFor();
        await page.getByRole("textbox").press("Enter");
        await page.getByRole("img",{name:"Saved to CHAT.md",exact:true}).waitFor();
        assert.equal(b.channel().messages[0].markdown,"Wait for the transcript");
        await page.close();
      });
    } finally {await browser.close();}
  });
}
