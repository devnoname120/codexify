import assert from 'node:assert/strict';
import {mkdtemp,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve} from 'node:path';
import {chromium,type Route} from 'patchright';
import {privateChromeExecutable} from '../src/chrome-executable';
import {ChatGptBrowserDriver,type SingleBrowserSurface} from '../src/chatgpt-driver';
import type {DriverEvent} from '../src/browser-window';
import {fingerprintAccountIdentity} from '../src/account';
import {protocolBuild,renderActionPrompt,type ExecuteActionRequest} from '../src/protocol';

const profile=await mkdtemp(join(tmpdir(),'navigation-readiness-'));
const executablePath=await privateChromeExecutable(resolve('.browsers',`${process.platform}-${process.arch}`));
const context=await chromium.launchPersistentContext(profile,{executablePath,headless:true});
const page=context.pages()[0]!,url='https://chatgpt.com/c/navigation-ready-fixture';
let mode:'ready'|'wrong-url'|'wrong-account'|'no-composer'|'cancelled'|'transport-error'|'user-draft'='ready',sends=0,timeouts=0;
const heldRoutes:Route[]=[];
let controller=new AbortController();
try {
 await page.exposeFunction('recordSend',()=>sends++);
 await context.route('https://chatgpt.com/**',async route=>{
  const path=new URL(route.request().url()).pathname;
  if(path==='/api/auth/session')return route.fulfill({json:{user:{id:mode==='wrong-account'?'other-user':'fixture-user'},account:{id:'fixture-account',structure:'personal',planType:'pro'}}});
  if(path==='/pending-navigation.js'){heldRoutes.push(route);return;}
  if(path==='/backend-api/f/conversation')return route.fulfill({json:{accepted:true}});
  const thread=path.startsWith('/c/');
  return route.fulfill({contentType:'text/html',body:`<!doctype html><body>
   <section id="history"><div data-turn-key="old"><div data-user-message-bubble>Old instruction</div><div data-conversation-role="assistant">Previous response</div></div></section>
   ${mode==='no-composer'&&thread?'':`<form data-chatgpt-composer><div id="prompt-textarea" contenteditable="true" style="white-space:pre-wrap;min-height:28px"></div><button type="submit" data-testid="send-button">Send</button></form>`}
   <script>
   const editor=document.querySelector('#prompt-textarea');
   if(editor){
    if(${JSON.stringify(mode)}==='user-draft')editor.textContent='My unfinished draft';
    editor.onfocus=()=>{const r=document.createRange();r.selectNodeContents(editor);r.collapse(false);const s=getSelection();s.removeAllRanges();s.addRange(r);};
    document.querySelector('form').onsubmit=e=>{e.preventDefault();window.recordSend();const turn=document.createElement('div');turn.dataset.turnKey='new-instruction';const bubble=document.createElement('div');bubble.setAttribute('data-user-message-bubble','');bubble.textContent=editor.innerText;turn.append(bubble);document.querySelector('#history').append(turn);editor.textContent='';fetch('/backend-api/f/conversation',{method:'POST',body:'fixture'}).catch(()=>{});};
   }
   </script>${thread?'<script defer src="/pending-navigation.js"></script>':''}`});
 });
 const surface:SingleBrowserSurface={requireSubmissionNetworkProof:true,page:async()=>page,navigate:async destination=>{
  if(mode==='transport-error'){await page.goto(destination,{waitUntil:'commit'});throw new Error('fixture transport failure');}
  try {await page.goto(mode==='wrong-url'?'https://chatgpt.com/c/another-fixture':destination,{timeout:150,waitUntil:'domcontentloaded'});}
  catch(error){
   assert.equal((error as Error).name,'TimeoutError');timeouts++;
   if(mode==='cancelled')controller.abort(new Error('fixture cancellation'));
   throw error;
  }
  throw Error('fixture must hold DOMContentLoaded until its navigation deadline');
 },reveal:async()=>{},beginAction:async()=>{},endAction:async()=>{},close:async()=>{}};
 for(const scenario of ['ready','wrong-url','wrong-account','no-composer','cancelled','transport-error','user-draft'] as const){
  mode=scenario;controller=new AbortController();await page.goto('https://chatgpt.com/');
  const actionId='navigation-'+scenario;
  const request:ExecuteActionRequest={actionId,idempotencyKey:actionId,url,expectedUrl:url,pairingGeneration:1,expectedAccountFingerprint:fingerprintAccountIdentity('fixture-user','fixture-account'),connectorName:'Fixture connector',model:null,effort:null,autoApproveToolCalls:false,prompt:{kind:'offline_chat',batchId:actionId,messages:[{id:actionId,markdown:'Investigate the next task'}]}};
  const before=sends,events:DriverEvent[]=[];
  const pending=new ChatGptBrowserDriver(surface,protocolBuild('navigation-readiness')).execute({request,renderedPrompt:renderActionPrompt(request.prompt),signal:AbortSignal.any([controller.signal,AbortSignal.timeout(60_000)]),emit:event=>{events.push(event);}});
  if(scenario==='ready'){
   await pending;assert.equal(sends,before+1);assert.equal(events.filter(e=>e.type==='send_accepted').length,1);assert(events.some(e=>e.type==='submission_complete'));
  } else {
   await assert.rejects(pending);assert.equal(sends,before);assert(!events.some(e=>e.type==='send_accepted'));
   if(scenario!=='wrong-account'&&scenario!=='user-draft')assert(!events.some(e=>e.type==='navigation_progress'&&e.step==='account_verified'));
   if(scenario==='user-draft')assert.equal(await page.locator('#prompt-textarea').innerText(),'My unfinished draft');
  }
 }
 assert.equal(timeouts,6);
 console.log(JSON.stringify({ok:true,renderedPageSurvivesNavigationTimeout:true,wrongThreadRejected:true,wrongAccountRejected:true,missingComposerRejected:true,cancellationPreserved:true,transportFailurePreserved:true,userDraftPreserved:true,sends,timeouts}));
} finally {
 for(const route of heldRoutes)await route.abort().catch(()=>{});
 await context.unrouteAll({behavior:'ignoreErrors'});await context.close();await rm(profile,{recursive:true,force:true,maxRetries:5,retryDelay:250});
}
