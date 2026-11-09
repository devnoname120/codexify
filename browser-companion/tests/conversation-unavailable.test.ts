import { afterAll, beforeAll, beforeEach, expect, setSystemTime, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chromium, type BrowserContext, type Page } from "patchright";
import { fingerprintAccountIdentity } from "../src/account";
import { SingleWindowCoordinator } from "../src/browser-window";
import { throwIfSessionAttention } from "../src/browser-session";
import { ChatGptBrowserDriver, type SingleBrowserSurface } from "../src/chatgpt-driver";
import { privateChromeExecutable } from "../src/chrome-executable";
import { protocolBuild, type ActionPrompt, type ExecuteActionRequest } from "../src/protocol";
import {navigatePage,observeConversationLoadResponses,conversationLoadWasRateLimited,conversationLoadAvailableAt} from "../src/navigation";

const native = process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME === "1";
const build = protocolBuild("conversation-error-test");
const url = "https://chatgpt.com/c/unavailable-fixture";
const projectUrl = "https://chatgpt.com/g/g-p-fixture-project/c/unavailable-fixture";
const message = "ChatGPT conversation temporarily not accessible";
const account = fingerprintAccountIdentity("fixture-user", "fixture-account");
const labels = [
  ["Could not load this ChatGPT conversation", "Retry"],
  ["Δεν ήταν δυνατή η φόρτωση αυτής της συνομιλίας ChatGPT", "Επανάληψη"],
  ["Impossibile caricare questa conversazione ChatGPT", "Riprova"],
] as const;

// Captured page structure: no error test ID or ARIA alert, and no composer.
// Keep the generated class names in the fixture, but never use them as selectors.
function errorPage(title: string, retry: string): string {
  return `<div class="WorkspaceContent-dnr6uY relative isolate flex min-h-0 flex-1 overflow-hidden">
    <div class="MainContentViewport-OinMfT" data-app-shell-main-content-layout="default">
      <div class="MainContentFrame-sss8BP" data-app-shell-thread-edge-divider="false">
        <div class="relative flex min-h-0 flex-1">
          <div class="flex h-full min-h-0 min-w-0 flex-1 flex-col outline-none" data-app-shell-focus-area="main">
            <div class="relative h-full min-h-0"><div class="w-full items-center px-3 flex flex-col justify-center py-6">
              <div class="flex w-full max-w-xl flex-col items-center text-center justify-center gap-3">
                <div class="flex flex-col items-center gap-2"><div class="leading-6 font-medium text-default text-lg">${title}</div></div>
                <div class="flex w-full flex-wrap items-center justify-center gap-2"><button type="button" class="no-drag cursor-interaction">${retry}</button></div>
              </div>
            </div></div>
          </div>
        </div>
      </div>
    </div>
  </div>`;
}

// Captured inline transcript failure: its translated message and retry label
// have no error test ID, heading, ARIA alert or centered full-page panel.
function inlineErrorPage(title:string,retry:string):string {
  return `<div data-request-input-activity-root="true"><div role="region" data-app-action-timeline-scroll="">
    <div data-mcp-app-portal-target="true" data-thread-user-message-navigation-content="true">
      <div data-chatgpt-conversation-selection-target="true" data-thread-find-target="conversation">
        <span hidden></span><div class="rounded-xl border border-default bg-surface-secondary/92 p-4 text-sm text-default">${title}<button type="button">${retry}</button></div><div aria-hidden="true" style="height:0px"></div>
      </div>
    </div></div></div>`;
}

function identifiedInlineErrorPage(title:string,retry:string):string {
  return `<main data-app-shell-main-surface="browser"><div data-app-shell-focus-area="main">${inlineErrorPage(title,retry)
    .replace('data-chatgpt-conversation-selection-target="true"','data-chatgpt-conversation-selection-target="6ad55395-b6ac-83eb-8d39-691e7e1f855f"')
    .replace('<button type="button">','<button type="button" disabled>')}</div></main>`;
}

const healthyPage = '<form data-chatgpt-composer><div id="prompt-textarea" contenteditable="true" style="min-height:24px"></div></form>';
let context: BrowserContext;
let page: Page;
let profile: string;
let currentHtml = errorPage(...labels[0]);
let sessionRequests = 0;
let signedOut=false;
let clicks = 0;
const documentLoads: string[] = [];
const surface: SingleBrowserSurface = {
  page:async () => page,
  navigate:async destination => { await page.goto(destination); return page; },
  reveal:async () => {}, beginAction:async () => {}, endAction:async () => {}, close:async () => {},
};

