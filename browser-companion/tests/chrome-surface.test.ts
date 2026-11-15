import { EventEmitter } from "node:events";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, expect, spyOn, test } from "bun:test";
import { chromium, type BrowserContext, type Page } from "patchright";
import { readChatGptAccount } from "../src/account";
import { ChromeSingleWindowSurface } from "../src/chrome-surface";
import { privateChromeExecutable } from "../src/chrome-executable";
import { backgroundChromeArgs } from "../src/background-chrome";
import { activeComposer, composerText, insertExactPrompt } from "../src/composer";
import { responseDomSnapshot, type ChatGptResponseDomCache } from "../src/response-observer";
import { readTurnDomState } from "../src/turn-binding";
import type {BrowserSimplicity} from "../src/minimal-page";
import {throwIfConversationRateLimited} from "../src/navigation";
import {installChatModeCompatibility} from "../src/chat-mode";

// Chat-mode compatibility is installed on every page; these tests inspect styling scripts.
const styled=<T extends {script:unknown}>(scripts:T[]):T[]=>scripts.filter(entry=>entry.script!==installChatModeCompatibility);

const fakeExecutablePath = join(tmpdir(), "managed-chrome");

class FakePage extends EventEmitter {
  currentUrl="about:blank";
  url=()=>this.currentUrl;
  owner:FakeContext|undefined;
  context=()=>this.owner as unknown as BrowserContext;
  isClosed = () => this.closed;
  closed = false;
  navigations: string[] = [];
  revealed = false;
  navigationError: Error | undefined;
  evaluatedScripts: Array<{ script:unknown; arg:unknown }> = [];
  async evaluate(script:unknown,arg:unknown) { this.evaluatedScripts.push({script,arg}); }
  async goto(url: string) {
    if (this.navigationError) throw this.navigationError;
    this.navigations.push(url);
    this.currentUrl=url;
  }
  async waitForLoadState() {}
  async bringToFront() { this.revealed = true; }
  async close() { this.closed = true; this.emit("close"); }
}

class FakeContext extends EventEmitter {
  closed = false;
  closeCalls=0;
  windowState = "normal";
  createdTargets: unknown[] = [];
  initScripts: Array<{ script:unknown; arg:unknown }> = [];
  constructor(readonly tabs = [new FakePage()]) { super();for(const page of tabs)page.owner=this; }
  pages() { return this.tabs.filter(page => !page.isClosed()) as unknown as Page[]; }
  async addInitScript(script:unknown,arg:unknown) { this.initScripts.push({script,arg}); }
  async newPage() {
    const page = new FakePage();
    page.owner=this;
    this.tabs.push(page);
    this.emit("page", page);
    return page as unknown as Page;
  }
  browser() {
    return { newBrowserCDPSession:async () => ({
      send:async (_method: string, params: unknown) => {
        this.createdTargets.push(params);
        const page = await this.newPage();
      },
      detach:async () => {},
    }) };
  }
  waitForEvent(event: string) {
    return new Promise<Page>(resolve => this.once(event, resolve));
  }
  async newCDPSession() {
    return {
      send:async (method: string) => {
        if (method === "Browser.setWindowBounds") this.windowState = "normal";
        return { windowId:1, bounds:{ windowState:this.windowState } };
      },
      detach:async () => {},
    };
  }
  async close() {
    this.closeCalls++;
    this.closed = true;
    for (const page of this.tabs) await page.close();
    this.emit("close");
  }
}

let profilePath: string | undefined;
let context: FakeContext | BrowserContext | undefined;
let launchSpy: ReturnType<typeof spyOn<typeof chromium, "launchPersistentContext">> | undefined;

afterEach(async () => {
  launchSpy?.mockRestore();
  launchSpy = undefined;
  await context?.close();
  context = undefined;
  if (profilePath) await rm(profilePath, { recursive:true, force:true });
  profilePath = undefined;
});

async function createSurface(tabs?: FakePage[], minimalUi?:boolean, browserSimplicity?:BrowserSimplicity,actionLaneLimit?:number,presentation=false) {
  profilePath = await mkdtemp(join(tmpdir(), "browser-chrome-surface-"));
  const fake = new FakeContext(tabs);
  context = fake;
  launchSpy = spyOn(chromium, "launchPersistentContext")
    .mockResolvedValue(fake as unknown as BrowserContext);
  return { surface:await ChromeSingleWindowSurface.create({ profilePath, executablePath:fakeExecutablePath, minimalUi, browserSimplicity,actionLaneLimit,
    ...(presentation?{presentationCssPath:join(profilePath,"presentation.css")}:{}) }), fake };
}

