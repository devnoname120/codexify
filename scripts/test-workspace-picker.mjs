import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import test from "node:test";

const { chromium, webkit } = createRequire(import.meta.url)("playwright");
const html = readFileSync(new URL("../src/setup_ui.html", import.meta.url), "utf8");

async function openCard(browser, selected = false, schema = "current") {
  const page = await browser.newPage({ viewport: { width: 390, height: 900 } });
  const calls = [], links = [], errors = [];
  const project = selected ? { status:"selected", selectionAvailable:true, name:"Demo", activePath:"/projects/demo", sourcePath:"/projects/demo", managedWorktree:false } : { status:"unselected", selectionAvailable:true, accessRoot:"/projects" };
  const state = {
    serverVersion:"1.5.3", worktreeMode:"always", project,
    update:{ status:"up_to_date", currentVersion:"1.5.3", latestVersion:"1.5.3" },
    connectorSchema:{ status:schema, advertisedVersion:"1.5.3+workspace-v1", observedVersion:schema === "current" ? "1.5.3+workspace-v1" : "1.5.2", connectorVersion:schema === "current" ? "1.5.3+workspace-v1" : "1.5.2", refreshRecommended:schema === "stale" }
  };
  let failReuse = false;
  page.on("pageerror", error => errors.push(error.message));
  await page.exposeFunction("hostTool", async (name, args) => {
    calls.push({ name, args });
    if (name === "setup_status") return { structuredContent:state };
    if (name === "doctor") return { structuredContent:{ ok:true, checks:[], summary:{ passed:1, warnings:0, failures:0 } } };
    if (name === "list_projects") return { structuredContent:{ projects:[{ name:"Demo", selector:"demo" }], total:1, warnings:[] } };
    if (name === "setup_ui_switch_project") {
      assert.equal(args.expectedPath, state.project.activePath);
      state.project = { status:"unselected", selectionAvailable:true, accessRoot:"/projects" };
      return { content:[{ type:"text", text:"Choose a workspace" }], structuredContent:{ content:"Choose a workspace" } };
    }
    if (name === "setup_ui_list_worktrees") return { structuredContent:{ sourcePath:"/projects/demo", worktrees:[
      { name:"unfinished-feature", path:"/worktrees/earlier/demo", gitRoot:"/worktrees/earlier/demo", branch:"unfinished-feature", lastUsedAtMs:Date.UTC(2026,8,14,12), managedWorktree:true, sourceCheckout:false },
      { name:"older", path:"/worktrees/older/demo", gitRoot:"/worktrees/older/demo", branch:null, lastUsedAtMs:null, managedWorktree:true, sourceCheckout:false }
    ] } };
    if (name === "setup_ui_reuse_worktree" && failReuse) return { isError:true, content:[{ type:"text", text:"Worktree is no longer available" }] };
    if (name === "set_project_root" || name === "setup_ui_reuse_worktree") {
      const active = args.worktreePath || (args.withoutProject ? "/scratch/explicit" : "/worktrees/new/demo");
      state.project = { status:args.withoutProject ? "without_project" : "selected", selectionAvailable:true, name:"Demo", activePath:active, sourcePath:"/projects/demo", managedWorktree:!args.withoutProject };
      return { structuredContent:{ mode:args.withoutProject ? "without_project" : "project", active_root:active, source_project_root:"/projects/demo", project_name:"Demo", managed_worktree:!args.withoutProject } };
    }
    throw new Error(`Unexpected tool ${name}`);
  });
  await page.exposeFunction("hostLink", options => links.push(options.href));
  await page.addInitScript(value => {
    window.openai = { toolOutput:{ structuredContent:value }, callTool:(...args) => window.hostTool(...args), openExternal:options => window.hostLink(options) };
    window.addEventListener("message", event => {
      const m = event.data;
      if (!m?.method || m.id === undefined) return;
      const run = m.method === "tools/call" ? window.hostTool(m.params.name, m.params.arguments) : Promise.resolve({});
      run.then(result => window.postMessage({ jsonrpc:"2.0", id:m.id, result }, "*"), error => window.postMessage({ jsonrpc:"2.0", id:m.id, error:{ message:error.message } }, "*"));
    });
  }, state);
  await page.route("https://asdk_app_test.web-sandbox.oaiusercontent.com/**", route => route.fulfill({ contentType:"text/html", body:html }));
  await page.goto("https://asdk_app_test.web-sandbox.oaiusercontent.com/widget");
  await page.getByRole("button", { name:"Check for updates", exact:true }).waitFor();
  return { page, calls, links, errors, state, setFailReuse:value => { failReuse = value; } };
}