beforeAll(async () => {
  if (!native) return;
  profile = await mkdtemp(join(tmpdir(), "conversation-error-fixture-"));
  const executablePath = await privateChromeExecutable(join(import.meta.dirname, "../.browsers", `${process.platform}-${process.arch}`));
  context = await chromium.launchPersistentContext(profile, {channel:"chrome", executablePath, headless:true});
  page = context.pages()[0]!;
  observeConversationLoadResponses(page);
  await page.exposeFunction("recordClick", () => { clicks++; });
  await context.route("https://chatgpt.com/**", async route => {
    if (new URL(route.request().url()).pathname === "/api/auth/session") {
      sessionRequests++;
      await route.fulfill({json:signedOut?{}:{user:{id:"fixture-user"}, account:{id:"fixture-account", planType:"pro", structure:"personal"}}});
    } else {
      documentLoads.push(route.request().url());
      await route.fulfill({contentType:"text/html", body:`<!doctype html><meta charset="utf-8">${currentHtml}<script>document.addEventListener('click',()=>window.recordClick());</script>`});
    }
  });
}, 30_000);
beforeEach(async()=>{
  if(!native)return;
  currentHtml=healthyPage;
  await surface.navigate(url);
  await new ChatGptBrowserDriver(surface,build).health();
});
afterAll(async () => { await context?.close(); if (profile) await rm(profile, {recursive:true, force:true});setSystemTime(); });

test.skipIf(!native)("conversation load failures are recognized in English, Greek and Italian on both chat URL forms", async () => {
  for (const render of [errorPage,inlineErrorPage,identifiedInlineErrorPage]) for (const [title, retry] of labels) {
    currentHtml = render(title, retry);
    for (const destination of [url, projectUrl]) {
      await surface.navigate(destination);
      const error = await throwIfSessionAttention(page).then(() => undefined, error => error);
      expect(error).toMatchObject({code:"conversation_unavailable", message});
    }
  }
  expect(clicks).toBe(0);
});

test.skipIf(!native)("a failed conversation keeps its verified account signed in and a healthy conversation clears the cooldown", async () => {
  for (const render of [errorPage,inlineErrorPage,identifiedInlineErrorPage]) {
  currentHtml = render("Unavailable", "Another language");
  await surface.navigate(url);
  const driver = new ChatGptBrowserDriver(surface, build);
  const before = sessionRequests;
  expect(await driver.health()).toEqual({authenticated:true, accountFingerprint:account, status:"idle", detail:message});
  expect(sessionRequests).toBe(before + 1);
  currentHtml = healthyPage;
  await surface.navigate("https://chatgpt.com/c/another-fixture");
  expect(await driver.health()).toEqual({authenticated:true, accountFingerprint:account, status:"idle", detail:null});
  expect(context.pages()).toEqual([page]);
  }
});

test.skipIf(!native)("attempted recovery loads retain the specific conversation error before Send", async () => {
  const prompts: ActionPrompt[] = [
    {kind:"offline_chat", batchId:"batch", messages:[{id:"message", markdown:"Continue"}]},
    {kind:"offline_goal", goalRevision:1},
  ];
  for (const render of [errorPage,inlineErrorPage,identifiedInlineErrorPage]) for (const [index, prompt] of prompts.entries()) {
    const failedUrl = index === 0 ? url : projectUrl;
    currentHtml = render("Unavailable", "Riprova");
    await surface.navigate(failedUrl);
    const coordinator = new SingleWindowCoordinator(new ChatGptBrowserDriver(surface, build), build);
    const request: ExecuteActionRequest = {
      actionId:`unavailable-${render.name}-${index}`, idempotencyKey:`unavailable-${render.name}-${index}`, url:failedUrl, expectedUrl:failedUrl,
      pairingGeneration:1, connectorName:"Fixture connector", expectedAccountFingerprint:account,
      model:"gpt-6-sol", effort:"high", prompt, autoApproveToolCalls:false, bypassConversationCooldown:true,
    };
    await coordinator.execute(request);
    const events = [];
    for await (const event of coordinator.events(request.actionId, 0)) events.push(event);
    expect(events.at(-1)).toMatchObject({type:"failed", code:"conversation_unavailable", message:expect.stringContaining(message)});
    expect(events.some(event => event.type === "authentication_required" || event.type === "send_accepted")).toBeFalse();
    expect(await coordinator.action(request.actionId)).toMatchObject({state:"failed", sendAccepted:false, attention:{code:"conversation_unavailable", message:expect.stringContaining(message)}});
    expect((await coordinator.health()).activeActionId).toBeNull();
  }
  expect(clicks).toBe(0);
},20_000);

