process.env.CODEXIFY_BROWSER_RECOVERY_ENHANCEMENTS="1";
import assert from 'node:assert/strict';
import {mkdtemp,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve} from 'node:path';
import {chromium} from 'patchright';
import {privateChromeExecutable} from '../src/chrome-executable';
import {ChatGptBrowserDriver,type SingleBrowserSurface} from '../src/chatgpt-driver';
import {fingerprintAccountIdentity} from '../src/account';
import {protocolBuild,renderActionPrompt,type ExecuteActionRequest} from '../src/protocol';
import {activeComposer,composerText} from '../src/composer';
import {DraftRecoveryJournal} from '../src/draft-recovery';

const profile=await mkdtemp(join(tmpdir(),'codexify-draft-recovery-'));
const executablePath=await privateChromeExecutable(resolve('.browsers',`${process.platform}-${process.arch}`));
const context=await chromium.launchPersistentContext(profile,{executablePath,headless:true});
const page=context.pages()[0]!,url='https://chatgpt.com/c/draft-recovery-fixture';
let draft='',sends=0,stops=0;
const prompt=renderActionPrompt({kind:'offline_goal',goalRevision:1});
try {
 await page.exposeFunction('recordSend',()=>sends++);await page.exposeFunction('recordStop',()=>stops++);
 await context.route('https://chatgpt.com/**',route=>{
  const path=new URL(route.request().url()).pathname;
  if(path==='/api/auth/session')return route.fulfill({json:{user:{id:'draft-user'},account:{id:'draft-account',structure:'personal',planType:'pro'}}});
  if(path.endsWith('/conversation'))return route.fulfill({json:{accepted:true}});
  return route.fulfill({contentType:'text/html',body:`<!doctype html><body><section id="history"></section>
  <form data-chatgpt-composer><div id="prompt-textarea" contenteditable="true" style="white-space:pre-wrap;min-height:28px"><p><br class="ProseMirror-trailingBreak"></p></div><button type="submit" data-testid="send-button">Send</button></form>
  <button id="end" type="button">End turn</button><script>
  const editor=document.querySelector('#prompt-textarea');editor.textContent=${JSON.stringify(draft)};
  editor.onfocus=()=>{const r=document.createRange();r.selectNodeContents(editor);r.collapse(false);const s=getSelection();s.removeAllRanges();s.addRange(r);};
  document.querySelector('#end').onclick=()=>{window.recordStop();document.querySelector('#end').remove();};
  document.querySelector('form').onsubmit=e=>{e.preventDefault();window.recordSend();const t=document.createElement('div');t.dataset.turnKey='accepted';const b=document.createElement('div');b.setAttribute('data-user-message-bubble','');b.textContent=editor.innerText;t.append(b);document.querySelector('#history').append(t);editor.textContent='';fetch('/backend-api/f/conversation',{method:'POST',body:'fixture'}).catch(()=>{});};
  </script>`});
 });
 await page.goto(url);
 await (await activeComposer(page)).evaluate(e=>e.innerHTML='<p><span hidden>stale hidden UI text</span><span aria-hidden="true">editor placeholder</span><span style="display:none">cached text</span>\u200b<br class="ProseMirror-trailingBreak"></p>');
 assert.equal(await composerText(await activeComposer(page)),'','hidden editor markup is not a draft');
 const request:ExecuteActionRequest={actionId:'draft-recovery',idempotencyKey:'draft-recovery',url,expectedUrl:url,pairingGeneration:1,expectedAccountFingerprint:fingerprintAccountIdentity('draft-user','draft-account'),connectorName:'Fixture connector',model:null,effort:null,autoApproveToolCalls:false,prompt:{kind:'offline_goal',goalRevision:1}};
 const surface:SingleBrowserSurface={requireSubmissionNetworkProof:true,page:async()=>page,navigate:async u=>{await page.goto(u);return page;},reveal:async()=>{},beginAction:async()=>{},endAction:async()=>{},close:async()=>{}};
 for(const text of [prompt,prompt.replace(/\n/g,'\n\n'),'@Fixture connector']){
  draft=text;await page.goto(url);const before=sends;
  const stopped=stops;
  await assert.rejects(new ChatGptBrowserDriver(surface,protocolBuild('draft-recovery'),undefined,false).execute({request,renderedPrompt:prompt,signal:AbortSignal.timeout(10000),emit:()=>{}}),{code:'external_interference'});
  assert.equal(await composerText(await activeComposer(page)),text,'disabled recovery preserves stale drafts');
  assert.equal(sends,before);assert.equal(stops,stopped);
  await new ChatGptBrowserDriver(surface,protocolBuild('draft-recovery')).execute({request,renderedPrompt:prompt,signal:AbortSignal.timeout(25000),emit:()=>{}});
  assert.equal(sends,before+1);assert.equal(await composerText(await activeComposer(page)),'');
 }
 for(const text of ['My actual draft',prompt+' edited by me']){
  draft=text;await page.goto(url);const before=sends,stopped=stops;
  await assert.rejects(new ChatGptBrowserDriver(surface,protocolBuild('draft-recovery')).execute({request,renderedPrompt:prompt,signal:AbortSignal.timeout(10000),emit:()=>{}}),{code:'external_interference'});
  assert.equal(await composerText(await activeComposer(page)),text);assert.equal(sends,before);assert.equal(stops,stopped);
 }
 const cachePath=join(profile,'connector-cache.json'),journalPath=cachePath+'.drafts.json',account=request.expectedAccountFingerprint;
 const combined:ExecuteActionRequest={...request,actionId:'new-retry',prompt:{kind:'offline_combined',goalRevision:1,batchId:'batch',messages:[{id:'one',markdown:'Work on the next verified task'}]}};
 const combinedPrompt=renderActionPrompt(combined.prompt),journal=new DraftRecoveryJournal(journalPath);
 journal.prepare(account,url,'interrupted-before-send',combinedPrompt);draft=combinedPrompt;await page.goto(url);const before=sends;
 await new ChatGptBrowserDriver(surface,protocolBuild('draft-recovery'),cachePath).execute({request:combined,renderedPrompt:combinedPrompt,signal:AbortSignal.timeout(25000),emit:()=>{}});
 assert.equal(sends,before+1,'a new driver recovers the recorded unsent draft once');
 journal.prepare(account,url,'uncertain-send',combinedPrompt);journal.beforeSend('uncertain-send');draft=combinedPrompt;await page.goto(url);
 await assert.rejects(new ChatGptBrowserDriver(surface,protocolBuild('draft-recovery'),cachePath).execute({request:combined,renderedPrompt:combinedPrompt,signal:AbortSignal.timeout(10000),emit:()=>{}}),{code:'external_interference'});
 assert.equal(sends,before+1,'an uncertain send with user messages must not be replayed');
 assert.equal(await composerText(await activeComposer(page)),combinedPrompt);
 console.log(JSON.stringify({ok:true,invisibleMarkupIgnored:true,staleAutomationDraftRecovered:true,editedDraftPreserved:true,sends}));
} finally {await context.unrouteAll({behavior:'ignoreErrors'});await context.close();await rm(profile,{recursive:true,force:true,maxRetries:5,retryDelay:250});}
