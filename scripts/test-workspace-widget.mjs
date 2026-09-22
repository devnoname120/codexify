import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { createRequire } from "node:module";
import test from "node:test";
import { setupChatHtml } from "./chat-widget-source.mjs";
const { chromium, webkit } = createRequire(import.meta.url)("playwright");
const META = "io.github.devnoname120/codexify/markdown-chat";
const VERSION = "1.5.3+markdown-chat-v5+workspace-v1";

async function screenshot(page, name) {
  if (!process.env.CODEXIFY_WORKSPACE_SCREENSHOTS) return;
  mkdirSync(process.env.CODEXIFY_WORKSPACE_SCREENSHOTS, { recursive:true });
  await page.screenshot({ path:`${process.env.CODEXIFY_WORKSPACE_SCREENSHOTS}/${name}.png`, fullPage:true });
}

async function fixture(browser, options = {}) {
  const page = await browser.newPage({ viewport:{ width:390, height:950 }, colorScheme:options.dark ? "dark" : "light" });
  const calls = [], links = [], errors = [];
  let failSelection = false;
  const state = {
    serverVersion:"1.5.3", worktreeMode:"auto",
    project:{ status:"unselected", selectionAvailable:true, accessRoot:"/projects", activePath:null, sourcePath:null },
    update:{ status:"up_to_date", currentVersion:"1.5.3", latestVersion:"1.5.3" },
    connectorSchema:{ status:options.stale ? "stale" : "current", advertisedVersion:VERSION, observedVersion:options.stale ? "1.5.2" : VERSION, connectorVersion:options.stale ? "1.5.2" : VERSION, refreshRecommended:!!options.stale }
  };
  function choose(path, managed = false) {
    state.project = { status:"selected", selectionAvailable:true, accessRoot:"/projects", name:"Demo", sourcePath:"/projects/demo", activePath:path, managedWorktree:managed, bindingScope:"chatgpt_conversation" };
    return { structuredContent:{ mode:"project", active_root:path, source_project_root:"/projects/demo", project_name:"Demo", managed_worktree:managed } };
  }
  await page.exposeFunction("hostCall", async (name, args) => {
    calls.push({ name, args });
    if (name === "setup_status") return { structuredContent:structuredClone(state) };
    if (name === "doctor") return { structuredContent:{ ok:true, checks:[], summary:{ passed:1, warnings:0, failures:0, skipped:0 } } };
    if (name === "setup_ui_list_projects" || name === "list_projects") return { structuredContent:{ projects:[{ selector:"demo", name:"Demo" }], total:1, warnings:[] } };
    if (name === "setup_ui_list_worktrees") return { structuredContent:{ sourcePath:"/projects/demo", worktrees:[
      { name:"main", path:"/projects/demo", gitRoot:"/projects/demo", branch:"main", sourceCheckout:true, managedWorktree:false, lastUsedAtMs:null },
      { name:"earlier-fix", path:"/worktrees/earlier/demo", gitRoot:"/worktrees/earlier/demo", branch:"earlier-fix", sourceCheckout:false, managedWorktree:true, lastUsedAtMs:1789372800000 }
    ] } };
    if (name === "setup_ui_select_project" || name === "set_project_root") {
      if (failSelection) return { isError:true, content:[{ type:"text", text:"Repository is not available" }] };
      return choose(args.createWorktree ? "/worktrees/new/demo" : "/projects/demo", args.createWorktree);
    }
    if (name === "setup_ui_reuse_worktree") return choose(args.worktreePath, true);
    if (name === "setup_ui_switch_project") {
      assert.equal(args.expectedPath, state.project.activePath);
      state.project = { status:"unselected", selectionAvailable:true, accessRoot:"/projects", activePath:null, sourcePath:null };
      return { structuredContent:{ content:"Workspace selection is open" } };
    }
    if (name === "chat_ui_state") return { _meta:{ [META]:{ chat_file:`${state.project.activePath}/CHAT.md`, workspace_path:state.project.activePath, revision:"1", messages:[{ id:"welcome", role:"agent", markdown:`Chat for ${state.project.activePath}`, start:10, end:100, tool_call_count:1 }], read_through:0, delivered_through:0, last_agent_call_at_ms:Date.now(), server_time_ms:Date.now(), total_tool_calls:1, has_more:false, before:null } } };
    throw new Error(`Unexpected tool ${name}`);
  });
  await page.exposeFunction("hostLink", href => { links.push(href); return {}; });
  await page.addInitScript(({ initial, theme }) => {
    window.openai = { toolOutput:{ structuredContent:initial }, theme, callTool:window.hostCall, openExternal:({href}) => window.hostLink(href) };
    window.addEventListener("message", async event => {
      const message = event.data;
      if (!message?.method || message.id === undefined) return;
      try {
        let result = {};
        if (message.method === "ui/initialize") result = { hostContext:{ theme }, hostCapabilities:{} };
        else if (message.method === "tools/call") result = await window.hostCall(message.params.name, message.params.arguments);
        else if (message.method === "ui/open-link") result = await window.hostLink(message.params.url || message.params.href);
        window.postMessage({ jsonrpc:"2.0", id:message.id, result }, "*");
      } catch (error) { window.postMessage({ jsonrpc:"2.0", id:message.id, error:{ message:error.message } }, "*"); }
    });
  }, { initial:state, theme:options.dark ? "dark" : "light" });
  page.on("pageerror", error => errors.push(error.message));
  await page.route("https://asdk_app_test.web-sandbox.oaiusercontent.com/**", route => route.fulfill({ contentType:"text/html", body:setupChatHtml }));
  await page.goto("https://asdk_app_test.web-sandbox.oaiusercontent.com/");
  await page.getByPlaceholder("Search projects").waitFor();
  await page.getByRole("button", { name:/^Demo/ }).first().waitFor();
  return { page, state, calls, links, errors, fail(value) { failSelection = value; } };
}

