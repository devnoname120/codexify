process.env.CODEXIFY_BROWSER_RECOVERY_ENHANCEMENTS="1";
import {execFile} from "node:child_process";
import {promisify} from "node:util";
import assert from 'node:assert/strict';
import {mkdtemp,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve} from 'node:path';
import {chromium} from 'patchright';
import {privateChromeExecutable} from '../src/chrome-executable';
import {ChatGptBrowserDriver,stopLingeringResponse,type SingleBrowserSurface} from '../src/chatgpt-driver';
import {SingleWindowCoordinator} from '../src/browser-window';
import {fingerprintAccountIdentity} from '../src/account';
import {protocolBuild,renderActionPrompt,type ExecuteActionRequest} from '../src/protocol';
import {activeComposer,composerText} from '../src/composer';

const profile=await mkdtemp(join(tmpdir(),'codexify-stop-recovery-'));
const executablePath=process.env.CODEXIFY_TEST_CHROME_PATH??await privateChromeExecutable(resolve('.browsers',`${process.platform}-${process.arch}`));
const context=await chromium.launchPersistentContext(profile,{executablePath,headless:true,timeout:20_000});
const page=context.pages()[0]!;
let mode='late-control',sends=0,steers=0,loads=0;
const url='https://chatgpt.com/c/stop-recovery-fixture';
const account=fingerprintAccountIdentity('stop-user','stop-account');
try {
 await page.exposeFunction('recordSend',()=>{sends++;});
 await context.route('https://chatgpt.com/**',async route=>{
  const path=new URL(route.request().url()).pathname;
  if(path==='/api/auth/session'){await route.fulfill({json:{user:{id:'stop-user'},account:{id:'stop-account',planType:'pro',structure:'personal'}}});return;}
  if(path.endsWith('/steer_turn')){steers++;await route.fulfill({json:{steered:true}});return;}
  if(path.endsWith('/conversation')){await route.fulfill({json:{accepted:true}});return;}
  loads++;
  await route.fulfill({contentType:'text/html',body:`<!doctype html><body data-running="true"><section id="history"></section>
   <form data-chatgpt-composer><div id="prompt-textarea" contenteditable="true" style="white-space:pre-wrap;min-height:28px"><p><br class="ProseMirror-trailingBreak"></p></div><button type="submit" data-testid="send-button">Send</button></form>
   <script>
    const mode=${JSON.stringify(mode)},editor=document.querySelector('#prompt-textarea');
    editor.onfocus=()=>{const range=document.createRange();range.selectNodeContents(editor.lastElementChild||editor);range.collapse(false);const selection=getSelection();selection.removeAllRanges();selection.addRange(range);};
    let lateStopAdded=false;
    editor.oninput=()=>{editor.querySelectorAll('br:last-child').forEach(br=>br.classList.add('ProseMirror-trailingBreak'));if(mode==='late-after-insertion'&&!lateStopAdded&&editor.innerText.trim()){lateStopAdded=true;document.body.append(stop);}};
    const stop=document.createElement('button');stop.type='button';stop.textContent='End turn';
    stop.onclick=()=>{
     document.body.dataset.stopRequests=String(Number(document.body.dataset.stopRequests||0)+1);
     if(mode==='ignore-first'&&document.body.dataset.stopRequests==='1')return;
     if(mode==='never-stops')return;
     if(mode==='slow-stop'){setTimeout(()=>{stop.remove();document.body.dataset.running='false';},15000);return;}
     stop.remove();document.body.dataset.running='false';
    };
    if(mode==='disabled-control'){stop.disabled=true;document.body.append(stop);setTimeout(()=>stop.disabled=false,6500);}
    else if(mode==='late-control')setTimeout(()=>document.body.append(stop),1100);
    else if(mode==='late-after-insertion'){}
    else document.body.append(stop);
    document.querySelector('form').onsubmit=event=>{
     event.preventDefault();window.recordSend();
     const turn=document.createElement('div');turn.dataset.turnKey='accepted-'+document.querySelectorAll('[data-turn-key]').length;
     const bubble=document.createElement('div');bubble.setAttribute('data-user-message-bubble','');bubble.textContent=editor.innerText;turn.append(bubble);document.querySelector('#history').append(turn);editor.textContent='';
     fetch(document.body.dataset.running==='true'?'/backend-api/f/steer_turn':'/backend-api/f/conversation',{method:'POST',body:'fixture'}).catch(()=>{});
    };
   </script>`});
 });
 const surface:SingleBrowserSurface={requireSubmissionNetworkProof:true,page:async()=>page,navigate:async destination=>{await page.goto(destination);return page;},reveal:async()=>{},beginAction:async()=>{},endAction:async()=>{},close:async()=>{}};
 const build=protocolBuild('stop-recovery-smoke');
 const request:ExecuteActionRequest={actionId:'late-stop',idempotencyKey:'late-stop',url,expectedUrl:url,pairingGeneration:1,
  expectedAccountFingerprint:account,connectorName:'Fixture connector',model:null,effort:null,autoApproveToolCalls:false,prompt:{kind:'offline_goal',goalRevision:1}};
 mode='disabled-control';await page.goto(url);
 await assert.rejects(new ChatGptBrowserDriver(surface,build,undefined,false).execute({request,renderedPrompt:renderActionPrompt(request.prompt),signal:AbortSignal.timeout(10000),emit:()=>{}}),{code:'editor_unavailable'});
 assert.equal(sends,0,'disabled recovery cannot send into the old response');
 assert.equal(await page.locator('body').getAttribute('data-stop-requests'),null,'disabled recovery cannot click Stop');
 mode='late-control';await page.goto(url);
 const coordinator=new SingleWindowCoordinator(new ChatGptBrowserDriver(surface,build),build);
 await coordinator.execute(request);
 const events=[];for await(const event of coordinator.events(request.actionId,0))events.push(event);
 assert.equal(events.at(-1)?.type,'submission_complete',JSON.stringify(events));
 assert.equal(steers,0,'A late End turn control must be stopped before the next prompt is sent');
 assert.equal(sends,1);assert.equal(await page.locator('body').getAttribute('data-stop-requests'),'1');
 await coordinator.close();
 mode='late-after-insertion';await page.goto(url);const beforeLateSends=sends,beforeLateLoads=loads;
 const lateCoordinator=new SingleWindowCoordinator(new ChatGptBrowserDriver(surface,build),build);
 await lateCoordinator.execute({...request,actionId:'late-after-insert',idempotencyKey:'late-after-insert'});
 const lateEvents=[];for await(const event of lateCoordinator.events('late-after-insert',0))lateEvents.push(event);
 assert.equal(lateEvents.at(-1)?.type,'submission_complete',JSON.stringify(lateEvents));assert.equal(sends,beforeLateSends+1);assert.equal(steers,0);assert.equal(loads,beforeLateLoads);assert.equal(await page.locator('body').getAttribute('data-stop-requests'),'1');
 await lateCoordinator.close();
 for(const nextMode of ['disabled-control','ignore-first','slow-stop']){
  mode=nextMode;await page.goto(url);
  const started=performance.now(),beforeLoads=loads;
  await stopLingeringResponse(page,new AbortController().signal,{canonicalUrl:url,accountFingerprint:account},()=>{});
  assert.equal(await page.locator('body').getAttribute('data-running'),'false');
  assert.equal(await page.locator('body').getAttribute('data-stop-requests'),nextMode==='ignore-first'?'2':'1');
  assert.equal(loads,beforeLoads,'A recoverable Stop should not reload the conversation');
  assert(performance.now()-started<(nextMode==='ignore-first'?30_000:nextMode==='slow-stop'?23_000:15_000),'Stop recovery must remain bounded');
 }
 mode='never-stops';await page.goto(url);await (await activeComposer(page)).fill('My unsent draft');
 const beforeLoads=loads;
 await assert.rejects(stopLingeringResponse(page,new AbortController().signal,{canonicalUrl:url,accountFingerprint:account},()=>{}),{code:'external_interference'});
 assert.equal(await composerText(await activeComposer(page)),'My unsent draft');
 assert.equal(loads,beforeLoads);assert.equal(await page.locator('body').getAttribute('data-stop-requests'),null);
 mode='never-stops';await page.goto(url);
 const stuckLoads=loads;
 await assert.rejects(stopLingeringResponse(page,new AbortController().signal,{canonicalUrl:url,accountFingerprint:account},()=>{},
  {mayReload:false,budgetMs:1_500}),{code:'selector_drift'});
 assert.equal(loads,stuckLoads);assert.equal(page.isClosed(),false,'An unresponsive Stop must not quit the browser');
 mode='disabled-control';await page.goto(url);
 const midCancel=new AbortController(),cancelStarted=performance.now();setTimeout(()=>midCancel.abort(),200);
 await assert.rejects(stopLingeringResponse(page,midCancel.signal,{canonicalUrl:url,accountFingerprint:account},()=>{}));
 assert(performance.now()-cancelStarted<2_000,'Cancellation must abort a pending Stop click promptly');
 assert.equal(await page.locator('body').getAttribute('data-stop-requests'),null);
 const cancelled=new AbortController();cancelled.abort();
 await assert.rejects(stopLingeringResponse(page,cancelled.signal,{canonicalUrl:url,accountFingerprint:account},()=>{}),{name:'AbortError'});
 assert.equal(sends,2,'Stopping and cancellation must not send any additional prompts');
 console.log(JSON.stringify({ok:true,lateStopBeforeSend:true,disabledStopWaited:true,ignoredStopRetried:true,draftPreserved:true,cancellationRespected:true,unresponsiveStopKeepsBrowser:true}));
}finally{
 await context.unrouteAll({behavior:'ignoreErrors'});
 let shutdownTimer:ReturnType<typeof setTimeout>|undefined;
 const closed=await Promise.race([context.close().then(()=>true),new Promise<false>(resolve=>{shutdownTimer=setTimeout(()=>resolve(false),35_000);})]);
 if(shutdownTimer)clearTimeout(shutdownTimer);
 if(!closed){
  // Patchright can retain a broken Windows pipe after Chrome has exited. This
  // is fixture cleanup: require the owned browser process to be gone first.
  assert.equal(process.platform,'win32','Chrome cleanup did not complete');
  const script=String.raw`$children=@(Get-CimInstance Win32_Process -Filter ('ParentProcessId='+$env:CODEXIFY_SMOKE_PARENT) | Where-Object {$_.Name -eq 'chrome.exe'});@{alive=$children.Count}|ConvertTo-Json -Compress`;
  const {stdout}=await promisify(execFile)('powershell.exe',['-NoProfile','-NonInteractive','-OutputFormat','Text','-EncodedCommand',Buffer.from(script,'utf16le').toString('base64')],{windowsHide:true,timeout:10000,env:{...process.env,CODEXIFY_SMOKE_PARENT:String(process.pid)}});
  assert.equal(JSON.parse(stdout.trim()).alive,0,'The fixture still owns a live Chrome process');
 }
 await rm(profile,{recursive:true,force:true,maxRetries:5,retryDelay:250});
}
// All assertions and owned-browser cleanup completed; close any stale Windows
// automation pipes so the test runner does not mistake them for browser work.
process.exit(0);