test.skipIf(!native)("ordinary content, hidden panels, other routes and incomplete error structures are not conversation load failures", async () => {
  const html = errorPage(...labels[0]);
  const inline=inlineErrorPage(...labels[0]);
  const cases = [
    inline.replace('data-request-input-activity-root="true"','data-unrelated="true"'),
    inline.replace('data-thread-user-message-navigation-content="true"','data-unrelated="true"'),
    inline.replace('data-thread-find-target="conversation"','data-thread-find-target="other"'),
    inline.replace('class="rounded-xl','hidden class="rounded-xl'),
    inline.replace('<button type="button"','<button type="button">Other</button><button type="button"'),
    inline.replace('<span hidden></span>','<article>Ordinary conversation content</article>'),
    inline.replace('<span hidden></span>',healthyPage),
    `<article data-message-author-role="assistant">${inline}</article>`,
    html.replace('data-app-shell-focus-area="main"', 'data-unrelated="main"'),
    html.replace('class="WorkspaceContent-dnr6uY', 'hidden class="WorkspaceContent-dnr6uY'),
    html.replace('<button type="button"', '<button type="button">Other action</button><button type="button"'),
    html.replace('text-default text-lg', 'text-default'),
    html.replace('<div class="relative h-full min-h-0">', '<div data-message-author-role="assistant" class="relative h-full min-h-0">'),
    html.replace('<div class="relative h-full min-h-0">', healthyPage + '<div class="relative h-full min-h-0">'),
  ];
  for (const body of cases) {
    currentHtml = body;
    await surface.navigate(url);
    await throwIfSessionAttention(page);
  }
  currentHtml = html;
  for (const destination of ["https://chatgpt.com/", "https://chatgpt.com/settings", "https://chatgpt.com/auth/login"]) {
    await surface.navigate(destination);
    await throwIfSessionAttention(page);
  }
});

test.skipIf(!native)("a sign-in page remains an authentication failure", async () => {
  currentHtml = '<a href="/auth/login">Sign in</a>';
  await surface.navigate("https://chatgpt.com/auth/login");
  expect(await new ChatGptBrowserDriver(surface, build).health()).toMatchObject({authenticated:false, status:"authentication_required"});
});

test.skipIf(!native)('repeated signed-out retries each reload the open conversation once and remain confirmed unsent',async()=>{
 signedOut=true;
 try{
  setSystemTime(Date.now()+300_001);
  currentHtml='<a href="/auth/login">Sign in</a>';await surface.navigate(projectUrl);
  const coordinator=new SingleWindowCoordinator(new ChatGptBrowserDriver(surface,build),build);
  for(const attempt of [1,2]){
   if(attempt===2)setSystemTime(Date.now()+180_001);
   const request:ExecuteActionRequest={actionId:`signed-out-${attempt}`,idempotencyKey:`signed-out-${attempt}`,url:projectUrl,expectedUrl:projectUrl,pairingGeneration:1,connectorName:'Fixture connector',expectedAccountFingerprint:account,model:'gpt-6-sol',effort:'high',prompt:{kind:'offline_goal',goalRevision:1},autoApproveToolCalls:false};
   const before=documentLoads.length;await coordinator.execute(request);
   const events=[];for await(const event of coordinator.events(request.actionId,0))events.push(event);
   expect(documentLoads.slice(before)).toEqual([projectUrl]);
   expect(events.at(-1)?.type).toBe('authentication_required');
   expect((await coordinator.action(request.actionId))?.sendAccepted).toBeFalse();
   expect((await coordinator.health()).activeActionId).toBeNull();
  }
  expect(clicks).toBe(0);
 }finally{signedOut=false;}
},15_000);

