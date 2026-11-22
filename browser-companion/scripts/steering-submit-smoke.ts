import assert from 'node:assert/strict';
import {mkdtemp,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve} from 'node:path';
import {chromium} from 'patchright';
import {privateChromeExecutable} from '../src/chrome-executable';
import {ChatGptBrowserDriver,type SingleBrowserSurface} from '../src/chatgpt-driver';
import {fingerprintAccountIdentity} from '../src/account';
import {protocolBuild,renderActionPrompt,type ExecuteActionRequest} from '../src/protocol';
import type {DriverEvent} from '../src/browser-window';

const profile=await mkdtemp(join(tmpdir(),'steering-submit-'));
const executablePath=await privateChromeExecutable(resolve('.browsers',`${process.platform}-${process.arch}`));
const context=await chromium.launchPersistentContext(profile,{executablePath,headless:true});
const page=context.pages()[0]!,url='https://chatgpt.com/c/steering-submit-fixture';
let mode:'accepted'|'phantom'|'error'='accepted',sends=0;
try {
 await page.exposeFunction('recordSend',()=>sends++);
 await context.route('https://chatgpt.com/**',route=>{
  const path=new URL(route.request().url()).pathname;
  if(path==='/api/auth/session')return route.fulfill({json:{user:{id:'fixture-user'},account:{id:'fixture-account',structure:'personal',planType:'pro'}}});
  if(path==='/backend-api/f/steer_turn')return route.fulfill({status:mode==='error'?500:200,json:mode==='error'?{error:'Internal Server Error'}:{accepted:true}});
  return route.fulfill({contentType:'text/html',body:`<!doctype html><body>
   <section id="history"><div data-turn-key="old"><div data-user-message-bubble>Old instruction</div><div data-conversation-role="assistant">Previous response</div></div></section>
   <form data-chatgpt-composer><div id="prompt-textarea" contenteditable="true" style="white-space:pre-wrap;min-height:28px"></div><button type="submit" data-testid="send-button">Send</button></form>
   <script>
   const editor=document.querySelector('#prompt-textarea');
   editor.onfocus=()=>{const r=document.createRange();r.selectNodeContents(editor);r.collapse(false);const s=getSelection();s.removeAllRanges();s.addRange(r);};
   document.querySelector('form').onsubmit=e=>{
    e.preventDefault();window.recordSend();
    if(${JSON.stringify(mode)}!=='phantom'){
     const turn=document.createElement('div');turn.dataset.turnKey='new-instruction';const bubble=document.createElement('div');bubble.setAttribute('data-user-message-bubble','');bubble.textContent=editor.innerText;turn.append(bubble);document.querySelector('#history').append(turn);
    }
    editor.textContent='';const stop=document.createElement('button');stop.dataset.testid='stop-button';stop.textContent='Stop';document.body.append(stop);
    fetch('/backend-api/f/steer_turn',{method:'POST',body:'fixture'}).catch(()=>{});
   };
   </script>`});
 });
 const surface:SingleBrowserSurface={requireSubmissionNetworkProof:true,page:async()=>page,navigate:async u=>{await page.goto(u);return page;},reveal:async()=>{},beginAction:async()=>{},endAction:async()=>{},close:async()=>{}};
 for(const scenario of ['accepted','phantom','error'] as const){
  mode=scenario;await page.goto(url);
  const actionId='owner-message-'+scenario;
  const request:ExecuteActionRequest={actionId,idempotencyKey:actionId,url,expectedUrl:url,pairingGeneration:1,expectedAccountFingerprint:fingerprintAccountIdentity('fixture-user','fixture-account'),connectorName:'Fixture connector',model:null,effort:null,autoApproveToolCalls:false,prompt:{kind:'offline_chat',batchId:actionId,messages:[{id:actionId,markdown:'Investigate the next task'}]}};
  const events:DriverEvent[]=[],before=sends;
  const sent=new ChatGptBrowserDriver(surface,protocolBuild('steering-submit')).execute({request,renderedPrompt:renderActionPrompt(request.prompt),signal:AbortSignal.timeout(60_000),emit:event=>{events.push(event);}});
  if(scenario==='accepted'){
   await sent;assert.equal(events.filter(e=>e.type==='send_accepted').length,1);
   assert(events.some(e=>e.type==='submission_complete'));
  } else {
   await assert.rejects(sent,(error:unknown)=>{
    assert.equal((error as {code:string}).code,'ambiguous_send');
    if(scenario==='error')assert.match((error as Error).message,/steering submission returned HTTP 500/);
    return true;
   });
   assert(!events.some(e=>e.type==='send_accepted'));
  }
  assert.equal(sends,before+1,'a steering result must never replay the message');
 }
 console.log(JSON.stringify({ok:true,successfulSteeringAccepted:true,oldGenerationAloneRejected:true,serverFailurePreserved:true,sends}));
} finally {await context.unrouteAll({behavior:'ignoreErrors'});await context.close();await rm(profile,{recursive:true,force:true,maxRetries:5,retryDelay:250});}
