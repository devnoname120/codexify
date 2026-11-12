import assert from "node:assert/strict";
import {mkdtemp,rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import {join,resolve} from "node:path";
import {chromium} from "patchright";
import {privateChromeExecutable} from "../src/chrome-executable";
import {waitForConversationComposer} from "../src/conversation-readiness";
import {ChatGptBrowserDriver,type SingleBrowserSurface} from "../src/chatgpt-driver";
import {SingleWindowCoordinator} from "../src/browser-window";
import {fingerprintAccountIdentity} from "../src/account";
import {activeComposer,composerText} from "../src/composer";
import {selectConnector,ConnectorMenuUnavailableError} from "../src/connector-selection";
import {refreshConversationEditor} from "../src/editor-recovery";
import {navigatePage,withConversationCooldownBypass} from "../src/navigation";
import {protocolBuild,type ExecuteActionRequest} from "../src/protocol";
import {MINIMAL_CHATGPT_CSS} from "../src/minimal-page";
import {openNewChat,assertNewChatDraft} from "../src/new-chat";

// Patchright's Chrome transport uses Node on Windows, matching the packaged app.
const profile=await mkdtemp(join(tmpdir(),"codexify-hydration-"));
const executablePath=process.env.CODEXIFY_TEST_CHROME_PATH
  ?? await privateChromeExecutable(resolve(".browsers",`${process.platform}-${process.arch}`));
const context=await chromium.launchPersistentContext(profile,{executablePath,headless:true,timeout:20_000});
const page=context.pages()[0]!;
try {
  const url="https://chatgpt.com/c/hydration-fixture";
  let signedOut=false,wrongAccount=false,mode="delayed",sends=0,documentLoads=0,submissionStatus=200,steers=0;
  await page.exposeFunction("recordSend",()=>{sends++;});
  const composer=`<form data-chatgpt-composer><div id="prompt-textarea" contenteditable="true" style="white-space:pre-wrap;min-height:28px"><p><span contenteditable="false" data-id="plugin:fixture" data-keyword="Fixture connector">Fixture connector</span></p></div><button data-testid="send-button" type="submit">Send</button></form>`;
  await context.route("https://chatgpt.com/**",async route=>{
    if(new URL(route.request().url()).pathname==="/backend-api/f/steer_turn") {
      steers++;await route.fulfill({json:{steered:true}});return;
    }
    if(new URL(route.request().url()).pathname==="/backend-api/f/conversation") {
      await route.fulfill({status:submissionStatus,json:{accepted:submissionStatus===200}});return;
    }
    if(new URL(route.request().url()).pathname==="/api/auth/session"){
      await route.fulfill({json:signedOut?{}:{user:{id:"fixture-user"},account:{id:wrongAccount?"other-account":"fixture-account",planType:"pro",structure:"personal"}}});
      return;
    }
    if(route.request().isNavigationRequest())documentLoads++;
    if(mode==="hanging-navigation"&&route.request().isNavigationRequest()) {
      await new Promise(resolve=>setTimeout(resolve,18_000));
      await route.abort().catch(()=>{});return;
    }
    await route.fulfill({contentType:"text/html",body:`<!doctype html><body><section id="history"></section><div id="mount"></div><script>
      const mode=${JSON.stringify(mode)};
      if(mode==='login')document.querySelector('#mount').innerHTML='<a href="/auth/login">Sign in</a>';
      if(mode.startsWith('delayed'))setTimeout(()=>{
        document.querySelector('#mount').innerHTML=${JSON.stringify(composer)};
        if(mode==='delayed-end-turn'){
          document.body.dataset.running='true';
          const end=document.createElement('button');end.type='button';end.dataset.testid='end-turn-button';end.setAttribute('aria-label','End turn');end.textContent='End turn';
          end.onclick=()=>{document.body.dataset.endRequests=String(Number(document.body.dataset.endRequests||0)+1);end.remove();setTimeout(()=>document.body.append(end),80);setTimeout(()=>{end.remove();document.body.dataset.running='false';},1500);};
          document.body.append(end);
        }
        if(mode==='delayed-hidden-running')document.body.dataset.running='true';
        if(mode==='delayed-no-connector'||mode==='delayed-click-connector')document.querySelector('#prompt-textarea').innerHTML='<p><br class="ProseMirror-trailingBreak"></p>';
        document.querySelector('#prompt-textarea').onfocus=event=>{
          const range=document.createRange();range.selectNodeContents(event.currentTarget.lastElementChild||event.currentTarget);range.collapse(false);
          const selection=getSelection();selection.removeAllRanges();selection.addRange(range);
        };
        document.querySelector('#prompt-textarea').oninput=event=>{
          if(event.currentTarget.textContent.startsWith('@'))document.body.dataset.appQueries=String(Number(document.body.dataset.appQueries||0)+1);
          // Match ProseMirror's placeholder breaks after rich-text insertion.
          event.currentTarget.querySelectorAll('br:last-child').forEach(br=>br.classList.add('ProseMirror-trailingBreak'));
          if(mode==='delayed-click-connector'&&event.currentTarget.textContent.startsWith('@')&&!document.querySelector('[data-mention-list-scroll-area]')){
            const editor=event.currentTarget;
            const menu=document.createElement('div');menu.setAttribute('data-mention-list-scroll-area','');
            menu.innerHTML='<button type="button" data-list-navigation-item="true"><span>Fixture connector</span></button>';
            menu.querySelector('button').onclick=()=>{editor.innerHTML='<p><span contenteditable="false" data-id="plugin:fixture" data-keyword="Fixture connector">Fixture connector</span></p>';menu.remove();};setTimeout(()=>document.body.append(menu),6_000);
          }
        };
        document.querySelector('form').onsubmit=event=>{
          event.preventDefault();
          const editor=document.querySelector('#prompt-textarea'),copy=editor.cloneNode(true);
          copy.querySelectorAll('[contenteditable="false"]').forEach(node=>node.remove());
          const turn=document.createElement('div');turn.dataset.turnKey='accepted-turn-'+document.querySelectorAll('[data-turn-key]').length;
          const bubble=document.createElement('div');bubble.setAttribute('data-user-message-bubble','');bubble.textContent=[...copy.children].map(node=>node.textContent).join(String.fromCharCode(10));
          turn.appendChild(bubble);document.querySelector('#history').appendChild(turn);editor.textContent='';window.recordSend();
          const steering=document.body.dataset.running==='true';
          fetch(steering?'/backend-api/f/steer_turn':'/backend-api/f/conversation',{method:'POST',body:'fixture'}).catch(()=>{});
          if(mode==='delayed-hidden-running'&&steering){
            const stop=document.createElement('button');stop.type='button';stop.textContent='Stop';stop.disabled=true;
            stop.onclick=()=>{document.body.dataset.stopRequests=String(Number(document.body.dataset.stopRequests||0)+1);document.body.dataset.running='false';stop.remove();};
            document.querySelector('form').append(stop);setTimeout(()=>{stop.disabled=false;},6_000);
          }
        };
      },350);
      </script>`});
  });
  const surface:SingleBrowserSurface={requireSubmissionNetworkProof:true,page:async()=>page,navigate:async(destination,signal)=>{await withConversationCooldownBypass(true,()=>navigatePage(page,destination,{signal}));return page;},reveal:async()=>{},beginAction:async()=>{},endAction:async()=>{},close:async()=>{}};
  const build=protocolBuild("hydration-smoke");
  const request:ExecuteActionRequest={actionId:"delayed",idempotencyKey:"delayed",pairingGeneration:1,url,expectedUrl:url,
    expectedAccountFingerprint:fingerprintAccountIdentity("fixture-user","fixture-account"),connectorName:"Fixture connector",model:null,effort:null,
    autoApproveToolCalls:false,prompt:{kind:"offline_goal",goalRevision:1}};
  // Match the real surface: it opens ChatGPT before publishing its control
  // server. Do not manufacture authentication-required from about:blank.
  await page.goto(url,{waitUntil:'domcontentloaded'});
  const coordinator=new SingleWindowCoordinator(new ChatGptBrowserDriver(surface,build),build);
  await coordinator.execute(request);
  const events=[];for await(const event of coordinator.events(request.actionId,0))events.push(event);
  assert.equal(events.at(-1)?.type,"submission_complete",JSON.stringify(events));
  assert.equal(sends,1,"Hydrated conversation must receive exactly one continuation");
  await coordinator.close();

  mode="delayed-no-connector";await page.goto("https://chatgpt.com/",{waitUntil:"domcontentloaded"});
  await waitForConversationComposer(page);
  const started=performance.now();
  await assert.rejects(selectConnector(page,"Fixture connector"),ConnectorMenuUnavailableError);
  assert.equal((await composerText(await activeComposer(page))).trim(),"");
  const missingConnectorMs=Math.round(performance.now()-started);
  assert(missingConnectorMs<20_000,"Missing optional connector must finish before the shared stall deadline");
  assert.equal(sends,1);

  mode="delayed-click-connector";await page.goto(url,{waitUntil:"domcontentloaded"});await waitForConversationComposer(page);
  const connectorProgress:string[]=[];
  const clicked=await selectConnector(page,"Fixture connector",undefined,stage=>connectorProgress.push(stage));assert.equal(clicked.identity,"plugin:fixture");
  assert.deepEqual(connectorProgress,["query_inserted","menu_ready","app_selected"]);
  assert.equal(sends,1,"Selecting an app must never submit the composer");

  mode="delayed-no-connector";await page.goto(url,{waitUntil:"domcontentloaded"});await waitForConversationComposer(page);
  const paired=new SingleWindowCoordinator(new ChatGptBrowserDriver(surface,build),build);
  await paired.execute({...request,actionId:"paired-without-menu",idempotencyKey:"paired-without-menu"});
  const pairedEvents=[];for await(const event of paired.events("paired-without-menu",0))pairedEvents.push(event);
  assert.equal(pairedEvents.at(-1)?.type,"submission_complete",JSON.stringify({events:pairedEvents,editor:await (await activeComposer(page)).evaluate(element=>element.innerHTML)}));
  assert.equal(sends,2,"A paired conversation continues once without reopening its app menu");
  assert.equal(await page.locator('body').getAttribute('data-app-queries'),null,"Paired continuation must not type an app query");
  await paired.close();

  mode="delayed";await page.goto(url,{waitUntil:"domcontentloaded"});await waitForConversationComposer(page);
  const beforeRefresh=documentLoads,progress:string[]=[];
  await refreshConversationEditor(page,url,request.expectedAccountFingerprint,new AbortController().signal,step=>progress.push(step));
  assert.equal(documentLoads,beforeRefresh+1);
  assert.deepEqual(progress,["started","document_loaded","ready"]);
  await (await activeComposer(page)).fill("User draft");
  await assert.rejects(refreshConversationEditor(page,url,request.expectedAccountFingerprint,new AbortController().signal,()=>{}),{code:"external_interference"});
  assert.equal(documentLoads,beforeRefresh+1,"Recovery must preserve a user draft without reloading");
  assert.equal(await composerText(await activeComposer(page)),"User draft");

  await page.goto("https://chatgpt.com/",{waitUntil:"domcontentloaded"});
  const stuckModel=new SingleWindowCoordinator(new ChatGptBrowserDriver(surface,build),build);
  const beforeModel=documentLoads,modelStarted=performance.now();
  await stuckModel.execute({...request,actionId:"stuck-model",idempotencyKey:"stuck-model",model:"gpt-6-sol",effort:"high"});
  const modelEvents=[];for await(const event of stuckModel.events("stuck-model",0))modelEvents.push(event);
  assert.equal(documentLoads,beforeModel+2,"Missing model control gets exactly one recovery refresh");
  assert.equal(modelEvents.at(-1)?.type,"failed",JSON.stringify(modelEvents));
  assert.equal((modelEvents.at(-1) as any).code,"model_unavailable");
  assert.equal((await stuckModel.action("stuck-model"))!.sendAccepted,false);
  assert.equal((await stuckModel.health()).status,"idle");
  assert.equal(page.isClosed(),false,"A missing model control must not quit Chrome");
  const stuckModelMs=Math.round(performance.now()-modelStarted);
  assert(stuckModelMs<25_000);
  await stuckModel.execute({...request,actionId:"after-stuck-model",idempotencyKey:"after-stuck-model"});
  const resumed=[];for await(const event of stuckModel.events("after-stuck-model",0))resumed.push(event);
  assert.equal(resumed.at(-1)?.type,"submission_complete",JSON.stringify(resumed));
  assert.equal(sends,3,"The same browser must accept the next continuation");
  await stuckModel.close();

  mode="hanging-navigation";
  const hanging=new SingleWindowCoordinator(new ChatGptBrowserDriver(surface,build),build);
  const loadStarted=performance.now(),hangingUrl="https://chatgpt.com/c/hanging-load";
  await hanging.execute({...request,actionId:"hanging-load",idempotencyKey:"hanging-load",url:hangingUrl,expectedUrl:hangingUrl});
  const loadEvents=[];for await(const event of hanging.events("hanging-load",0))loadEvents.push(event);
  assert.equal(loadEvents.at(-1)?.type,"failed",JSON.stringify(loadEvents));
  assert.equal((await hanging.action("hanging-load"))!.sendAccepted,false);
  assert(performance.now()-loadStarted<25_000,"A stalled load must finish before forced browser retirement");
  assert.equal(page.isClosed(),false);
  mode="delayed";await page.goto(url,{waitUntil:"domcontentloaded"});await waitForConversationComposer(page);
  assert.equal((await hanging.health()).status,"idle");await hanging.close();

  // A leftover Temporary draft must never initialize a durable worker.
  await page.goto('https://chatgpt.com/?temporary-chat=true',{waitUntil:'domcontentloaded'});await waitForConversationComposer(page);
  await assert.rejects(assertNewChatDraft(page,null),{code:'conversation_unavailable'});
  await openNewChat(surface,null);
  assert.equal(page.url(),'https://chatgpt.com/','Worker creation must leave a Temporary draft for a saved chat');
  // A new draft has no persistent conversation ID; refresh its real destination.
  const projectUrl="https://chatgpt.com/g/g-p-0123456789abcdef0123456789abcdef/project";
  await page.goto(projectUrl,{waitUntil:"domcontentloaded"}); await waitForConversationComposer(page);
  const beforeDraft=documentLoads;
  await refreshConversationEditor(page,"https://chatgpt.com/c/local-draft",request.expectedAccountFingerprint,new AbortController().signal,()=>{},{projectUrl});
  assert.equal(page.url(),projectUrl);
  assert.equal(documentLoads,beforeDraft+1);
  assert.equal(sends,3,"Refreshing a new draft must not submit initialization");

  // A signed-in shell that never renders is an editor failure, never sign-in.
  mode="missing";await page.goto(url,{waitUntil:"domcontentloaded"});
  await assert.rejects(waitForConversationComposer(page,undefined,120),{code:"editor_unavailable"});
  const abort=new AbortController();setTimeout(()=>abort.abort(),50);
  await assert.rejects(waitForConversationComposer(page,abort.signal,2_000),{name:"AbortError"});
  mode="login";signedOut=true;await page.goto(url,{waitUntil:"domcontentloaded"});
  await assert.rejects(waitForConversationComposer(page),{code:"authentication_required"});

  // Hydration cannot bypass the separate, fresh account fingerprint check.
  mode="delayed";signedOut=false;wrongAccount=true;
  await page.goto("https://chatgpt.com/",{waitUntil:"domcontentloaded"});
  const other=new SingleWindowCoordinator(new ChatGptBrowserDriver(surface,build),build);
  await assert.rejects(other.execute({...request,actionId:"wrong-account",idempotencyKey:"wrong-account"}),/account changed/);
  assert.equal(sends,3,"Account mismatch must not send another continuation");
  await other.close();
  wrongAccount=false;mode='delayed-end-turn';await page.goto(url,{waitUntil:'domcontentloaded'});await waitForConversationComposer(page);
  await page.addStyleTag({content:MINIMAL_CHATGPT_CSS});
  assert.equal(await page.getByTestId('end-turn-button').isVisible(),true,'Compact styling must keep End turn visible');
  await page.getByTestId('end-turn-button').evaluate(element=>{element.removeAttribute('data-testid');element.removeAttribute('aria-label');});
  const endTurn=new SingleWindowCoordinator(new ChatGptBrowserDriver(surface,build),build);
  await endTurn.execute({...request,actionId:'end-then-continue',idempotencyKey:'end-then-continue'});
  const ended=[];for await(const event of endTurn.events('end-then-continue',0))ended.push(event);
  assert.equal(ended.at(-1)?.type,'submission_complete',JSON.stringify(ended));
  assert.equal(await page.locator('body').getAttribute('data-end-requests'),'1');
  assert.equal(steers,0,'A stalled turn must end before sending a new continuation');
  assert.equal(sends,4);await endTurn.close();
  mode='delayed-hidden-running';await page.goto(url,{waitUntil:'domcontentloaded'});await waitForConversationComposer(page);
  const staleTurn=new SingleWindowCoordinator(new ChatGptBrowserDriver(surface,build),build);
  const beforeSteer=sends,beforeSteerLoads=documentLoads;
  await staleTurn.execute({...request,actionId:'hidden-old-turn',idempotencyKey:'hidden-old-turn'});
  const steered=[];for await(const event of staleTurn.events('hidden-old-turn',0))steered.push(event);
  assert.equal(steered.at(-1)?.type,'ambiguous_send',JSON.stringify(steered));
  assert.equal(steered.some(event=>event.type==='send_accepted'),false);
  assert.equal(steers,1);assert.equal(sends,beforeSteer+1,'Never replay the uncertain send');
  assert.equal(documentLoads,beforeSteerLoads,'Never reload after the uncertain send');
  assert.equal(await page.locator('body').getAttribute('data-stop-requests'),'1','Wait for the newly exposed disabled Stop before ending the old turn');
  await staleTurn.execute({...request,actionId:'fresh-after-steer',idempotencyKey:'fresh-after-steer'});
  const fresh=[];for await(const event of staleTurn.events('fresh-after-steer',0))fresh.push(event);
  assert.equal(fresh.at(-1)?.type,'submission_complete',JSON.stringify(fresh));
  assert.equal(steers,1,'The next distinct goal continuation must start a fresh turn');
  assert.equal(sends,beforeSteer+2);await staleTurn.close();
  mode='delayed';await page.goto(url,{waitUntil:'domcontentloaded'});await waitForConversationComposer(page);
  let releaseLate!:()=>void,lateEntered=false,lateClosed=0;
  const lateGate=new Promise<void>(resolve=>{releaseLate=resolve;});
  const lateSurface:SingleBrowserSurface={...surface,navigate:async destination=>{
    lateEntered=true;await lateGate;
    // Reproduce a browser transport that finishes its navigation after abort.
    await page.goto(destination,{waitUntil:'domcontentloaded'});return page;
  },close:async()=>{lateClosed++;}};
  const late=new SingleWindowCoordinator(new ChatGptBrowserDriver(lateSurface,build),build,{operationMs:1000,cleanupMs:100,hardStopMs:100});
  const lateUrl='https://chatgpt.com/c/late-cancelled-load',beforeLate=sends;
  await late.execute({...request,actionId:'late-load',idempotencyKey:'late-load',url:lateUrl,expectedUrl:lateUrl});
  const lateEvents=[];for await(const event of late.events('late-load',0))lateEvents.push(event);
  assert(lateEntered);assert.equal((lateEvents.at(-1) as any).code,'conversation_unavailable');
  assert.equal(lateClosed,0);assert.equal(page.isClosed(),false);assert.equal(sends,beforeLate);
  assert.equal((await late.health()).activeActionId,'late-load');assert.equal((await late.liveness()).available,true);
  await assert.rejects(late.execute({...request,actionId:'while-late-load',idempotencyKey:'while-late-load'}),/still owns/);
  releaseLate();
  const cleanupDeadline=Date.now()+5000;
  while((await late.health()).activeActionId&&Date.now()<cleanupDeadline)await new Promise(resolve=>setTimeout(resolve,50));
  assert.equal((await late.health()).activeActionId,null);assert.equal(sends,beforeLate,'Aborted late navigation cannot submit');
  assert.equal(lateClosed,0);await late.close();assert.equal(lateClosed,1);
  const sameBrowser=new SingleWindowCoordinator(new ChatGptBrowserDriver(surface,build),build);
  await sameBrowser.execute({...request,actionId:'after-late-navigation',idempotencyKey:'after-late-navigation'});
  const afterLate=[];for await(const event of sameBrowser.events('after-late-navigation',0))afterLate.push(event);
  assert.equal(afterLate.at(-1)?.type,'submission_complete',JSON.stringify(afterLate));assert.equal(sends,beforeLate+1);
  await sameBrowser.close();
  mode='delayed';submissionStatus=429;await page.goto(url,{waitUntil:'domcontentloaded'});
  const optimistic=new SingleWindowCoordinator(new ChatGptBrowserDriver(surface,build),build);
  await optimistic.execute({...request,actionId:'rejected-optimistic',idempotencyKey:'rejected-optimistic'});
  const rejected=[];for await(const event of optimistic.events('rejected-optimistic',0))rejected.push(event);
  assert.equal(rejected.at(-1)?.type,'ambiguous_send',JSON.stringify(rejected));
  assert.equal(rejected.some(event=>event.type==='send_accepted'),false,'An optimistic bubble is not server acceptance');
  assert.equal((await optimistic.health()).status,'idle','A server cooldown must preserve the signed-in browser');
  assert.equal(page.isClosed(),false);await optimistic.close();
  console.log(JSON.stringify({ok:true,lateNavigationRetainsBrowserAndLease:true,sameBrowserContinuationAfterLateCleanup:true,serverProofRejectsOptimisticBubble:true,pairedContinuationSkipsAppMenu:true,delayedComposerAcceptedOnce:true,missingEditorClassified:true,abortRespected:true,wrongAccountRejected:true,missingConnectorMs,stuckModelMs,refreshPreservesDraft:true,missingModelKeepsBrowserOpen:true,newDraftRefreshPreservesProject:true,stalledNavigationKeepsBrowserOpen:true}));
} finally {
  console.error("fixture cleanup: releasing routes");
  await context.unrouteAll({behavior:"ignoreErrors"});
  console.error("fixture cleanup: closing Chrome");
  const closingSession=await context.newCDPSession(page).catch(()=>null);
  if(closingSession)await closingSession.send("Browser.close").catch(()=>{});
  await context.close();
  console.error("fixture cleanup: removing profile");
  await rm(profile,{recursive:true,force:true,maxRetries:8,retryDelay:200});
  console.error("fixture cleanup: complete");
}
