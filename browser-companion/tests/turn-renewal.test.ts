import {expect,test} from "bun:test";
import {handoffReportReady} from "../src/turn-renewal";
import {renderActionPrompt} from "../src/protocol";
const id=`renew-${"a".repeat(64)}`;
const proof={answer:`Turn handoff ${id}\nWork performed and next steps.`,responsePresent:true,generationRunning:false,completionActionVisible:true};
test("renewal requires the new completed final, never idle or an older answer",()=>{
 expect(handoffReportReady(proof,id)).toBe(true);
 for(const altered of [{generationRunning:true},{responsePresent:false},{completionActionVisible:false},{answer:"Old final answer"},{answer:`Turn handoff ${id}`},{answer:`Turn handoff ${"renew-"+"b".repeat(64)}\nOther turn`}])expect(handoffReportReady({...proof,...altered},id)).toBe(false);
 expect(handoffReportReady(proof,"invalid")).toBe(false);
});
test("continuation is plain and does not mirror queued work or mention the preceding report",()=>{
 const prompt=renderActionPrompt({kind:"turn_renewal",handoffId:id});expect(prompt.startsWith("Continue.\n")).toBe(true);expect(prompt).not.toContain("previous final");expect(prompt).not.toContain("codexify-unread");expect(prompt).not.toContain("goal_finish");
});

import {chromium} from "patchright";
import {privateChromeExecutable} from "../src/chrome-executable";
import {fingerprintAccountIdentity} from "../src/account";
import {completedHandoff} from "../src/turn-renewal";
import {mkdtemp,rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import {join} from "node:path";
import {ChatGptBrowserDriver,type SingleBrowserSurface} from "../src/chatgpt-driver";
import {protocolBuild} from "../src/protocol";
const native=process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME==="1";
test.skipIf(!native)("handoff polls reuse the current document and bound visits after another chat takes the tab",async()=>{
 const executablePath=await privateChromeExecutable(join(import.meta.dirname,'../.browsers',`${process.platform}-${process.arch}`));
 const browser=await chromium.launch({executablePath,headless:true});
 try{
  const context=await browser.newContext(),page=await context.newPage(),url='https://chatgpt.com/c/handoff-poll',account=fingerprintAccountIdentity('user','account');let running=false,loads=0;
  await context.route('https://chatgpt.com/**',route=>new URL(route.request().url()).pathname==='/api/auth/session'?route.fulfill({json:{user:{id:'user'},account:{id:'account',structure:'personal',planType:'pro'}}}):route.fulfill({contentType:'text/html',body:`<article data-testid="conversation-turn-1" data-turn="assistant" data-turn-id="final"><div class="markdown"><p>Turn handoff ${id}</p><p>Work performed. Current state. Next steps.</p></div><button data-testid="copy-turn-action-button">Copy</button></article>${running?'<button data-testid="stop-button">Stop</button>':''}`}));
  const surface:SingleBrowserSurface={page:async()=>page,navigate:async target=>{loads++;await page.goto(target);return page;},reveal:async()=>{},beginAction:async()=>{},endAction:async()=>{},close:async()=>{}};
  const driver=new ChatGptBrowserDriver(surface,protocolBuild('handoff-navigation-test'));
  await page.goto(url);expect(await driver.turnHandoffComplete(url,account,id)).toBe(true);expect(loads).toBe(0);
  running=true;await page.goto('https://chatgpt.com/c/other');expect(await driver.turnHandoffComplete(url,account,id)).toBe(false);expect(loads).toBe(1);
  await page.goto('https://chatgpt.com/c/other');await expect(driver.turnHandoffComplete(url,account,id)).rejects.toMatchObject({code:'editor_unavailable'});expect(loads).toBe(1);
  running=false;await page.goto(url);expect(await driver.turnHandoffComplete(url,account,id)).toBe(true);expect(loads).toBe(1);
 }finally{await browser.close();}
},15_000);
for(const grouped of [false,true])test.skipIf(!native)(`real Chrome proves the exact final and rejects running, old, changing and wrong-account reports (${grouped?"grouped":"legacy"})`,async()=>{
 const profile=await mkdtemp(join(tmpdir(),"turn-renewal-fixture-"));const executablePath=await privateChromeExecutable(join(import.meta.dirname,"../.browsers",`${process.platform}-${process.arch}`));
 const context=await chromium.launchPersistentContext(profile,{channel:"chrome",executablePath,headless:true});
 try {
  const page=context.pages()[0]!;const url="https://chatgpt.com/c/renewal-fixture";const account=fingerprintAccountIdentity("user","account");
  await context.route("https://chatgpt.com/**",route=>new URL(route.request().url()).pathname==="/api/auth/session"?route.fulfill({json:{user:{id:"user"},account:{id:"account",structure:"personal",planType:"pro"}}}):route.fulfill({contentType:"text/html",body:"<!doctype html><html><body></body></html>"}));await page.goto(url);
  const render=async(answer:string,running=false)=>{
   await page.evaluate(({answer,running,grouped})=>{
    const content=`<div class="markdown"><p></p><p>Work performed. Current state. Next steps.</p></div><button data-testid="copy-turn-action-button">Copy</button>`;
    document.body.innerHTML=grouped?`<section data-turn-key="key"><div data-content-search-unit-key="final"><div data-conversation-role="assistant">${content}</div></div></section>`:`<article data-testid="conversation-turn-1" data-turn="assistant" data-turn-id="final">${content}</article>`;
    document.querySelector('p')!.textContent=answer;if(running)document.body.insertAdjacentHTML('beforeend','<button data-testid="stop-button">Stop</button>');
   },{answer,running,grouped});
  };
  await render(`Turn handoff ${id}`);expect(await completedHandoff(page,url,account,id)).toBe(true);
  await page.evaluate(grouped=>document.body.insertAdjacentHTML('beforeend',grouped
    ? '<section data-turn-key="later-user"><div data-user-message-bubble>Continue.</div></section>'
    : '<article data-testid="conversation-turn-2" data-turn="user" data-turn-id="later-user">Continue.</article>'),grouped);
  expect(await completedHandoff(page,url,account,id)).toBe(false);
  await render(`Turn handoff ${id}`);
  await render(`Turn handoff ${id}`,true);expect(await completedHandoff(page,url,account,id)).toBe(false);
  await render("An old final answer");expect(await completedHandoff(page,url,account,id)).toBe(false);
  await render(`Turn handoff ${id}`);expect(await completedHandoff(page,url,"wrong-account",id)).toBe(false);expect(await completedHandoff(page,url.replace("renewal-fixture","other"),account,id)).toBe(false);
  await page.evaluate(()=>setTimeout(()=>document.querySelector('.markdown p:last-child')!.textContent+=' changed',250));expect(await completedHandoff(page,url,account,id)).toBe(false);
  await render(`Turn handoff ${id}`);await page.locator('[data-testid="copy-turn-action-button"]').evaluate(e=>e.removeAttribute('data-testid'));expect(await completedHandoff(page,url,account,id)).toBe(false);
 }finally{await context.close();await rm(profile,{recursive:true,force:true});}
},20_000);
