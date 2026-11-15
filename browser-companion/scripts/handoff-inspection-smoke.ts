import assert from 'node:assert/strict';
import {mkdtemp,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve} from 'node:path';
import {chromium} from 'patchright';
import {privateChromeExecutable} from '../src/chrome-executable';
import {ChatGptBrowserDriver,type SingleBrowserSurface} from '../src/chatgpt-driver';
import {fingerprintAccountIdentity} from '../src/account';
import {protocolBuild} from '../src/protocol';
import {createCompletedHandoffVerifier} from '../src/turn-renewal';
const profile=await mkdtemp(join(tmpdir(),'codexify-handoff-inspection-'));
const context=await chromium.launchPersistentContext(profile,{executablePath:process.env.CODEXIFY_TEST_CHROME_PATH??await privateChromeExecutable(resolve('.browsers',`${process.platform}-${process.arch}`)),headless:true});
const page=context.pages()[0]!,url='https://chatgpt.com/c/handoff-inspection',account=fingerprintAccountIdentity('probe-user','probe-account'),id=`renew-${'a'.repeat(64)}`;
let accountId='probe-account',loads=0,accountReads=0;
try {
 await context.route('https://chatgpt.com/**',route=>{
  if(new URL(route.request().url()).pathname==='/api/auth/session'){accountReads++;return route.fulfill({json:{user:{id:'probe-user'},account:{id:accountId,structure:'personal',planType:'pro'}}});}
  return route.fulfill({contentType:'text/html',body:'<!doctype html><form data-chatgpt-composer><div id="prompt-textarea" contenteditable="true"></div></form><button data-testid="stop-button">Stop</button>'});
 });
 const surface:SingleBrowserSurface={page:async()=>page,navigate:async u=>{loads++;await page.goto(u);return page;},reveal:async()=>{},beginAction:async()=>{},endAction:async()=>{},close:async()=>{}};
 const driver=new ChatGptBrowserDriver(surface,protocolBuild('handoff-inspection'));
 await page.goto(url);assert.equal(await driver.turnHandoffComplete(url,account,id),false,'a real incomplete response is a negative probe');
 accountId='different-account';await assert.rejects(driver.turnHandoffComplete(url,account,id),{code:'authentication_required'});accountId='probe-account';
 await page.goto('https://chatgpt.com/c/other');assert.equal(await driver.turnHandoffComplete(url,account,id),false);assert.equal(loads,1);
 await page.goto('https://chatgpt.com/c/other');await assert.rejects(driver.turnHandoffComplete(url,account,id),{code:'editor_unavailable'});assert.equal(loads,1,'a skipped visit cannot masquerade as a real negative probe');
 const recheckStartedAt=performance.now();
 await new Promise(resolve=>setTimeout(resolve,1_010));
 assert.equal(await driver.turnHandoffComplete(url,account,id),false);
 assert.equal(loads,2,'a healthy pending handoff can be revisited after one second');
 const offTabRecheckMs=performance.now()-recheckStartedAt;
 await page.goto(url);
 await page.evaluate(id=>{
  document.querySelector('[data-testid="stop-button"]')!.remove();
  const turn=document.createElement('article');turn.dataset.turnId='final-handoff';turn.dataset.turnIdContainer='final-handoff';turn.dataset.testid='conversation-turn-1';turn.dataset.turn='assistant';
  turn.innerHTML='<div class="markdown"><p></p><p>Verified implementation and next steps.</p></div><button data-testid="copy-turn-action-button">Copy</button>';
  turn.querySelector('p')!.textContent=`Turn handoff ${id}`;document.body.append(turn);
 },id);
 const verify=createCompletedHandoffVerifier(page,url,account,id);
 const before=accountReads,started=performance.now();assert.equal(await verify(),true);
 const initialProofMs=performance.now()-started;
 assert.equal(accountReads-before,2,'Initial proof requires two fresh account checks and stable snapshots');
 const currentBefore=accountReads,recheckStarted=performance.now();assert.equal(await verify(),true);
 const unchangedRecheckMs=performance.now()-recheckStarted;
 assert.equal(accountReads-currentBefore,1,'An unchanged proof still requires fresh current account identity');
 await page.locator('.markdown p').last().evaluate(el=>{el.textContent='Changed after verification';});
 assert.equal(await verify(),false,'Changed handoff text must invalidate the action-local proof');
 await page.locator('.markdown p').last().evaluate(el=>{el.textContent='Verified implementation and next steps.';});
 assert.equal(await verify(),true);
 await page.evaluate(()=>{const stop=document.createElement('button');stop.dataset.testid='stop-button';stop.textContent='Stop';document.body.append(stop);});
 assert.equal(await verify(),false,'A resumed turn cannot reuse a completed handoff');
 await page.locator('[data-testid="stop-button"]').evaluate(el=>el.remove());
 accountId='different-account';assert.equal(await verify(),false,'Cached handoff proof must never cache account authorization');accountId='probe-account';
 console.log(JSON.stringify({ok:true,realNegativeProbe:true,accountChangesBlocked:true,skippedProbeIsNotFailure:true,offTabRecheckMs,initialProofMs,unchangedRecheckMs,initialAccountReads:2,recheckAccountReads:1,changedReportBlocked:true,runningTurnBlocked:true}));
} finally {await context.unrouteAll({behavior:'ignoreErrors'});await context.close();await rm(profile,{recursive:true,force:true,maxRetries:5,retryDelay:250});}