for (const [name, engine] of [["chromium", chromium], ["webkit", webkit]]) {
  test(`${name}: greeting remains unselected; URL entry, errors, switching and reuse`, { timeout:45000 }, async () => {
    const browser = await engine.launch();
    try {
      const f = await fixture(browser);
      const { page, calls } = f;
      assert(!calls.some(call => /select_project|set_project_root|reuse_worktree/.test(call.name)));
      await screenshot(page, `${name}-picker`);
      const github = page.getByRole("textbox", { name:"GitHub URL" });
      await github.fill("https://github.com/example/demo/pull/45");
      f.fail(true);
      await github.press("Enter");
      await page.getByText(/Repository is not available/).waitFor();
      assert.equal(await github.inputValue(), "https://github.com/example/demo/pull/45");
      f.fail(false);
      await page.getByRole("checkbox").uncheck();
      await github.press("Enter");
      await page.getByRole("button", { name:"Switch to another project", exact:true }).waitFor();
      const selected = calls.filter(call => /select_project|set_project_root/.test(call.name)).at(-1);
      assert.deepEqual(selected.args, { path:"https://github.com/example/demo/pull/45", createWorktree:false });
      await page.locator("#markdown-chat-host").getByText("Chat for /projects/demo", { exact:true }).waitFor();
      await page.locator("#markdown-chat-host #draft").fill("Keep this draft while choosing");
      await page.getByRole("button", { name:"Switch to another project", exact:true }).click();
      await github.waitFor();
      await page.getByRole("button", { name:"Existing worktrees for Demo", exact:true }).click();
      await page.getByText("/worktrees/earlier/demo", { exact:true }).waitFor();
      assert.match(await page.locator("#worktree-options").innerText(), /earlier-fix|Last used/);
      await screenshot(page, `${name}-worktrees`);
      await page.getByRole("button", { name:"Use worktree earlier-fix", exact:true }).click();
      await page.getByRole("button", { name:"Switch to another project", exact:true }).waitFor();
      assert.equal(await page.locator("#markdown-chat-host #draft").inputValue(), "Keep this draft while choosing");
      await page.locator("#markdown-chat-host").getByText("Chat for /worktrees/earlier/demo", { exact:true }).waitFor({ timeout:8000 });
      assert.equal(await page.locator("#markdown-chat-host #status.error").count(), 0);
      assert.equal(await page.locator("#markdown-chat-host").getByText("Chat for /projects/demo", { exact:true }).count(), 0);
      assert.deepEqual(calls.find(call => call.name === "setup_ui_reuse_worktree").args, { path:"demo", worktreePath:"/worktrees/earlier/demo" });
      assert.deepEqual(f.errors, []);
      assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
      if (process.env.CODEXIFY_WORKSPACE_SCREENSHOTS) {
        mkdirSync(process.env.CODEXIFY_WORKSPACE_SCREENSHOTS, { recursive:true });
        await page.screenshot({ path:`${process.env.CODEXIFY_WORKSPACE_SCREENSHOTS}/${name}-reused.png`, fullPage:true });
      }
    } finally { await browser.close(); }
  });
  test(`${name}: all GitHub URL kinds use the normal selection backend`, { timeout:45000 }, async () => {
    const browser = await engine.launch();
    try {
      const f = await fixture(browser);
      const input = f.page.getByRole("textbox", { name:"GitHub URL" });
      for (const suffix of ["", "/pull/45", "/tree/feature/nested", "/commit/" + "a".repeat(40)]) {
        const url = "https://github.com/example/demo" + suffix;
        await input.fill(url);
        await f.page.getByRole("button", { name:"Use URL", exact:true }).click();
        await f.page.getByRole("button", { name:"Switch to another project", exact:true }).waitFor();
        assert.deepEqual(f.calls.filter(call => call.name === "setup_ui_select_project").at(-1).args, { path:url, createWorktree:true });
        await f.page.getByRole("button", { name:"Switch to another project", exact:true }).click();
        await input.waitFor();
      }
      assert.deepEqual(f.errors, []);
    } finally { await browser.close(); }
  });
  test(`${name}: refresh opens an accessible dimmed popover without claiming reload`, { timeout:30000 }, async () => {
    const browser = await engine.launch();
    try {
      const f = await fixture(browser, { stale:true, dark:true });
      const { page } = f;
      await page.getByRole("button", { name:"Refresh", exact:true }).click();
      const dialog = page.getByRole("dialog", { name:"Refresh the Codexify connector" });
      await dialog.waitFor();
      assert.equal(f.links.length, 0);
      assert.match(await dialog.innerText(), /Settings.*Plugins/);
      assert.match(await dialog.innerText(), /Information/);
      const link = dialog.getByRole("link", { name:"Open connector settings", exact:true });
      assert.match(await link.getAttribute("href"), /^https:\/\/chatgpt.com\/#settings\/Plugins\/plugin_asdk_app_test:~:text=Information-,Refresh,-Connected$/);
      await link.click();
      assert.equal(f.links.length, 1);
      assert.equal(f.state.connectorSchema.status, "stale");
      if (process.env.CODEXIFY_WORKSPACE_SCREENSHOTS) {
        mkdirSync(process.env.CODEXIFY_WORKSPACE_SCREENSHOTS, { recursive:true });
        await page.screenshot({ path:`${process.env.CODEXIFY_WORKSPACE_SCREENSHOTS}/${name}-refresh.png`, fullPage:true });
      }
      await page.keyboard.press("Escape");
      await dialog.waitFor({ state:"hidden" });
      assert.deepEqual(f.errors, []);
    } finally { await browser.close(); }
  });
}
