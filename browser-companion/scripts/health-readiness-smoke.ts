import assert from 'node:assert/strict';
import {chromium,type Page} from 'patchright';
import {ChatGptBrowserDriver,type SingleBrowserSurface} from '../src/chatgpt-driver';
import {protocolBuild} from '../src/protocol';

const browser=await chromium.launch({executablePath:process.env.CODEXIFY_TEST_CHROME_PATH,headless:true});
try {
  const context=await browser.newContext();const page=await context.newPage();
  let signedOut=false;
  await context.route('https://chatgpt.com/**',route=>new URL(route.request().url()).pathname==='/api/auth/session'
    ?route.fulfill({json:signedOut?{}:{user:{id:'fixture-user'},account:{id:'fixture-account',planType:'pro',structure:'personal'}}})
    :route.fulfill({contentType:'text/html',body:'<!doctype html><body><form><div id="prompt-textarea" role="textbox" contenteditable="false" style="min-height:24px">Preserved draft</div></form></body>'}));
  const surface:SingleBrowserSurface={page:async()=>page,navigate:async url=>{await page.goto(url);return page;},beginAction:async()=>{},endAction:async()=>{},reveal:async()=>{}};
  await page.goto('https://chatgpt.com/c/health-hydration');
  const driver=new ChatGptBrowserDriver(surface,protocolBuild('test-build'));
  const started=performance.now();const health=await driver.health();const elapsedMs=performance.now()-started;
  assert.equal(health.authenticated,true);assert.equal(health.status,'idle','An authenticated hydrating editor must not make the browser unavailable');
  assert(elapsedMs<1000,'Passive health must not wait for editor readiness');
  assert.equal(await page.locator('#prompt-textarea').innerText(),'Preserved draft');
  await page.evaluate(()=>{const first=document.querySelector('#prompt-textarea')!;first.setAttribute('contenteditable','true');first.parentElement!.append(first.cloneNode(true));});
  assert.equal((await driver.health()).status,'idle','Duplicate transient editors are an action-readiness concern');
  signedOut=true;await page.goto('https://chatgpt.com/auth/login');
  assert.equal((await driver.health()).status,'authentication_required','Real sign-out still requires authentication');
  await page.close();
  assert.equal((await driver.health()).status,'unavailable','A real closed page stays unavailable');
  console.log(JSON.stringify({ok:true,healthMs:elapsedMs,checks:['disabled hydration','duplicate hydration','draft preserved','real sign-out','closed page']}));
} finally {await browser.close();}
