import assert from "node:assert/strict";
import {mkdtemp,rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import {join,resolve} from "node:path";
import {chromium} from "patchright";
import {privateChromeExecutable} from "../src/chrome-executable";
import {activateChatGptEffortMenu,CHATGPT_EFFORT_CONTROL_SELECTOR} from "../src/session";

const profile=await mkdtemp(join(tmpdir(),"codexify-effort-control-"));
const executablePath=await privateChromeExecutable(resolve(".browsers",`${process.platform}-${process.arch}`));
const context=await chromium.launchPersistentContext(profile,{executablePath,headless:true,timeout:20_000});
try {
  const page=context.pages()[0]!;
  await page.route("https://chatgpt.com/**",route=>route.fulfill({contentType:"text/html",body:`
    <form><button type="button" data-tone="neutral" aria-haspopup="menu" aria-expanded="false" aria-controls="picker">Extra High</button></form>
    <div id="picker" role="menu" hidden><div data-model-reasoning-effort-slider><div role="slider" aria-valuemin="0" aria-valuemax="3" aria-valuenow="3">Extra High</div></div></div>
    <script>document.querySelector('button').onclick=event=>{const b=event.currentTarget;b.dataset.tone='active';b.setAttribute('aria-expanded','true');document.querySelector('#picker').hidden=false;document.body.dataset.clicks=String(Number(document.body.dataset.clicks||0)+1);};</script>`}));
  await page.goto("https://chatgpt.com/",{waitUntil:"domcontentloaded"});
  const control=page.locator(CHATGPT_EFFORT_CONTROL_SELECTOR);
  let timeout:ReturnType<typeof setTimeout>|undefined;
  const started=performance.now();
  const activated=await Promise.race([activateChatGptEffortMenu(page,control,{settleMs:500}),new Promise<never>((_yes,no)=>{timeout=setTimeout(()=>no(new Error("Model control style change stalled the selector instead of opening its owned picker")),3_000);})]).finally(()=>clearTimeout(timeout));
  assert.equal(activated.method,"click");
  assert.equal(await page.locator('button[data-tone="neutral"]').count(),0);
  assert.equal(await control.count(),1,"The model control must remain addressable while open");
  assert(performance.now()-started<3_000);
  const aborted=new AbortController();aborted.abort();
  await assert.rejects(activateChatGptEffortMenu(page,control,{signal:aborted.signal}),{name:"AbortError"});
  assert.equal(await page.locator('body').getAttribute('data-clicks'),'1');
  console.log(JSON.stringify({ok:true,styleChangePreservesOwnedControl:true,abortedActivationDoesNotClick:true}));
} finally {
  await context.close();
  await rm(profile,{recursive:true,force:true,maxRetries:8,retryDelay:200});
}