test("an explicit Bones level overrides the legacy toggle and styles a restored conversation",async()=>{
  const shared=new FakePage();shared.currentUrl="https://chatgpt.com/c/restored";
  const {fake}=await createSurface([shared],false,"bones");
  expect(shared.navigations).toEqual([]);
  expect(styled(fake.initScripts)).toHaveLength(1);
  expect(styled(fake.initScripts)[0]!.arg).toMatchObject({level:"bones"});
  expect(styled(shared.evaluatedScripts)[0]!.arg).toMatchObject({level:"bones"});
  expect(launchSpy!.mock.calls[0]![1]?.reducedMotion).toBe("reduce");
});

test("the browser uses a dedicated persistent Chrome profile and one shared page", async () => {
  const shared = new FakePage();
  const restored = new FakePage();
  const { surface } = await createSurface([shared, restored]);
  expect(launchSpy).toHaveBeenCalledWith(profilePath, {
    channel:"chrome", executablePath:fakeExecutablePath, headless:false, viewport:null,
    ignoreDefaultArgs:true, timeout:30_000, args:await backgroundChromeArgs(profilePath!),
  });
  expect(restored.closed).toBeTrue();
  expect(await surface.page()).toBe(shared as unknown as Page);
  expect(shared.navigations).toEqual(["https://chatgpt.com/"]);
  expect(styled((context as FakeContext).initScripts)).toHaveLength(0);
  await surface.reveal();
  expect(shared.revealed).toBeTrue();
});

test("startup creates a background page and only an explicit reveal restores focus", async () => {
  const { surface, fake } = await createSurface([]);
  expect(fake.createdTargets).toEqual([{ url:"about:blank", newWindow:true, background:true }]);
  expect(fake.tabs[0]!.revealed).toBeFalse();
  await surface.beginAction();
  await surface.endAction();
  expect(fake.tabs[0]!.revealed).toBeFalse();
  expect(fake.windowState).toBe("normal");
  fake.windowState = "minimized";
  await surface.reveal();
  expect(fake.windowState).toBe("normal");
  expect(fake.tabs[0]!.revealed).toBeTrue();
});

test("startup preserves a restored ChatGPT conversation without loading home",async()=>{
  const shared=new FakePage();shared.currentUrl='https://chatgpt.com/c/restored';
  const {surface}=await createSurface([shared],true);
  expect(shared.navigations).toEqual([]);expect(await surface.page()).toBe(shared as unknown as Page);
  expect(styled(shared.evaluatedScripts)).toHaveLength(1);
  expect(styled(shared.evaluatedScripts)[0]!.arg).toMatchObject({styleId:"codexify-minimal-chatgpt"});
});

test("minimal UI is off unless explicitly enabled and preserves ordinary restored tabs",async()=>{
  const previous=process.env.CODEXIFY_BROWSER_MINIMAL_UI;
  delete process.env.CODEXIFY_BROWSER_MINIMAL_UI;
  try {
    const shared=new FakePage();shared.currentUrl="https://chatgpt.com/c/restored";
    const {fake}=await createSurface([shared]);
    expect(styled(fake.initScripts)).toHaveLength(0);
    expect(styled(shared.evaluatedScripts)).toHaveLength(0);
    expect(launchSpy!.mock.calls[0]![1]?.reducedMotion).toBeUndefined();
  } finally {
    if(previous===undefined)delete process.env.CODEXIFY_BROWSER_MINIMAL_UI;
    else process.env.CODEXIFY_BROWSER_MINIMAL_UI=previous;
  }
});

test("an explicit disabled setting overrides a minimal UI environment opt-in",async()=>{
  const previous=process.env.CODEXIFY_BROWSER_MINIMAL_UI;
  process.env.CODEXIFY_BROWSER_MINIMAL_UI="1";
  try {
    const {surface,fake}=await createSurface(undefined,false);
    expect(styled(fake.initScripts)).toHaveLength(0);
    expect(launchSpy!.mock.calls[0]![1]?.reducedMotion).toBeUndefined();
    await expect(surface.setPresentationStyle("body { color:red; }")).rejects.toThrow("Enable browser simplifications");
  }finally{
    if(previous===undefined)delete process.env.CODEXIFY_BROWSER_MINIMAL_UI;
    else process.env.CODEXIFY_BROWSER_MINIMAL_UI=previous;
  }
});