for (const [engineName, engine] of [["Chromium", chromium], ["WebKit", webkit]]) {
  test(`${engineName}: greeting does not select scratch; GitHub URLs require explicit submit`, { timeout:60000 }, async () => {
    const browser = await engine.launch();
    try {
      for (const suffix of ["", "/pull/45", "/tree/topic/branch", `/commit/${"a".repeat(40)}`]) {
        const { page, calls, errors } = await openCard(browser);
        await page.getByLabel("GitHub URL", { exact:true }).fill(`https://github.com/example/demo${suffix}`);
        assert(!calls.some(call => call.name === "set_project_root"));
        await page.getByRole("button", { name:"Check for updates", exact:true }).click();
        assert.equal(await page.getByLabel("GitHub URL", { exact:true }).inputValue(), `https://github.com/example/demo${suffix}`);
        await page.getByLabel("GitHub URL", { exact:true }).press("Enter");
        await page.getByText("Project selected", { exact:true }).waitFor();
        assert.deepEqual(calls.find(call => call.name === "set_project_root").args, { path:`https://github.com/example/demo${suffix}`, createWorktree:true });
        assert.deepEqual(errors, []);
        await page.close();
      }
    } finally { await browser.close(); }
  });
  test(`${engineName}: switch, list, retry and reuse an older worktree without creating one`, { timeout:30000 }, async () => {
    const browser = await engine.launch();
    try {
      const { page, calls, errors, setFailReuse } = await openCard(browser, true);
      await page.getByRole("button", { name:"Switch to another project", exact:true }).click();
      await page.getByRole("heading", { name:"Choose a workspace" }).waitFor();
      await page.getByRole("button", { name:"Existing worktrees for Demo", exact:true }).click();
      await page.getByText("/worktrees/earlier/demo", { exact:true }).waitFor();
      await page.getByText("Last used: Not recorded", { exact:true }).waitFor();
      assert.equal(calls.filter(call => call.name === "set_project_root").length, 0);
      setFailReuse(true);
      await page.getByRole("button", { name:"Use worktree unfinished-feature", exact:true }).click();
      await page.getByText(/Could not select workspace: Worktree is no longer available/).waitFor();
      setFailReuse(false);
      await page.getByRole("button", { name:"Use worktree unfinished-feature", exact:true }).click();
      await page.getByText("Project selected", { exact:true }).waitFor();
      await page.getByText("/worktrees/earlier/demo", { exact:true }).waitFor();
      assert.deepEqual(calls.filter(call => call.name === "setup_ui_reuse_worktree").at(-1).args, { path:"demo", worktreePath:"/worktrees/earlier/demo" });
      assert.equal(calls.filter(call => call.name === "set_project_root").length, 0);
      assert.deepEqual(errors, []);
    } finally { await browser.close(); }
  });
  test(`${engineName}: fresh conversation can reuse a worktree and refresh uses an accessible popover`, { timeout:30000 }, async () => {
    const browser = await engine.launch();
    try {
      const { page, calls, links, errors } = await openCard(browser, false, "stale");
      await page.getByRole("button", { name:"Refresh", exact:true }).click();
      const dialog = page.getByRole("dialog", { name:"Refresh the Codexify connector" });
      await dialog.waitFor();
      assert.equal(links.length, 0);
      assert.match(await dialog.innerText(), /Settings.*Plugins.*Codexify connector/);
      const link = dialog.getByRole("link", { name:"Open connector settings", exact:true });
      assert.match(await link.getAttribute("href"), /plugin_asdk_app_test:~:text=Information-,Refresh,-Connected$/);
      await link.click();
      assert.equal(links.length, 1);
      assert(!calls.some(call => /reload|refresh/.test(call.name)));
      await dialog.press("Escape");
      await dialog.waitFor({ state:"hidden" });
      await page.getByRole("button", { name:"Existing worktrees for Demo", exact:true }).click();
      await page.getByRole("button", { name:"Use worktree unfinished-feature", exact:true }).click();
      await page.getByText("Project selected", { exact:true }).waitFor();
      assert.equal(calls.filter(call => call.name === "set_project_root").length, 0);
      assert.deepEqual(errors, []);
    } finally { await browser.close(); }
  });
}
