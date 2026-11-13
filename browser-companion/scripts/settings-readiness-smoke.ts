import assert from "node:assert/strict";
import {resolve} from "node:path";
import {chromium} from "patchright";
import {privateChromeExecutable} from "../src/chrome-executable";
import {installMinimalChatGptPage} from "../src/minimal-page";
import {inspectConnectorSettings} from "../src/account-settings";
import {readChatGptAccount} from "../src/account";

const executablePath=process.env.CODEXIFY_TEST_CHROME_PATH??await privateChromeExecutable(resolve(".browsers",`${process.platform}-${process.arch}`));
const browser=await chromium.launch({executablePath,headless:true,timeout:20_000});
try {
  const context=await browser.newContext();await installMinimalChatGptPage(context,"",0,"bones");
  let delayMs=5_500,accountId="fixture-account",changeAfterSessionRead=false,sessionReads=0;
  const settingsUrl="https://chatgpt.com/settings/plugins-settings/plugin_asdk_app_fixture";
  await context.route("**/*",async route=>{
    if(new URL(route.request().url()).pathname==="/api/auth/session"){
      sessionReads++;await route.fulfill({json:{user:{id:"fixture-user"},account:{id:accountId,planType:"pro",structure:"personal"}}});
      if(changeAfterSessionRead){changeAfterSessionRead=false;accountId="changed-account";}
      return;
    }
    await route.fulfill({contentType:"text/html",body:`<!doctype html><div id="loading" role="progressbar">Loading</div><main hidden><header><h1>Settings</h1></header><h2 id="connector">Codexify</h2><button data-action="refresh-tools" id="refresh">Refresh tools</button></main><script>document.body.dataset.clicks='0';document.querySelector('#refresh').onclick=()=>document.body.dataset.clicks=String(Number(document.body.dataset.clicks)+1);setTimeout(()=>{document.querySelector('main').hidden=false;document.querySelector('#loading').remove();},${delayMs});</script>`});
  });
  const page=await context.newPage();await page.goto(settingsUrl);
  const fingerprint=(await readChatGptAccount(page)).fingerprint;
  const started=performance.now();await inspectConnectorSettings(page,"asdk_app_fixture","Codexify",fingerprint);const hydratedMs=performance.now()-started;
  assert(hydratedMs>5_000&&hydratedMs<15_000,"Settings must hydrate after the former five-second budget");
  assert.equal(await page.locator('#connector').isVisible(),true);assert.equal(await page.locator('[data-refresh-tools-target="true"]').count(),1);

  delayMs=2_000;await page.goto(settingsUrl);
  const cancelled=new AbortController();const timer=setTimeout(()=>cancelled.abort(),60);const cancellingAt=performance.now();
  try {await assert.rejects(inspectConnectorSettings(page,"asdk_app_fixture","Codexify",fingerprint,cancelled.signal),{name:"AbortError"});}
  finally {clearTimeout(timer);}
  const cancelledMs=performance.now()-cancellingAt;assert(cancelledMs<1_000);
  assert.equal(await page.locator('[data-refresh-tools-target="true"]').count(),0);

  delayMs=200;accountId="fixture-account";await page.goto(settingsUrl);changeAfterSessionRead=true;
  const readsBefore=sessionReads;
  await assert.rejects(inspectConnectorSettings(page,"asdk_app_fixture","Codexify",fingerprint),/account changed/);
  assert.equal(sessionReads-readsBefore,2,"Identity must be freshly verified after hydration");
  assert.equal(await page.locator('#connector').isVisible(),true);
  assert.equal(await page.locator('[data-refresh-tools-target="true"]').count(),0,"A changed account must fail before a Refresh target is marked");
  assert.equal(await page.locator('body').getAttribute('data-clicks'),'0');
  console.log(JSON.stringify({ok:true,hydratedMs,cancelledMs,refreshClicks:0,checks:["settings hydration beyond five seconds","Bones headings remain visible","abort settles promptly without later control input","fresh account verification after hydration","changed account prevents Refresh targeting"]}));
} finally {await browser.close();}