test("background launch retains Patchright's transport and private profile without a startup URL", async () => {
  const args = await backgroundChromeArgs(fakeExecutablePath);
  expect(args).toContain(`--user-data-dir=${fakeExecutablePath}`);
  expect(args).toContain("--remote-debugging-pipe");
  expect(args).toContain("--no-startup-window");
  expect(args).not.toContain("about:blank");
  expect(args).not.toContain("--headless");
});

test("extra pages and closing Chrome report external interference", async () => {
  const { surface, fake } = await createSurface();
  const extra = await fake.newPage();
  await expect(surface.page()).rejects.toMatchObject({ code:"external_interference" });
  await extra.close();
  expect(await surface.page()).toBeDefined();
  await surface.close();
  await surface.closed;
  await expect(surface.page()).rejects.toMatchObject({ code:"external_interference" });
});

test("Chrome actions keep exclusive ownership and reject unsafe or aborted navigation", async () => {
  const { surface, fake } = await createSurface();
  await surface.beginAction();
  await expect(surface.beginAction()).rejects.toThrow("already owns an action");
  await surface.endAction();
  await surface.beginAction();
  await surface.endAction();
  await expect(surface.navigate("http://chatgpt.com/")).rejects.toThrow("requires HTTPS");
  await expect(surface.navigate("https://chatgpt.com/", AbortSignal.abort())).rejects.toThrow("aborted");
  expect(fake.tabs[0]!.navigations).toHaveLength(1);
});

test("registered lanes own distinct fixed pages and simultaneous actions within one private context",async()=>{
  const {surface,fake}=await createSurface(undefined,true,"bones");
  const primary=await surface.page();
  const lane=await surface.createActionLane();
  const secondary=await lane.page();
  expect(secondary).not.toBe(primary);
  expect(primary.context()).toBe(secondary.context());
  expect(await surface.page()).toBe(primary);
  expect(fake.tabs[1]!.navigations).toEqual(["https://chatgpt.com/"]);
  // Rate-limit and transcript-load observers each listen for responses.
  expect(fake.tabs[1]!.listenerCount("response")).toBe(2);
  expect(fake.tabs[1]!.evaluatedScripts.at(-1)!.arg).toMatchObject({level:"bones"});
  await Promise.all([surface.beginAction(),lane.beginAction()]);
  await expect(surface.beginAction()).rejects.toThrow("already owns an action");
  await expect(lane.beginAction()).rejects.toThrow("already owns an action");
  await Promise.all([surface.endAction(),lane.endAction()]);
  await lane.navigate("https://chatgpt.com/g/fixture");
  expect(primary.url()).toBe("https://chatgpt.com/");
  expect(await lane.page()).toBe(secondary);
  expect(launchSpy!.mock.calls).toHaveLength(1);
});

test("lane allocation is bounded and racing allocations cannot exceed capacity",async()=>{
  const {surface,fake}=await createSurface();
  const allocations=await Promise.allSettled([surface.createActionLane(),surface.createActionLane()]);
  expect(allocations.filter(result=>result.status==="fulfilled")).toHaveLength(1);
  expect(allocations.filter(result=>result.status==="rejected")).toHaveLength(1);
  expect(fake.pages()).toHaveLength(2);
  const lane=(allocations.find(result=>result.status==="fulfilled") as PromiseFulfilledResult<ChromeSingleWindowSurface>).value;
  await expect(lane.createActionLane()).rejects.toThrow("Only the primary");
  await lane.close();
  const replacement=await surface.createActionLane();
  expect(await replacement.page()).not.toBe(await surface.page());
  expect(fake.pages()).toHaveLength(2);
});

test("configured four-page capacity remains in one context and rejects a fifth lane",async()=>{
  const {surface,fake}=await createSurface(undefined,false,undefined,4);
  const lanes=await Promise.all([surface.createActionLane(),surface.createActionLane(),surface.createActionLane()]);
  expect(new Set(await Promise.all([surface,...lanes].map(lane=>lane.page()))).size).toBe(4);
  await expect(surface.createActionLane()).rejects.toThrow("capacity");
  expect(fake.pages()).toHaveLength(4);
});