test.skipIf(!native)("all chats share five-minute access recovery slots and a healthy load restores normal navigation",async()=>{
  currentHtml=identifiedInlineErrorPage(...labels[0]);await surface.navigate(projectUrl);
  const controlled:SingleBrowserSurface={...surface,navigate:async(destination,signal)=>{await navigatePage(page,destination,{signal});return page;}};
  const driver=new ChatGptBrowserDriver(controlled,build),coordinator=new SingleWindowCoordinator(driver,build);
  const failedAt=Date.now();
  expect(await driver.health()).toMatchObject({authenticated:true,detail:message});
  const base:ExecuteActionRequest={actionId:'cooldown-initial',idempotencyKey:'cooldown-initial',url:projectUrl,expectedUrl:projectUrl,pairingGeneration:1,connectorName:'Fixture connector',expectedAccountFingerprint:account,model:'gpt-6-sol',effort:'high',prompt:{kind:'offline_goal',goalRevision:1},autoApproveToolCalls:false};
  const run=async(id:string,destination:string,loads:string[])=>{
    const request={...base,actionId:id,idempotencyKey:id,url:destination,expectedUrl:destination};
    const before=documentLoads.length;await coordinator.execute(request);
    const events=[];for await(const event of coordinator.events(id,0))events.push(event);
    expect(documentLoads.slice(before)).toEqual(loads);
    expect(events.at(-1)).toMatchObject(loads.length?{type:'failed',code:'conversation_unavailable',message:expect.stringContaining(message)}:{type:'waiting',availableAtMs:expect.any(Number)});
    expect(events.some(event=>event.type==='authentication_required'||event.type==='send_accepted')).toBeFalse();
    expect((await coordinator.action(id))?.sendAccepted).toBeFalse();
    expect((await coordinator.health()).activeActionId).toBeNull();
  };
  await run('cooldown-initial',projectUrl,[]);
  setSystemTime(failedAt+299_000);await driver.health();
  await run('cooldown-another-too-soon',url,[]);
  setSystemTime(failedAt+301_000);
  await run('cooldown-reload-slot',projectUrl,[projectUrl]);
  await run('cooldown-another-blocked',url,[]);
  setSystemTime(Date.now()+300_000);
  await run('cooldown-another-slot',url,[url]);
  setSystemTime(Date.now()+300_000);currentHtml=healthyPage;
  await navigatePage(page,projectUrl);
  expect(await driver.health()).toMatchObject({authenticated:true,status:'idle',detail:null});
  const recoveredAt=Date.now();setSystemTime(recoveredAt+179_000);
  await expect(navigatePage(page,url)).rejects.toMatchObject({code:'conversation_cooldown'});
  setSystemTime(recoveredAt+180_001);
  const before=documentLoads.length;await navigatePage(page,url);
  expect(documentLoads.slice(before)).toEqual([url]);
  expect(context.pages()).toEqual([page]);expect(clicks).toBe(0);
},20_000);

test.skipIf(!native)("transcript API 429s are load failures, honor Retry-After, and exclude Send, lists and other chats",async()=>{
  currentHtml=errorPage(...labels[0]);await surface.navigate(url);
  let status=429;
  await context.route('https://chatgpt.com/backend-api/**',route=>route.fulfill({status,headers:{'retry-after':'900'},json:{}}));
  const detail='/backend-api/conversations/unavailable-fixture?num_turns=10';
  const began=Date.now();
  expect(await page.evaluate(async path=>(await fetch(path)).status,detail)).toBe(429);
  for(let i=0;i<100&&!conversationLoadWasRateLimited(page);i++)await Bun.sleep(10);
  expect(conversationLoadWasRateLimited(page)).toBeTrue();
  await expect(throwIfSessionAttention(page)).rejects.toMatchObject({code:'conversation_load_rate_limited'});
  expect(conversationLoadAvailableAt(page,'https://chatgpt.com/c/another')!).toBeGreaterThanOrEqual(began+900_000);
  expect((await new ChatGptBrowserDriver(surface,build).health()).authenticated).toBeTrue();
  status=200;await page.evaluate(async path=>fetch(path),detail);
  for(let i=0;i<100&&conversationLoadWasRateLimited(page);i++)await Bun.sleep(10);
  expect(conversationLoadWasRateLimited(page)).toBeFalse();
  status=429;
  for(const [path,method] of [[detail,'POST'],['/backend-api/conversations?limit=20','GET'],['/backend-api/conversations/another-chat','GET'],['/backend-api/account/plan','GET']]){
    await page.evaluate(async({path,method})=>fetch(path,{method}),{path:path!,method:method!});
    await Bun.sleep(20);expect(conversationLoadWasRateLimited(page)).toBeFalse();
  }
  await context.unroute('https://chatgpt.com/backend-api/**');
},20_000);