test("lane creation honors disabled capacity and rejects out-of-range limits before browser launch",async()=>{
  const {surface,fake}=await createSurface(undefined,false,undefined,1);
  await expect(surface.createActionLane()).rejects.toThrow("capacity");
  expect(fake.pages()).toHaveLength(1);
  for(const actionLaneLimit of [0,5,1.5]){
    await expect(ChromeSingleWindowSurface.create({profilePath:profilePath!,executablePath:fakeExecutablePath,actionLaneLimit}))
      .rejects.toThrow("between 1 and 4");
  }
  expect(launchSpy!.mock.calls).toHaveLength(1);
});

test("submission rate limits observed on one lane remain shared across its profile",async()=>{
  const {surface}=await createSurface();
  const primary=await surface.page();
  const lane=await surface.createActionLane();
  const page=await lane.page() as unknown as FakePage;
  page.emit("response",{url:()=>"https://chatgpt.com/backend-api/conversation",status:()=>429,
    request:()=>({method:()=>"POST"}),headerValue:async()=>"10"});
  expect(()=>throwIfConversationRateLimited(page as unknown as Page)).toThrow("rate limited");
  expect(()=>throwIfConversationRateLimited(primary)).toThrow("rate limited");
});

test("every lane rejects unmanaged tabs and duplicate concurrent ownership on the same page",async()=>{
  const {surface,fake}=await createSurface();
  const lane=await surface.createActionLane();
  const extra=await fake.newPage();
  await expect(surface.page()).rejects.toMatchObject({code:"external_interference"});
  await expect(lane.page()).rejects.toMatchObject({code:"external_interference"});
  await extra.close();
  const starts=await Promise.allSettled([lane.beginAction(),lane.beginAction()]);
  expect(starts.filter(result=>result.status==="fulfilled")).toHaveLength(1);
  expect(starts.filter(result=>result.status==="rejected")).toHaveLength(1);
  await lane.endAction();
});

test("lazy lane registration does not temporarily invalidate another owned page",async()=>{
  const {surface,fake}=await createSurface();
  const original=fake.newPage.bind(fake);
  let release!:()=>void;
  const registered=new Promise<void>(resolve=>{release=resolve;});
  let emitted!:()=>void;
  const created=new Promise<void>(resolve=>{emitted=resolve;});
  fake.newPage=async()=>{const page=await original();emitted();await registered;return page;};
  const allocating=surface.createActionLane();
  await created;
  let checked=false;
  const checking=surface.page().then(page=>{checked=true;return page;});
  await Bun.sleep(0);expect(checked).toBeFalse();
  release();
  const lane=await allocating;
  expect(await checking).not.toBe(await lane.page());
});

test("closing a lane preserves the primary and root close closes the context exactly once",async()=>{
  const {surface,fake}=await createSurface();
  const lane=await surface.createActionLane();
  let rootClosed=false;
  void surface.closed.then(()=>{rootClosed=true;});
  await Promise.all([lane.close(),lane.close()]);
  await lane.closed;
  expect(fake.closed).toBeFalse();
  expect(rootClosed).toBeFalse();
  expect(await surface.page()).toBeDefined();
  await expect(lane.page()).rejects.toMatchObject({code:"external_interference"});
  const replacement=await surface.createActionLane();
  await Promise.all([surface.close(),surface.close()]);
  await Promise.all([surface.closed,replacement.closed]);
  expect(fake.closeCalls).toBe(1);
  expect(fake.pages()).toHaveLength(0);
});

test("failed lane navigation closes only its page and permits another allocation",async()=>{
  const {surface,fake}=await createSurface();
  const original=fake.newPage.bind(fake);
  fake.newPage=async()=>{const page=await original();(page as unknown as FakePage).navigationError=new Error("lane navigation failed");return page;};
  await expect(surface.createActionLane()).rejects.toThrow("lane navigation failed");
  expect(fake.closed).toBeFalse();
  expect(fake.pages()).toHaveLength(1);
  expect(await surface.page()).toBeDefined();
  fake.newPage=original;
  expect(await (await surface.createActionLane()).page()).toBeDefined();
});

test("retiring the primary action page preserves a healthy registered sibling and browser lifecycle",async()=>{
  const {surface,fake}=await createSurface();
  const lane=await surface.createActionLane();
  const page=await lane.page();
  let rootClosed=false;void surface.closed.then(()=>{rootClosed=true;});
  await Promise.all([surface.closeActionPage(),surface.closeActionPage()]);
  expect(fake.closeCalls).toBe(0);
  expect(rootClosed).toBeFalse();
  await expect(surface.page()).rejects.toMatchObject({code:"external_interference"});
  await lane.beginAction();expect(await lane.page()).toBe(page);await lane.endAction();
  await surface.close();await surface.closed;await lane.closed;
  expect(fake.closeCalls).toBe(1);
});

test("presentation is shared by all idle lanes and cannot update during any lane action",async()=>{
  const {surface,fake}=await createSurface(undefined,true,"bones",2,true);
  const lane=await surface.createActionLane();
  await lane.beginAction();
  await expect(surface.setPresentationStyle("body { color: red; }")).rejects.toThrow("busy");
  await lane.endAction();
  const receipt=await surface.setPresentationStyle("body { color: red; }");
  expect(receipt.revision).toBe(1);
  expect(await readFile(join(profilePath!,"presentation.css"),"utf8")).toBe("body { color: red; }");
  for(const page of fake.tabs)expect(page.evaluatedScripts.at(-1)!.arg).toMatchObject({revision:1,level:"bones"});
  expect(styled(fake.initScripts)).toHaveLength(2);
  const again=await lane.setPresentationStyle("body { color: red; }");
  expect(again.revision).toBe(1);
  expect(styled(fake.initScripts)).toHaveLength(2);
});

test("shared presentation reserves maintenance before asynchronous page validation",async()=>{
  const {surface,fake}=await createSurface(undefined,true,"bones",2,true);
  const lane=await surface.createActionLane();
  let release!:()=>void,started!:()=>void;
  const gate=new Promise<void>(resolve=>{release=resolve;}),validating=new Promise<void>(resolve=>{started=resolve;});
  const evaluate=fake.tabs[0]!.evaluate.bind(fake.tabs[0]);
  fake.tabs[0]!.evaluate=async(script,arg)=>{started();await gate;return evaluate(script,arg);};
  const updating=surface.setPresentationStyle("body { color: blue; }");
  await validating;
  await expect(lane.beginAction()).rejects.toThrow("presentation is being updated");
  await expect(surface.createActionLane()).rejects.toThrow("presentation is being updated");
  release();await updating;
  await lane.beginAction();await lane.endAction();
});

test("startup closes Chrome when initial navigation fails", async () => {
  const page = new FakePage();
  page.navigationError = new Error("navigation failed");
  await expect(createSurface([page])).rejects.toThrow("navigation failed");
  expect(page.closed).toBeTrue();
});

test("startup selects the restored ChatGPT tab before an empty startup tab", async () => {
  const blank = new FakePage(), restored = new FakePage();
  restored.currentUrl = "https://chatgpt.com/c/restored";
  const {surface} = await createSurface([blank, restored]);
  expect(await surface.page()).toBe(restored as unknown as Page);
  expect(restored.navigations).toEqual([]);
  expect(blank.closed).toBeTrue();
});

test("startup tolerates an aborted goto that already restored ChatGPT", async () => {
  const page = new FakePage();
  page.goto = async () => {
    page.currentUrl = "https://chatgpt.com/c/restored";
    throw new Error("page.goto: net::ERR_ABORTED; maybe frame was detached?");
  };
  const {surface} = await createSurface([page]);
  expect(await surface.page()).toBe(page as unknown as Page);
  expect(page.closed).toBeFalse();
  expect(page.url()).toBe("https://chatgpt.com/c/restored");
});

test("startup retries an interrupted blank tab only once", async () => {
  const page = new FakePage();
  let attempts = 0;
  page.goto = async url => {
    attempts++;
    if (attempts === 1) throw new Error("page.goto: net::ERR_ABORTED");
    page.currentUrl = url;
  };
  const {surface} = await createSurface([page]);
  expect(attempts).toBe(2);
  expect(await surface.page()).toBe(page as unknown as Page);
});

test("persistent startup navigation aborts still close the failed browser", async () => {
  const page = new FakePage();
  page.navigationError = new Error("page.goto: net::ERR_ABORTED");
  await expect(createSurface([page])).rejects.toThrow("ERR_ABORTED");
  expect(page.closed).toBeTrue();
});

test.skipIf(process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME !== "1")(
  "real Chrome preserves the DOM observer cache and composer in Patchright's default isolated context",
  async () => {
    profilePath = await mkdtemp(join(tmpdir(), "browser-chrome-integration-"));
    const launch = chromium.launchPersistentContext.bind(chromium);
    launchSpy = spyOn(chromium, "launchPersistentContext").mockImplementation(async (profile, options) => {
      const real = await launch(profile, options);
      context = real;
      await real.route("https://chatgpt.com/**", async route => {
        if (new URL(route.request().url()).pathname === "/api/auth/session") {
          await route.fulfill({ json:{ user:{ id:"fixture-user" }, account:{ id:"fixture-account", planType:"plus", structure:"personal" } } });
        } else {
          await route.fulfill({ contentType:"text/html", body:`<!doctype html>
            <form><div id="prompt-textarea" contenteditable="true" role="textbox"></div></form>
            <section data-turn-id-container="assistant-one"><article data-turn-id="assistant-one" data-testid="conversation-turn-1">
              <div data-message-author-role="assistant"><div class="markdown"><p>Fixture answer</p></div></div>
              <button data-testid="copy-turn-action-button">Copy</button>
            </article></section>` });
        }
      });
      return real;
    });
    const executablePath = await privateChromeExecutable(join(import.meta.dirname, "../.browsers", `${process.platform}-${process.arch}`));
    const surface = await ChromeSingleWindowSurface.create({ profilePath, executablePath });
    try {
      const page = await surface.page();
      expect(page.viewportSize()).toBeNull();
      const composer = await activeComposer(page);
      const prompt = "Exact  fixture prompt";
      await insertExactPrompt(composer, prompt);
      expect(await composerText(composer)).toBe(prompt);
      expect((await readChatGptAccount(page)).planType).toBe("plus");
      expect((await readTurnDomState(page)).responseIdentities).toEqual(["assistant-one"]);
      const turn = page.locator('[data-turn-id="assistant-one"]');
      const cache: ChatGptResponseDomCache = {};
      const first = await responseDomSnapshot(turn, cache);
      expect(first.visibleText).toContain("Fixture answer");
      expect(first.completionActionVisible).toBeTrue();
      expect(await responseDomSnapshot(turn, cache)).toEqual(first);
      expect(cache.cacheHits).toBe(1);
      await turn.locator(".markdown p").evaluate(element => { element.textContent = "Updated answer"; });
      expect((await responseDomSnapshot(turn, cache)).visibleText).toContain("Updated answer");
      expect(cache.fullScans).toBe(2);
      const lane=await surface.createActionLane();
      const lanePage=await lane.page();
      expect(lanePage).not.toBe(page);
      expect(lanePage.context()).toBe(page.context());
      await Promise.all([surface.beginAction(),lane.beginAction()]);
      const laneComposer=await activeComposer(lanePage);
      await insertExactPrompt(laneComposer,"Independent lane prompt");
      expect(await composerText(laneComposer)).toBe("Independent lane prompt");
      expect(await composerText(composer)).toBe(prompt);
      expect(await surface.page()).toBe(page);
      await Promise.all([surface.endAction(),lane.endAction()]);
      await lane.close();await lane.closed;
      expect(page.isClosed()).toBeFalse();
      await page.close();
      const closed = await Promise.race([surface.closed.then(()=>true),Bun.sleep(2_000).then(()=>false)]);
      expect(closed).toBeTrue();
    } finally {
      await surface.close();
    }
  },
  60_000,
);


test("closing the last Chrome page releases the launcher even if the browser context stays alive",async()=>{
  const {surface,fake}=await createSurface();
  await fake.tabs[0]!.close();await surface.closed;
  expect(fake.closed).toBeFalse();
  await expect(surface.page()).rejects.toMatchObject({code:"external_interference"});
});

test("closing the shared page preserves other open pages until the last window is closed",async()=>{
  const {surface,fake}=await createSurface();const other=await fake.newPage();let stopped=false;
  void surface.closed.then(()=>{stopped=true;});
  await fake.tabs[0]!.close();await Bun.sleep(0);expect(stopped).toBeFalse();
  expect((other as unknown as FakePage).closed).toBeFalse();
  await other.close();await surface.closed;expect(stopped).toBeTrue();
});
