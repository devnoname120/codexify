import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { createRequire } from "node:module";
import test from "node:test";
import { ownerHtml, routeOwner } from "./owner-chat-source.mjs";

const { chromium, webkit } = createRequire(import.meta.url)("playwright");
const META = "io.github.devnoname120/codexify/markdown-chat";

async function ownerTestPage(browser, options) {
  const page=await browser.newPage(options);
  const register=page.route.bind(page);
  page.route=(pattern,handler,settings)=>register(pattern,route=>{
    if(new URL(route.request().url()).pathname==='/api/settings/connector')return route.fulfill({json:{configured:true,connectorName:'Fixture connector'}});
    if(new URL(route.request().url()).pathname==='/api/global')return route.fulfill({json:{version:'1.7.0',plan:null,latest:{updateAvailable:false},settingsAvailable:false,restartAvailable:false}});
    if(new URL(route.request().url()).pathname==='/api/analytics/extras')return route.fulfill({status:503,body:'Optional extras unavailable'});
    return handler(route);
  },settings);
  return page;
}

test("owner presence honors configured ten-minute and one-minute boundaries", () => {
  const source = ownerHtml.match(/function presence\([^)]*\) \{[\s\S]*?\n  \}/)?.[0];
  assert(source);
  const ten = Function('offlineAfterMs', `return (${source})`)(600000);
  const one = Function('offlineAfterMs', `return (${source})`)(60000);
  assert.equal(ten(1000, 301000), 'away');
  assert.equal(ten(1000, 600999), 'away');
  assert.equal(ten(1000, 601000), 'offline');
  assert.equal(one(1000, 60999), 'online');
  assert.equal(one(1000, 61000), 'offline');
});

test("owner presence changes at exactly three and five minutes", () => {
  const source = ownerHtml.match(/function presence\([^)]*\) \{[\s\S]*?\n  \}/)?.[0];
  assert(source);
  const presence = Function('offlineAfterMs', `return (${source})`)(300000);
  assert.equal(presence(1000, 1000 + 179999, null), "online");
  assert.equal(presence(1000, 1000 + 180000, null), "away");
  assert.equal(presence(1000, 1000 + 299999, null), "away");
  assert.equal(presence(1000, 1000 + 300000, null), "offline");
});

test("idle waiting deadlines expire independently of question waits and ordinary presence", () => {
  const source = ownerHtml.match(/function presence\([^)]*\) \{[\s\S]*?\n  \}/)?.[0];
  const presence = Function('offlineAfterMs', `return (${source})`)(300000);
  assert.equal(presence(1000, 5999, null, null, 6000), "waiting");
  assert.equal(presence(1000, 6000, null, null, 6000), "online");
  assert.equal(presence(null, 5999, null, null, 6000), "waiting");
  assert.equal(presence(null, 6000, null, null, 6000), "offline");
  assert.equal(presence(1000, 6000, 9000, null, 5999), "waiting");
});

test("goal elapsed time uses compact day/hour/minute/second labels", () => {
  const source = ownerHtml.match(/function goalDuration\([^)]*\) \{[\s\S]*?\n    \}/)?.[0];
  assert(source); const duration = Function(`return (${source})`)();
  for (const [seconds, expected] of [[0,"0s"],[3,"3s"],[252,"4m 12s"],[83434,"23h 10m 34s"],[213819,"2d 11h 23m 39s"]]) assert.equal(duration(seconds * 1000), expected);
});

test("pairing stays pending through submission and queue waits, then exposes expired or failed attempts", () => {
  const source=ownerHtml.match(/function pairingPhase\([^)]*\) \{[\s\S]*?\n  \}/)?.[0];
  assert(source);const phase=Function(`return (${source})`)();
  const now=10000;
  assert.equal(phase({pairing:{status:'unpaired'},action:{status:'idle'}},now),'unpaired');
  assert.equal(phase({pairing:{status:'unpaired',creatingChat:true,expiresAtMs:now+1000},action:{kind:'pair_verify',status:'queued'}},now),'pairing');
  assert.equal(phase({pairing:{status:'unverified',expiresAtMs:now-1},action:{kind:'pair_verify',status:'queued'}},now),'pairing','queued work receives a new deadline when executed');
  for(const status of ['navigating','sending','generating','idle']) {
    assert.equal(phase({pairing:{status:'unverified',expiresAtMs:now+1000},action:{kind:'pair_verify',status}},now),'pairing');
  }
  for(const status of ['failed','ambiguous','stalled','approval_required','authentication_required','rate_limited','interference']) {
    assert.equal(phase({pairing:{status:'unverified',expiresAtMs:now+1000},action:{kind:'pair_verify',status}},now),'failed');
  }
  assert.equal(phase({pairing:{status:'unverified',expiresAtMs:now-1},action:{kind:'pair_verify',status:'idle'}},now),'failed');
  assert.equal(phase({pairing:{status:'unverified'},action:{status:'idle'}},now),'failed');
  assert.equal(phase({pairing:{status:'verified'},action:{kind:'offline_chat',status:'failed'}},now),'verified','a chat send failure does not turn into a pairing retry');
});

for (const [engineName, engine] of [["Chromium", chromium], ["WebKit", webkit]]) {
test(`${engineName}: multiline drafts keep message history stable across background refreshes`, {timeout:45000}, async () => {
  const browser=await engine.launch();
  try {
    const page=await ownerTestPage(browser,{viewport:{width:1280,height:900}});page.setDefaultTimeout(5000);
    const now=Date.now(),id='9'.repeat(64),errors=[];let historyRequests=0;
    page.on('pageerror',error=>errors.push(error.message));
    const messages=Array.from({length:40},(_,i)=>({id:`message-${i}`,role:'agent',markdown:`Progress ${i}. ${'Some details from the ongoing work. '.repeat(15)}`,start:i*100,end:(i+1)*100,created_at_ms:now,tool_call_count:i+1}));
    const goal={status:'completed',markdown:'Completed goal',revision:1,elapsedActiveMs:3600000,turnCount:1,finishedAtMs:now-1000};
    await routeOwner(page, 'http://127.0.0.1:43221/**',route=>{
      const url=new URL(route.request().url());
      if(url.pathname==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
      if(url.pathname==='/api/chats')return route.fulfill({json:{chats:[{id,title:'Multiline draft',workspace:'scrolling',lastEntryEnd:4000,lastEntryAtMs:now,lastAgentCallAtMs:now,totalToolCalls:40,goalStatus:'completed',goalCompletionDismissed:true}],serverTimeMs:Date.now()}});
      if(url.pathname.endsWith('/automation'))return route.fulfill({json:{enabled:true,goal,pairing:{status:'verified'},action:{status:'idle'},selection:{model:'gpt-6-astra',effort:'max'},companion:{authenticated:true,status:'idle'},catalog:{models:[{id:'gpt-6-astra',label:'GPT-6 Astra',efforts:[{id:'max',label:'Pro',available:true}]}]},presence:{lastAgentCallAtMs:now},serverTimeMs:Date.now()}});
      historyRequests++;
      return route.fulfill({json:{_meta:{[META]:{chat_file:'/private/multiline/CHAT.md',revision:'4000',delivered_through:0,read_through:0,last_agent_call_at_ms:now,total_tool_calls:40+historyRequests,server_time_ms:Date.now(),messages,has_more:false,unchanged:historyRequests>1}}}});
    });
    await page.goto(`http://127.0.0.1:43221/?chat=${id}#test-token`);
    const host=page.locator('#chat-host'),history=host.locator('#messages'),input=host.getByRole('textbox',{name:'Message the agent',exact:true});
    await history.locator('.message').last().waitFor();await host.locator('#goal-bar').waitFor();
    for(const draft of ['first\nsecond\nthird\nfourth\nfifth','first\nsecond\n\nfourth']) {
      await input.fill(draft);
      for(const reading of [false,true]) {
        await history.evaluate((node,reading)=>{node.scrollTop=node.scrollHeight-node.clientHeight-(reading?250:0);},reading);
        const samples=await history.evaluate(async history=>{
          const root=history.getRootNode(),draft=root.querySelector('#draft'),composer=root.querySelector('#composer');
          const result=[];const until=performance.now()+4200;
          do {
            await new Promise(requestAnimationFrame);
            result.push({top:history.scrollTop,height:history.clientHeight,composerY:composer.getBoundingClientRect().y,draftHeight:draft.getBoundingClientRect().height,draftText:draft.value,pageTop:scrollY});
          }while(performance.now()<until);
          return result;
        });
        const range=key=>Math.max(...samples.map(s=>s[key]))-Math.min(...samples.map(s=>s[key]));
        assert(range('height')<1&&range('composerY')<1&&range('draftHeight')<1,'the final layout remains unchanged between refreshes');
        assert(range('top')<1,`${reading?'reading older replies':'at bottom'}: history jumped ${range('top')}px with no input`);
        assert(samples.every(s=>s.draftText===draft),'background refreshes preserve the draft');
        assert(range('pageTop')<1,'the outer page must remain fixed');
      }
    }
    assert(historyRequests>3,'the observation must include repeated history refreshes');
    assert.deepEqual(errors,[]);
  }finally{await browser.close();}
});

test(`${engineName}: unpaired composer offers Pair chat until verification completes`, {timeout:30000}, async () => {
  const browser=await engine.launch();
  try {
    const page=await ownerTestPage(browser);page.setDefaultTimeout(5000);
    const id="e".repeat(64),errors=[],operations=[];
    page.on('pageerror',error=>errors.push(error.message));
    const chat={id,title:'Unpaired chat',workspace:'test',lastEntryEnd:1,lastEntryAtMs:Date.now(),lastAgentCallAtMs:Date.now(),totalToolCalls:1};
    const state={enabled:true,pairing:{status:'unpaired'},goal:null,action:{status:'idle'},selection:{model:'gpt-6-astra',effort:'max'},companion:{authenticated:true,status:'idle'},presence:{lastAgentCallAtMs:chat.lastAgentCallAtMs}};
    await routeOwner(page, 'http://127.0.0.1:43220/**',route=>{
      const request=route.request(),url=new URL(request.url());
      if(request.method()!=='GET') {operations.push(url.pathname);return route.fulfill({status:405});}
      if(url.pathname==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
      if(url.pathname==='/api/chats')return route.fulfill({json:{chats:[chat],serverTimeMs:Date.now()}});
      if(url.pathname.endsWith('/automation'))return route.fulfill({json:{...state,serverTimeMs:Date.now()}});
      return route.fulfill({json:{_meta:{[META]:{chat_file:'/private/unpaired/CHAT.md',revision:'1',delivered_through:0,read_through:0,last_agent_call_at_ms:chat.lastAgentCallAtMs,total_tool_calls:1,server_time_ms:Date.now(),messages:[],has_more:false,unchanged:false}}}});
    });
    for(const width of [1280,375]) {
      state.pairing={status:'unpaired'};state.action={status:'idle'};delete state.catalog;
      await page.setViewportSize({width,height:900});
      await page.goto(`http://127.0.0.1:43220/?chat=${id}&viewport=${width}#test-token`);
      const host=page.locator('#chat-host'),pair=host.getByRole('button',{name:'Pair chat',exact:true});
      await pair.waitFor();
      for(const control of ['automation-add-goal','automation-model','automation-effort']) assert.equal(await host.locator('#'+control).isVisible(),false);
      assert.equal(await host.locator('#automation-settings').isVisible(),false);
      await host.getByRole('button',{name:'Stop agent',exact:true}).waitFor();
      const box=await pair.boundingBox(),send=await host.locator('#send').boundingBox();
      assert(box&&send&&box.x+box.width<=send.x&&box.x>=0,`${width}px: Pair chat must fit beside Send/Stop`);
      if(process.env.CODEXIFY_WORKSPACE_SCREENSHOTS) {
        mkdirSync(process.env.CODEXIFY_WORKSPACE_SCREENSHOTS,{recursive:true});
        await page.screenshot({path:`${process.env.CODEXIFY_WORKSPACE_SCREENSHOTS}/${engineName.toLowerCase()}-unpaired-${width}.png`,fullPage:true});
      }
      await pair.click();
      await host.getByLabel('ChatGPT conversation URL').waitFor();
      assert.equal(await host.getByLabel('ChatGPT conversation URL').evaluate(node=>node===node.getRootNode().activeElement),true);
      assert.equal(await host.locator('#automation-settings').evaluate(node=>node.open),true,'Pair chat opens a modal connection dialog');
      state.pairing={status:'unverified',candidateUrl:'https://chatgpt.com/c/pending-fixture'};
      state.action={status:'queued',kind:'pair_verify'};
      await host.getByText('Pairing verification queued',{exact:true}).waitFor();
      await host.getByRole('button',{name:'Close connection settings',exact:true}).click();
      const pairing=host.getByRole('button',{name:'Pairing…',exact:true});
      await pairing.waitFor();
      assert.equal(await pairing.isDisabled(),true,'a pending pairing cannot be started again from the composer');
      assert.equal(await host.locator('#automation-model').isVisible(),false);
      state.pairing={status:'verified',canonicalUrl:'https://chatgpt.com/c/pending-fixture'};
      state.action={status:'idle'};
      state.catalog={models:[{id:'gpt-6-astra',label:'GPT-6 Astra',efforts:[{id:'max',label:'Pro',available:true}]}]};
      await host.locator('#automation-pair-chat').waitFor({state:'hidden'});
      for(const control of ['automation-add-goal','automation-model','automation-effort']) await host.locator('#'+control).waitFor();
      assert.equal(await host.locator('#automation-model').inputValue(),'gpt-6-astra');
      assert.equal(await host.locator('#automation-effort').inputValue(),'max');
      state.pairing={status:'unpaired'};
      await pair.waitFor();
      for(const control of ['automation-add-goal','automation-model','automation-effort']) assert.equal(await host.locator('#'+control).isVisible(),false);
    }
    assert.deepEqual(operations,[],'opening pairing must not submit a request or scan models');
    assert.deepEqual(errors,[]);
  } finally {await browser.close();}
});

test(`${engineName}: pairing failures open a prefilled retry box and submit one new request`, {timeout:30000}, async () => {
  const browser=await engine.launch();
  let releasePost,releasePoll;
  try {
    const page=await ownerTestPage(browser);page.setDefaultTimeout(5000);await page.clock.install();
    const id='f'.repeat(64),errors=[],submitted=[];
    const chat={id,title:'Pairing retry',workspace:'test',lastEntryEnd:0,lastEntryAtMs:Date.now(),lastAgentCallAtMs:null,totalToolCalls:0};
    const oldUrl='https://chatgpt.com/g/project/c/previous-attempt';
    const state={enabled:true,pairing:{status:'unverified',lastTriedUrl:oldUrl},goal:null,action:{kind:'pair_verify',status:'failed',lastError:'Pairing failed before Send'},selection:{},companion:{authenticated:true,status:'idle'},presence:{}};
    let holdNextPoll=false,pollStarted,postStarted,rejectReceipt=false;
    const postReady=new Promise(resolve=>{postStarted=resolve;});
    page.on('pageerror',error=>errors.push(error.message));
    await routeOwner(page, 'http://127.0.0.1:43226/**',async route=>{
      const request=route.request(),url=new URL(request.url());
      if(url.pathname==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
      assert.equal(request.headers().authorization,'Bearer test-token');
      if(url.pathname==='/api/chats')return route.fulfill({json:{chats:[chat],serverTimeMs:Date.now()}});
      if(url.pathname.endsWith('/automation/pair')) {
        submitted.push(request.postDataJSON());postStarted();
        await new Promise(resolve=>{releasePost=resolve;});
        state.pairing={status:'unverified',lastTriedUrl:submitted.at(-1).url,candidateUrl:submitted.at(-1).url,expiresAtMs:Date.now()+600000};
        state.action={kind:'pair_verify',status:'queued'};
        if(rejectReceipt)return route.fulfill({status:503,body:'Pairing response was lost'});
        return route.fulfill({json:{pairing:state.pairing,action:state.action}});
      }
      if(url.pathname.endsWith('/automation')) {
        const snapshot=structuredClone({...state,serverTimeMs:Date.now()});
        if(holdNextPoll) {holdNextPoll=false;pollStarted();await new Promise(resolve=>{releasePoll=resolve;});}
        return route.fulfill({json:snapshot});
      }
      return route.fulfill({json:{_meta:{[META]:{chat_file:'/private/retry/CHAT.md',revision:'0',delivered_through:0,read_through:0,last_agent_call_at_ms:null,total_tool_calls:0,server_time_ms:Date.now(),messages:[],has_more:false,unchanged:false}}}});
    });
    await page.goto(`http://127.0.0.1:43226/?chat=${id}#test-token`);
    const host=page.locator('#chat-host'),retry=host.getByRole('button',{name:'Retry chat pairing',exact:true});
    const editor=host.locator('#pairing-editor'),field=editor.getByLabel('ChatGPT URL'),pair=editor.getByRole('button',{name:'Pair',exact:true});
    await retry.waitFor();await retry.click();
    assert.equal(await field.inputValue(),oldUrl);
    assert.equal(await pair.isEnabled(),true);
    const editedUrl='https://chatgpt.com/c/edited-retry';
    await field.fill(editedUrl);await page.clock.fastForward(1600);
    assert.equal(await field.inputValue(),editedUrl,'status polling must not overwrite an edited retry URL');
    assert.equal(submitted.length,0,'opening the box is not a pairing submission');
    const pollReady=new Promise(resolve=>{pollStarted=resolve;});
    holdNextPoll=true;await page.clock.fastForward(1600);await pollReady;
    await pair.click();
    const pending=host.getByRole('button',{name:'Pairing…',exact:true});
    await pending.waitFor();assert.equal(await pending.isDisabled(),true);
    assert.equal(await editor.isVisible(),false);
    await postReady;
    assert.equal(submitted.length,1);assert.equal(submitted[0].url,editedUrl);
    releasePost();releasePost=null;
    await host.getByText('Pairing verification queued',{exact:true}).waitFor({state:'attached'});
    const oldPollResponse=page.waitForResponse(response=>new URL(response.url()).pathname.endsWith('/automation'));
    releasePoll();releasePoll=null;
    await oldPollResponse;
    await page.clock.fastForward(1600);
    assert.equal(await pending.isDisabled(),true,'a stale pre-submit poll must not bring back a retry button');

    for(const width of [1280,375]) {
      await page.setViewportSize({width,height:900});
      state.pairing={status:'unpaired',creatingChat:true,lastTriedUrl:null,candidateUrl:'https://chatgpt.com/g/g-p-project/project'};
      state.action={kind:'pair_verify',status:'failed',lastError:'Creation failed'};
      await page.clock.fastForward(1600);await retry.waitFor();await retry.click();
      assert.equal(await field.inputValue(),'','no created conversation means an empty prefill');
      assert.equal(await pair.isDisabled(),true);
      await field.fill('https://chatgpt.com/c/manual');assert.equal(await pair.isEnabled(),true);
      const box=await editor.boundingBox();assert(box&&box.x>=0&&box.x+box.width<=width,`${width}px: retry box must fit in the viewport`);
      await editor.getByRole('button',{name:'Close pairing',exact:true}).click();
      state.pairing={status:'unverified',candidateUrl:oldUrl,expiresAtMs:Date.now()-1};state.action={kind:'pair_verify',status:'idle'};
      await page.clock.fastForward(1600);await retry.waitFor();await retry.click();
      assert.equal(await field.inputValue(),oldUrl,'legacy pairing state can still prefill its candidate URL');
      await editor.getByRole('button',{name:'Close pairing',exact:true}).click();
    }
    await retry.click();rejectReceipt=true;
    const retryPostReady=new Promise(resolve=>{postStarted=resolve;});
    await pair.click();await retryPostReady;
    releasePost();releasePost=null;
    await page.clock.fastForward(1600);await pending.waitFor();
    assert.equal(await pending.isDisabled(),true,'a failed HTTP receipt must not override a server-confirmed pending pairing');
    assert.equal(submitted.length,2,'each explicit Pair click submits one request');
    assert.deepEqual(errors,[]);
  } finally {releasePost?.();releasePoll?.();await browser.close();}
});

test(`${engineName}: failed chats show an inline retry and a sidebar error without a goal`, {timeout:45000}, async () => {
  const browser=await engine.launch();
  try {
    const page=await ownerTestPage(browser,{viewport:{width:1280,height:900}});page.setDefaultTimeout(5000);
    const now=Date.now(),id="f".repeat(64),errors=[],operations=[];
    page.on('pageerror',error=>errors.push(error.message));
    const chat={id,title:"Failed message",workspace:"test",lastEntryEnd:1,lastEntryAtMs:now,lastAgentCallAtMs:null,totalToolCalls:1};
    let action={id:"failed-action",kind:"offline_chat",status:"failed",lastError:"ChatGPT connector menu did not open",sendAccepted:false,retryMode:"retry",automaticRetryAtMs:null,automaticRetryAttempts:5};
    await routeOwner(page, 'http://127.0.0.1:43219/**',route=>{
      const request=route.request(),url=new URL(request.url());
      if(url.pathname==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
      if(url.pathname==='/api/chats')return route.fulfill({json:{chats:[{...chat,automationError:['failed','ambiguous'].includes(action.status)?action.lastError:null,automationRetrying:Number.isSafeInteger(action.automaticRetryAtMs)}],serverTimeMs:Date.now()}});
      if(url.pathname.endsWith('/automation'))return route.fulfill({json:{enabled:true,goal:null,action,catalog:{models:[]},pairing:{status:'verified'},companion:{authenticated:true,status:'idle'},serverTimeMs:Date.now(),presence:{lastAgentCallAtMs:null}}});
      if(url.pathname.endsWith('/action/retry')) {
        const body=request.postDataJSON();operations.push(body);
        assert.equal(body.actionId,action.id);
        action={...action,id:'retried-action',status:'queued',lastError:null,retryMode:null,automaticRetryAtMs:null,automaticRetryAttempts:0};
        return route.fulfill({json:{action}});
      }
      return route.fulfill({json:{_meta:{[META]:{chat_file:'/private/test/CHAT.md',revision:'1',delivered_through:0,read_through:0,last_agent_call_at_ms:null,total_tool_calls:1,server_time_ms:Date.now(),messages:[],has_more:false,unchanged:false}}}});
    });
    await page.goto(`http://127.0.0.1:43219/?chat=${id}#test-token`);
    const host=page.locator('#chat-host'),inline=host.locator('#automation-retry-inline');
    await inline.waitFor();
    assert.equal(await host.locator('#goal-bar').isVisible(),false);
    assert.equal(await host.getByRole('button',{name:'Dismiss error',exact:true}).isVisible(),true);
    assert.equal(await host.locator('#automation-settings').isVisible(),false);
    assert.equal(await inline.textContent(),'Retry');
    assert((await host.locator('#automation-error').textContent()).includes('All 5 automatic retries failed'));
    await page.locator('.conversation .presence.error').waitFor();
    assert((await page.locator('.conversation .presence.error').getAttribute('aria-label')).includes('connector menu'));
    assert.equal(await page.locator('.conversation .presence.error').evaluate(node=>getComputedStyle(node).color),'rgb(196, 49, 59)');
    assert.equal(await host.locator('#presence .presence-label').textContent(),'offline');
    await inline.click();
    await inline.waitFor({state:'hidden'});
    await page.locator('.conversation .presence.error').waitFor({state:'detached'});
    assert.equal(operations.length,1);
    action={id:'pending-retry',kind:'offline_chat',status:'failed',lastError:'Connector menu unavailable',sendAccepted:false,retryMode:'retry',automaticRetryAtMs:Date.now()+30000,automaticRetryAttempts:0};
    await host.locator('#automation-error').getByText(/Retrying automatically in \d+s \(retry 1 of 5\)/).waitFor();
    await page.locator('.conversation .presence.retrying .presence-spinner').waitFor();
    await inline.waitFor();
    await inline.click();await inline.waitFor({state:'hidden'});
    assert.equal(operations.length,2,'manual Retry works during automatic backoff');
    action={id:'authentication-backoff',kind:'offline_chat',status:'authentication_required',lastError:'Sign in to ChatGPT, then retry this action.',sendAccepted:false,retryMode:'retry',automaticRetryAtMs:Date.now()+180000,automaticRetryAttempts:0,automaticRetryLimit:6};
    await host.locator('#automation-error').getByText(/retry 1 of 6/).waitFor();
    action={...action,automaticRetryAtMs:Date.now()+3600000,automaticRetryAttempts:5};
    await host.locator('#automation-error').getByText(/retry 6 of 6/).waitFor();
    assert(!(await host.locator('#automation-error').textContent()).includes('All 6 automatic retries failed'));
    action={...action,automaticRetryAtMs:null,automaticRetryAttempts:6};
    await host.locator('#automation-error').getByText(/All 6 automatic retries failed/).waitFor();
    assert(await inline.isVisible(),'exhausted authentication retries still allow a manual retry');
    action={...action,id:'conversation-load-backoff',status:'conversation_unavailable',lastError:'ChatGPT conversation temporarily not accessible',automaticRetryAtMs:Date.now()+600000,automaticRetryAttempts:2};
    await host.locator('#automation-error').getByText(/retry 3 of 6/).waitFor();
    assert(!(await host.locator('#automation-error').textContent()).includes('Sign in'));
    action={...action,automaticRetryAtMs:null,automaticRetryAttempts:6};
    await host.locator('#automation-error').getByText(/All 6 automatic retries failed/).waitFor();
    assert(!(await host.locator('#automation-error').textContent()).includes('Sign in'));
    assert(await inline.isVisible());
    action={id:'uncertain-send',kind:'offline_chat',status:'ambiguous',lastError:'Send acceptance could not be confirmed',sendAccepted:true,retryMode:null,automaticRetryAtMs:null,automaticRetryAttempts:0};
    await host.locator('#automation-error').getByText(/check whether the message was sent/).waitFor();
    assert.equal(await inline.isVisible(),false,'an uncertain user message cannot be replayed');
    await page.locator('.conversation .presence.error').waitFor();
    await page.emulateMedia({colorScheme:'dark'});
    assert.equal(await page.locator('.conversation .presence.error').evaluate(node=>getComputedStyle(node).color),'rgb(255, 133, 142)');
    action={id:'recovered',kind:'offline_chat',status:'idle',lastError:null,sendAccepted:true,retryMode:null,automaticRetryAtMs:null,automaticRetryAttempts:0};
    await page.locator('.conversation .presence.error').waitFor({state:'detached'});
    assert.deepEqual(errors,[]);
  } finally {await browser.close();}
});

test(`${engineName}: completed sidebar badges clear after follow-up and leave the goal bar completed`, { timeout:30000 }, async () => {
  const browser=await engine.launch();
  try {
    const page=await ownerTestPage(browser);page.setDefaultTimeout(5000);
    const now=Date.now(),id="c".repeat(64);
    let goal={revision:1,markdown:"Completed objective",status:"completed",createdAtMs:now-5000,finishedAtMs:now-1000,elapsedActiveMs:4000,turnCount:1};
    const chat={id,title:"Completed objective chat",workspace:"completed",goalStatus:"completed",goalCompletionDismissed:false,goalCompletedAtMs:goal.finishedAtMs,goalCompletedChatEnd:42,lastAgentCallAtMs:now,lastEntryEnd:42,lastEntryAtMs:now,totalToolCalls:1};
    const errors=[];page.on('pageerror',error=>errors.push(error.message));
    await routeOwner(page, 'http://127.0.0.1:43218/**',route=>{
      const request=route.request(),url=new URL(request.url());
      if(url.pathname==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
      if(url.pathname==='/api/chats')return route.fulfill({json:{chats:[chat],serverTimeMs:Date.now()}});
      if(url.pathname.endsWith('/automation'))return route.fulfill({json:{enabled:true,goal,pairing:{status:'verified'},serverTimeMs:Date.now(),companion:{authenticated:false},presence:{lastAgentCallAtMs:chat.lastAgentCallAtMs}}});
      if(url.pathname.endsWith('/send')){
        const body=request.postDataJSON();
        if(body.message==='/goal clear'){
          goal=null;chat.goalStatus=null;chat.goalCompletionDismissed=false;
          return route.fulfill({json:{_meta:{[META]:{localCommand:{},goal:null}}}});
        }
        if(body.message.startsWith('/goal set ')){
          goal={revision:2,markdown:body.message.slice(10),status:'active',createdAtMs:Date.now(),elapsedActiveMs:0,turnCount:0};
          chat.goalStatus='active';chat.goalCompletionDismissed=false;
          return route.fulfill({json:{_meta:{[META]:{localCommand:{},goal}}}});
        }
        chat.goalCompletionDismissed=true;chat.lastEntryEnd=43;
        return route.fulfill({json:{_meta:{[META]:{sent:{id:body.request_id,end:43,created_at_ms:Date.now(),tool_call_count:1}}}}});
      }
      return route.fulfill({json:{_meta:{[META]:{chat_file:'/private/completed/CHAT.md',revision:'42',delivered_through:0,read_through:0,last_agent_call_at_ms:chat.lastAgentCallAtMs,total_tool_calls:1,server_time_ms:Date.now(),messages:[],has_more:false,unchanged:false}}}});
    });
    await page.goto(`http://127.0.0.1:43218/?chat=${id}#test-token`);
    const host=page.locator('#chat-host');
    await page.locator('.conversation .presence.goal-completed').waitFor();
    await host.getByText('Goal completed',{exact:true}).waitFor();
    const input=host.getByRole('textbox',{name:'Message the agent',exact:true});
    await input.fill('Continue with the next request');await input.press('Enter');
    await page.locator('.conversation .presence.online .presence-spinner').waitFor();
    assert.equal(await host.locator('#goal-caption').textContent(),'Goal completed');
    assert.equal(await host.locator('#goal-icon .goal-outer-ring').evaluate(node=>getComputedStyle(node).animationName),'none');
    await page.reload();await page.locator('.conversation .presence.online .presence-spinner').waitFor();
    chat.lastAgentCallAtMs=null;
    await page.evaluate(()=>window.dispatchEvent(new Event('focus')));
    await page.locator('.conversation .presence.offline').waitFor({state:'attached'});
    assert.equal(await page.locator('.conversation .presence.offline').isVisible(),false);

    chat.goalCompletionDismissed=false;await page.reload();
    await page.locator('.conversation .presence.goal-completed').waitFor();
    await host.getByRole('button',{name:'Clear goal',exact:true}).click();
    await host.locator('#goal-bar').waitFor({state:'hidden'});
    await page.evaluate(()=>window.dispatchEvent(new Event('focus')));
    await page.locator('.conversation .presence.offline').waitFor({state:'attached'});
    assert.equal(await page.locator('.conversation .presence.offline').isVisible(),false);
    await host.getByRole('button',{name:'Add goal',exact:true}).click();
    await host.getByLabel('Goal Markdown').fill('Pursue a new objective');
    chat.lastAgentCallAtMs=Date.now();
    await host.getByRole('button',{name:'Set goal',exact:true}).click();
    await page.evaluate(()=>window.dispatchEvent(new Event('focus')));
    await page.locator('.conversation .presence.goal-running').waitFor();
    assert.deepEqual(errors,[]);
  } finally {await browser.close();}
});

test(`${engineName}: reopening a running goal editor immediately enables its restored text`, { timeout:15000 }, async () => {
  const browser=await engine.launch();
  let releasePoll;
  try {
    const page=await ownerTestPage(browser);page.setDefaultTimeout(5000);
    const now=Date.now(),id="e".repeat(64);
    let goal={revision:1,markdown:"Continue the running goal.",status:"active",createdAtMs:now,elapsedActiveMs:1000,turnCount:1};
    let stateRequests=0,holdPoll=true,pollStarted;
    const started=new Promise(resolve=>{pollStarted=resolve;});
    const held=new Promise(resolve=>{releasePoll=resolve;});
    const submissions=[],errors=[];
    page.on("pageerror",error=>errors.push(error.message));
    await routeOwner(page, "http://127.0.0.1:43217/**",async route=>{
      const request=route.request(),url=new URL(request.url());
      if(url.pathname==="/")return route.fulfill({contentType:"text/html",body:ownerHtml});
      if(url.pathname==="/api/chats")return route.fulfill({json:{chats:[{id,title:"Running goal",workspace:"goal-editor",lastAgentCallAtMs:now,lastEntryEnd:0,lastEntryAtMs:now,totalToolCalls:1,goalStatus:goal.status}],serverTimeMs:Date.now()}});
      if(url.pathname.endsWith("/automation")){
        if(++stateRequests>1&&holdPoll){pollStarted();await held;}
        return route.fulfill({json:{enabled:true,goal,pairing:{status:"verified"},serverTimeMs:Date.now(),companion:{authenticated:true,status:"busy",activeActionId:"other-browser-action"},presence:{lastAgentCallAtMs:now},catalog:{models:[{id:"cached-model",label:"Cached model",efforts:[{id:"cached-effort",label:"Cached effort",available:true}]}]}}});
      }
      if(url.pathname.endsWith("/send")){
        const body=request.postDataJSON();submissions.push(body.message);
        goal={...goal,revision:goal.revision+1,markdown:body.message.slice(10)};
        return route.fulfill({json:{_meta:{[META]:{goal}}}});
      }
      return route.fulfill({json:{_meta:{[META]:{chat_file:"/private/goal-editor/CHAT.md",revision:"0",delivered_through:0,read_through:0,last_agent_call_at_ms:now,total_tool_calls:1,server_time_ms:Date.now(),messages:[],has_more:false,unchanged:false}}}});
    });
    await page.goto(`http://127.0.0.1:43217/?chat=${id}#test-token`);
    const host=page.locator("#chat-host"),edit=host.getByRole("button",{name:"Edit goal",exact:true});
    const input=host.getByLabel("Goal Markdown"),save=host.getByRole("button",{name:"Set goal",exact:true});
    await edit.waitFor();await started;
    await edit.click();
    await input.fill("");assert.equal(await save.isDisabled(),true);
    await host.getByRole("button",{name:"Close goal editor",exact:true}).click();
    await edit.click();
    assert.equal(await input.inputValue(),goal.markdown);
    assert.equal(await save.isDisabled(),false,"restoring nonempty goal text must immediately enable Set goal, even while the browser and status poll are busy");
    await input.fill(" \n ");assert.equal(await save.isDisabled(),true);
    await input.fill("Continue the updated running goal.");
    assert.equal(await save.isDisabled(),false);
    await save.click();
    await host.locator("#goal-text").getByText("Continue the updated running goal.",{exact:true}).waitFor();
    assert.equal(goal.status,"active");assert.equal(goal.revision,2);
    assert.deepEqual(submissions,["/goal set Continue the updated running goal."]);
    holdPoll=false;releasePoll();
    await page.waitForTimeout(50);
    assert.deepEqual(errors,[]);
  }finally{releasePoll?.();await browser.close();}
});

test(`${engineName}: opening a goal chat stays at the bottom after controls load`, { timeout:45000 }, async () => {
  const browser = await engine.launch();
  try {
    for (const width of [1280,390]) for (const order of ["history-first","goal-first"]) {
      const page = await ownerTestPage(browser,{viewport:{width,height:900}});
      page.setDefaultTimeout(5000);
      const now = Date.now();
      const chats = ["a","b"].map((letter,i) => ({id:letter.repeat(64),title:i ? "Other chat" : "Running goal chat",workspace:"scrolling",lastEntryEnd:5000,lastEntryAtMs:now-i,lastAgentCallAtMs:now,totalToolCalls:50}));
      const messages = Array.from({length:50},(_,i) => ({id:`reply-${i}`,role:"agent",markdown:`Reply ${i}. ${"Details from the latest progress update. ".repeat(8)}`,start:i*100,end:(i+1)*100,created_at_ms:now,tool_call_count:i+1}));
      const activeGoal = {status:"active",markdown:"Continue the running goal.",revision:1,elapsedActiveMs:1000,activeSinceMs:now,turnCount:1};
      let goal = activeGoal, gateKind, releaseGate, gate;
      const hold = kind => {gateKind=kind;gate=new Promise(resolve=>{releaseGate=resolve;});};
      hold(order === "history-first" ? "automation" : "history");
      const errors=[];
      page.on("pageerror",error=>errors.push(error.message));
      await routeOwner(page, "http://127.0.0.1:43216/**",async route=>{
        const url=new URL(route.request().url());
        if(url.pathname==="/")return route.fulfill({contentType:"text/html",body:ownerHtml});
        if(url.pathname==="/api/chats")return route.fulfill({json:{chats:chats.map((chat,i)=>({...chat,goal:i===0?goal:null,goalStatus:i===0?goal?.status:null})),automationEnabled:true,serverTimeMs:Date.now()}});
        const isGoalChat=url.pathname.split("/")[3]===chats[0].id;
        const kind=url.pathname.endsWith("/automation") ? "automation" : "history";
        if(isGoalChat && gateKind===kind){gateKind=null;await gate;}
        if(kind==="automation")return route.fulfill({json:{enabled:isGoalChat,goal:isGoalChat ? goal : null,serverTimeMs:Date.now(),companion:{authenticated:false},presence:{lastAgentCallAtMs:now}}});
        return route.fulfill({json:{_meta:{[META]:{
          chat_file:`/private/${isGoalChat ? "goal" : "other"}/CHAT.md`,revision:"5000",delivered_through:0,read_through:0,last_agent_call_at_ms:now,total_tool_calls:50,server_time_ms:Date.now(),messages:isGoalChat ? messages : messages.slice(-2),has_more:false,unchanged:false
        }}}});
      });
      await page.goto("http://127.0.0.1:43216/#test-token");
      await page.locator(".conversation").filter({ hasText:/Running goal chat/ }).click();
      const history=page.locator("#chat-host #messages");
      const goalBar=page.locator("#chat-host #goal-bar");
      const bottomGap=()=>history.evaluate(node=>node.scrollHeight-node.clientHeight-node.scrollTop);
      if(order==="history-first"){
        await history.locator(".message").last().waitFor();
        assert(await bottomGap()<1,"history initially reaches its bottom");
      }else await goalBar.waitFor();
      releaseGate();
      await goalBar.waitFor();
      await history.locator(".message").last().waitFor();
      const sidebarFont=await page.locator('.conversation[aria-current="true"] .conversation-title').evaluate(node=>({size:getComputedStyle(node).fontSize,weight:getComputedStyle(node).fontWeight}));
      for(const selector of ["#goal-caption","#goal-text"]){
        assert.equal(await page.locator(`#chat-host ${selector}`).evaluate(node=>getComputedStyle(node).fontSize),sidebarFont.size,`${width}px: sidebar titles and goal text must use the same font size`);
      }
      assert.equal(await page.locator("#chat-host #goal-caption").evaluate(node=>getComputedStyle(node).fontWeight),sidebarFont.weight);
      await page.waitForTimeout(100);
      assert(await bottomGap()<1,`${width}px ${order}: loading goal controls must keep the last reply fully visible`);

      const showList=async()=>{const back=page.locator('#chat-host').getByRole('button',{name:'Back to chats',exact:true});if(await back.isVisible())await back.click();};
      await showList();
      await page.locator(".conversation").filter({ hasText:/Other chat/ }).click();
      await page.locator("#chat-host #composer").waitFor();
      hold("automation");
      await showList();
      await page.locator(".conversation").filter({ hasText:/Running goal chat/ }).click();
      await history.locator(".message").last().waitFor();
      releaseGate();
      await goalBar.waitFor();
      await page.waitForTimeout(100);
      assert(await bottomGap()<1,"returning to a running goal chat must also reach its bottom");

      await history.evaluate(node=>{node.scrollTop-=300;});
      const readingTop=await history.evaluate(node=>node.scrollTop);
      goal=null;
      await goalBar.waitFor({state:"hidden"});
      goal=activeGoal;
      await goalBar.waitFor();
      assert(Math.abs(await history.evaluate(node=>node.scrollTop)-readingTop)<1,"goal controls must preserve the position when reading older replies");
      assert(await bottomGap()>50);
      assert.deepEqual(errors,[]);
      await page.close();
    }
  }finally{await browser.close();}
});

test(`${engineName}: sidebar goal states and spinners leave header presence unchanged`, { timeout:30000 }, async () => {
  const browser = await engine.launch();
  try {
    const page = await ownerTestPage(browser);
    page.setDefaultTimeout(5000);
    const now = Date.now();
    const cases = [
      [null, now, null, "online"],
      ["active", now, null, "goal-running"],
      ["active", now - 240000, null, "goal-running"],
      ["configured", now, null, "goal-paused"],
      ["configured", null, null, "goal-paused"],
      ["blocked", now, null, "goal-blocked"],
      ["blocked", null, null, "goal-blocked"],
      ["active", null, null, "offline"],
      [null, now, now + 60000, "waiting"],
      [null, now - 240000, null, "away"],
      ["completed", now, null, "goal-completed"],
      ["completed", null, null, "goal-completed"],
      ["completed", now, null, "online", true],
      ["completed", null, null, "offline", true],
      ["active", null, null, "browser-cooldown", false, {automationWaitingForBrowser:true}],
      [null, null, null, "error", false, {automationWaitingForBrowser:true,automationError:"Browser action failed"}]
    ];
    const chats = cases.map(([goalStatus, lastAgentCallAtMs, agentWaitingUntilMs, , goalCompletionDismissed = false, flags = {}], i) => ({
      id:i.toString(16).repeat(64), title:`Icon case ${i}`, workspace:"icons", goalStatus,
      goalCompletionDismissed, goalCompletedAtMs:now-1000, goalCompletedChatEnd:0,
      lastAgentCallAtMs, agentWaitingUntilMs, lastEntryEnd:0, lastEntryAtMs:now - i, totalToolCalls:1, ...flags
    }));
    const errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await routeOwner(page, "http://127.0.0.1:43215/**", route => {
      const url = new URL(route.request().url());
      if (url.pathname === "/") return route.fulfill({ contentType:"text/html", body:ownerHtml });
      if (url.pathname === "/api/chats") return route.fulfill({ json:{ chats, serverTimeMs:Date.now() } });
      if (url.pathname.endsWith("/automation")) return route.fulfill({ json:{ enabled:false } });
      const chat = chats.find(chat => chat.id === url.pathname.split("/")[3]);
      return route.fulfill({ json:{ _meta:{ [META]:{
        chat_file:"/private/CHAT.md", revision:"0", delivered_through:0, read_through:0,
        last_agent_call_at_ms:chat.lastAgentCallAtMs, agent_waiting_until_ms:chat.agentWaitingUntilMs,
        total_tool_calls:1, server_time_ms:Date.now(), messages:[], has_more:false, unchanged:false
      } } } });
    });
    await page.goto(`http://127.0.0.1:43215/?chat=${chats[0].id}#test-token`);
    await page.locator(".conversation").last().waitFor();
    const icons = page.locator(".conversation .presence");
    assert.deepEqual(await icons.evaluateAll(nodes => nodes.map(node => [...node.classList].at(-1))), cases.map(row => row[3]));
    assert.equal(await icons.nth(7).isVisible(),false,"offline sidebar icons are hidden");
    for(const index of [10,11]) {
      assert.equal(await icons.nth(index).locator('.goal-outer-ring').count(),1);
      assert.equal(await icons.nth(index).locator('.goal-outer-ring').evaluate(node=>getComputedStyle(node).animationName),'none');
      assert.equal(await icons.nth(index).getAttribute('title'),'Goal completed');
      assert.equal(await icons.nth(index).evaluate(node=>getComputedStyle(node).color),index===11?'rgb(133, 137, 141)':'rgb(21, 155, 39)');
      assert.equal(await icons.nth(index).isVisible(),true,'completed goals remain visible when the agent is offline');
    }
    assert.equal(await icons.nth(12).locator('.presence-spinner').count(),1);
    assert.equal(await icons.nth(13).isVisible(),false);
    const cooldownIcon=icons.nth(14);
    assert.equal(await cooldownIcon.isVisible(),true,'cooldown waits stay visible while the agent is offline');
    assert.equal(await cooldownIcon.getAttribute('aria-label'),'Waiting for browser cooldown');
    assert.equal(await cooldownIcon.evaluate(node=>getComputedStyle(node).color),'rgb(133, 137, 141)');
    assert.equal(await cooldownIcon.locator('path').getAttribute('d'),'M6 2h12v5l-6 5 6 5v5H6v-5l6-5-6-5ZM4 2h16M4 22h16');
    const spinSettings=node=>{
      const css=getComputedStyle(node);return [css.animationName,css.animationDuration,css.animationTimingFunction,css.animationIterationCount];
    };
    assert.deepEqual(await cooldownIcon.locator('svg').evaluate(spinSettings),['presence-spin','2s','linear','infinite']);
    assert.deepEqual(await cooldownIcon.locator('svg').evaluate(spinSettings),await icons.nth(0).locator('svg').evaluate(spinSettings),'the hourglass uses the same continuous rotation as the other spinners');
    assert.equal(await icons.nth(15).getAttribute('aria-label'),'Chat error: Browser action failed','actual errors take priority over the cooldown indicator');
    if(process.env.CODEXIFY_WORKSPACE_SCREENSHOTS){
      mkdirSync(process.env.CODEXIFY_WORKSPACE_SCREENSHOTS,{recursive:true});
      await page.locator(`.conversation-row[data-chat-id="${chats[14].id}"]`).screenshot({path:`${process.env.CODEXIFY_WORKSPACE_SCREENSHOTS}/${engineName.toLowerCase()}-browser-cooldown.png`});
    }
    assert.equal(await icons.nth(0).locator(".presence-spinner").count(),1);
    assert.equal(await icons.nth(0).locator("svg").evaluate(node => getComputedStyle(node).animationName),"presence-spin");
    assert.equal(await icons.nth(3).locator("path").getAttribute("d"),"M9 8v8m6-8v8");
    assert.equal(await icons.nth(3).locator("circle").getAttribute("r"),"9");
    assert.equal(await icons.nth(1).locator(".goal-outer-ring").getAttribute("d"),"M22.5 12a10.5 10.5 0 1 1-10.5-10.5");
    assert.equal(await icons.nth(1).locator("path:not(.goal-outer-ring)").getAttribute("d"),"M17.5 12a5.5 5.5 0 1 1-5.5-5.5M22.5 1.5 12 12m0-5v5h5");
    assert.equal(await icons.nth(1).evaluate(node=>getComputedStyle(node).color),await icons.nth(0).evaluate(node=>getComputedStyle(node).color));
    assert.equal(await icons.nth(1).locator("svg").evaluate(node=>getComputedStyle(node).strokeWidth),await icons.nth(7).locator("circle").evaluate(node=>getComputedStyle(node).strokeWidth));
    const goalSize=await icons.nth(1).locator(".goal-outer-ring").evaluate(node=>{const box=node.getBBox();return {width:box.width,height:box.height};});
    const spinnerSize=await icons.nth(0).locator("circle").evaluate(node=>{const box=node.getBBox();return {width:box.width,height:box.height};});
    assert.deepEqual(spinnerSize,goalSize,"the standalone spinner matches the goal ring diameter");
    assert.equal(await icons.nth(0).locator("svg").evaluate(node=>getComputedStyle(node).animationDuration),await icons.nth(1).locator(".goal-outer-ring").evaluate(node=>getComputedStyle(node).animationDuration));
    const offlineSize=await icons.nth(7).locator("circle").evaluate(node=>{const box=node.getBBox();return {width:box.width,height:box.height};});
    assert.deepEqual(goalSize,offlineSize);
    assert.equal(await icons.nth(5).locator("svg").innerHTML(),await icons.nth(1).locator("svg").innerHTML());
    assert.equal(await icons.nth(8).locator("circle").count(),3,"chat_await keeps its speech-bubble icon");
    assert.equal(await icons.nth(5).evaluate(node => getComputedStyle(node).color),await icons.nth(8).evaluate(node => getComputedStyle(node).color));
    assert.equal(await icons.nth(5).getAttribute("aria-label"),"Goal blocked");
    const ring=icons.nth(1).locator(".goal-outer-ring");
    assert.equal(await ring.evaluate(node=>getComputedStyle(node).animationName),"presence-spin");
    const initialRotation=await ring.evaluate(node=>getComputedStyle(node).transform);
    await page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve))));
    assert.notEqual(await ring.evaluate(node=>getComputedStyle(node).transform),initialRotation);
    const phaseErrors=await page.evaluate(async()=>{
      const sample=()=>{
        return [".conversation .presence.online .presence-spinner",".conversation .presence.browser-cooldown .presence-spinner"].map(selector=>{
          const matrix=new DOMMatrixReadOnly(getComputedStyle(document.querySelector(selector)).transform);
          return {time:performance.now(),angle:Math.atan2(matrix.b,matrix.a)};
        });
      };
      const before=sample();
      document.getElementById("view-recent").click();
      await new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));
      const after=sample();
      return after.map((sample,index)=>{
        const difference=sample.angle-before[index].angle-(sample.time-before[index].time)*2*Math.PI/2000;
        return Math.abs(Math.atan2(Math.sin(difference),Math.cos(difference)));
      });
    });
    assert(phaseErrors.every(error=>error<.2),"sidebar redraws must preserve the spinner and hourglass rotation phases");
    assert.equal(await icons.nth(1).locator("path:not(.goal-outer-ring)").evaluate(node=>getComputedStyle(node).transform),"none");
    assert.equal(await icons.nth(5).locator(".goal-outer-ring").evaluate(node=>getComputedStyle(node).animationName),"none");
    await page.emulateMedia({colorScheme:"dark"});
    assert.equal(await cooldownIcon.evaluate(node=>getComputedStyle(node).color),'rgb(133, 137, 141)','browser cooldowns stay gray in dark mode');
    assert.equal(await icons.nth(5).evaluate(node=>getComputedStyle(node).color),"rgb(255, 133, 142)");
    assert.equal(await icons.nth(11).evaluate(node=>getComputedStyle(node).color),await icons.nth(7).evaluate(node=>getComputedStyle(node).color),'completed offline goals match the offline gray in dark mode');
    await page.emulateMedia({colorScheme:"light"});
    const header = page.locator("#chat-host #presence");
    await page.locator('#chat-host #presence[data-state="online"]').waitFor();
    assert.equal(await header.locator(".presence-spinner").count(),0);
    await page.locator(".conversation").filter({ hasText:/Icon case 2/ }).click();
    await page.locator('#chat-host #presence[data-state="away"]').waitFor();
    await page.locator(".conversation").filter({ hasText:/Icon case 7/ }).click();
    await page.locator('#chat-host #presence[data-state="offline"]').waitFor();
    assert.equal(await page.locator('#chat-host #presence[data-state="offline"] svg').isVisible(),true,"the header offline icon stays visible");
    chats[14].goalStatus=null;
    await page.evaluate(()=>window.dispatchEvent(new Event('focus')));
    await page.locator(`.conversation-row[data-chat-id="${chats[14].id}"] .presence.browser-cooldown`).waitFor();
    assert.equal(await cooldownIcon.isVisible(),true,'ordinary chats also show the cooldown hourglass');
    chats[14].automationWaitingForBrowser=false;
    await page.evaluate(()=>window.dispatchEvent(new Event('focus')));
    await page.locator(`.conversation-row[data-chat-id="${chats[14].id}"] .presence.offline`).waitFor({state:'attached'});
    assert.equal(await cooldownIcon.isVisible(),false,'the cooldown indicator clears when the queued action resumes');
    chats[3].goalStatus = null;
    chats[4].goalStatus = null;
    await page.evaluate(() => window.dispatchEvent(new Event("focus")));
    await page.locator(".conversation-row:nth-of-type(4) .conversation .presence.online .presence-spinner").waitFor();
    await page.locator(".conversation-row:nth-of-type(5) .conversation .presence.offline").waitFor({state:"attached"});
    assert.equal(await page.locator(".conversation-row:nth-of-type(5) .conversation .presence.offline").isVisible(),false);
    const completed=page.locator(`.conversation-row[data-chat-id="${chats[10].id}"] .presence.goal-completed`);
    chats[10].lastAgentCallAtMs=Date.now()-240000;
    await page.evaluate(()=>window.dispatchEvent(new Event('focus')));
    await page.locator(`.conversation-row[data-chat-id="${chats[10].id}"] .presence[data-agent-state="away"]`).waitFor();
    assert.equal(await completed.evaluate(node=>getComputedStyle(node).color),'rgb(21, 155, 39)','completed goals stay green while the agent is away');
    chats[10].lastAgentCallAtMs=null;chats[10].agentPresenceUntilMs=Date.now()+2000;
    await page.evaluate(()=>window.dispatchEvent(new Event('focus')));
    await page.locator(`.conversation-row[data-chat-id="${chats[10].id}"] .presence[data-agent-state="online"]`).waitFor();
    assert.equal(await completed.evaluate(node=>getComputedStyle(node).color),'rgb(21, 155, 39)','goal completion alone does not gray an online agent');
    await page.locator(`.conversation-row[data-chat-id="${chats[10].id}"] .presence[data-agent-state="offline"]`).waitFor();
    assert.equal(await completed.evaluate(node=>getComputedStyle(node).color),'rgb(133, 137, 141)','the existing completed icon turns gray when presence expires');
    chats[10].agentWaitingUntilMs=Date.now()+60000;
    await page.evaluate(()=>window.dispatchEvent(new Event('focus')));
    await page.locator(`.conversation-row[data-chat-id="${chats[10].id}"] .presence[data-agent-state="waiting"]`).waitFor();
    assert.equal(await completed.evaluate(node=>getComputedStyle(node).color),'rgb(21, 155, 39)','an agent waiting for a reply keeps its completed icon green');
    if (process.env.CODEXIFY_WORKSPACE_SCREENSHOTS) {
      mkdirSync(process.env.CODEXIFY_WORKSPACE_SCREENSHOTS,{recursive:true});
      await page.locator(`.conversation-row[data-chat-id="${chats[10].id}"]`).screenshot({path:`${process.env.CODEXIFY_WORKSPACE_SCREENSHOTS}/${engineName.toLowerCase()}-completed-waiting.png`});
      await page.locator(`.conversation-row[data-chat-id="${chats[11].id}"]`).screenshot({path:`${process.env.CODEXIFY_WORKSPACE_SCREENSHOTS}/${engineName.toLowerCase()}-completed-offline.png`});
      await page.screenshot({path:`${process.env.CODEXIFY_WORKSPACE_SCREENSHOTS}/${engineName.toLowerCase()}-goal-sidebar.png`,fullPage:true});
    }
    assert.deepEqual(errors,[]);
  } finally { await browser.close(); }
});

test(`${engineName}: standalone owner view restores its URL selection`, { timeout:30000 }, async () => {
  const browser = await engine.launch();
  try {
    const page = await ownerTestPage(browser);
    page.setDefaultTimeout(5000);
    const errors = [];
    const copied = [];
    const markdownRequests = [];
    await page.exposeFunction("recordClipboard", text => copied.push(text));
    await page.addInitScript(() => {
      window.__writeTestClipboard = async text => {
        await window.recordClipboard(text);
        window.__lastCopied = text;
      };
      Object.defineProperty(navigator, "clipboard", {
        configurable:true,
        value:{ writeText:text => window.__writeTestClipboard(text) }
      });
    });
    page.on("pageerror", error => errors.push(error.message));
    const now = Date.now();
    const rawSource = "**Rendered**  \n\n```js\nconst x = 1;\n```";
    const wholeTranscript = `## Agent\n\nOlder server-only entry\n\n## Agent\n\n${rawSource}`;
    const chats = [
      { id:"a".repeat(64), title:"Fix tests", workspace:"project-a", lastEntryEnd:200, lastIncomingEntryEnd:200, lastEntryAtMs:now, lastAgentCallAtMs:now, totalToolCalls:3 },
      { id:"b".repeat(64), title:"Review files", workspace:"project-b", lastEntryEnd:100, lastIncomingEntryEnd:100, lastEntryAtMs:now - 60000, lastAgentCallAtMs:now - 240000, totalToolCalls:1 },
      { id:"c".repeat(64), title:"Old task", workspace:"project-c", lastEntryEnd:75, lastIncomingEntryEnd:75, lastEntryAtMs:now - 120000, lastAgentCallAtMs:null, totalToolCalls:0 }
    ];
    const messages = new Map(chats.map(chat => [chat.id, []]));
    messages.get(chats[0].id).push({ id:"raw-one", role:"agent", markdown:rawSource, start:1, end:100, created_at_ms:now, tool_call_count:3 });
    const seenRequests = [];
    let failPolling = false;
    await routeOwner(page, "http://127.0.0.1:43210/**", async route => {
      const request = route.request();
      const url = new URL(request.url());
      if (url.pathname === "/") return route.fulfill({ contentType:"text/html", body:ownerHtml, headers:{
        "content-security-policy":"default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src blob: data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
      } });
      seenRequests.push(request.headers().authorization);
      if (failPolling) return route.fulfill({ status:503, body:"Preview connection unavailable" });
      if (url.pathname === "/api/chats") return route.fulfill({ json:{ chats, serverTimeMs:Date.now() } });
      const id = url.pathname.split("/")[3];
      if (url.pathname.endsWith("/automation")) return route.fulfill({ json:{ enabled:false, serverTimeMs:Date.now() } });
      if (url.pathname.endsWith("/markdown")) {
        markdownRequests.push(url.pathname);
        return route.fulfill({ contentType:"text/markdown; charset=utf-8", body:wholeTranscript });
      }
      if (url.pathname.endsWith("/send")) {
        const body = request.postDataJSON();
        const end = chats.find(chat => chat.id === id).lastEntryEnd += 100;
        chats.find(chat => chat.id === id).agentWaitingUntilMs = null;
        const tool_call_count = chats.find(chat => chat.id === id).totalToolCalls;
        messages.get(id).push({ id:body.request_id, role:"user", markdown:body.message, start:end - 100, end, created_at_ms:Date.now(), tool_call_count });
        return route.fulfill({ json:{ _meta:{ [META]:{ sent:{ id:body.request_id, end, created_at_ms:Date.now(), tool_call_count } } } } });
      }
      if (url.pathname.startsWith("/api/chats/")) {
        const rows = messages.get(id);
        return route.fulfill({ json:{ _meta:{ [META]:{
          chat_file:`/private/${id}/CHAT.md`, revision:String(chats.find(chat => chat.id === id).lastEntryEnd),
          delivered_through:0, read_through:0, last_agent_call_at_ms:chats.find(chat => chat.id === id).lastAgentCallAtMs,
          agent_waiting_until_ms:chats.find(chat => chat.id === id).agentWaitingUntilMs ?? null,
          total_tool_calls:chats.find(chat => chat.id === id).totalToolCalls, server_time_ms:Date.now(),
          messages:rows, has_more:false, before:rows[0]?.start ?? null, unchanged:false
        } } } });
      }
      return route.fulfill({ status:404 });
    });
    await page.goto("http://127.0.0.1:43210/#test-token");
    await page.locator(".conversation").filter({ hasText:/Fix tests/ }).waitFor();
    assert.equal(new URL(page.url()).hash, "#test-token", "the access token must remain in the address bar");
    assert.deepEqual(await page.locator(".conversation-title").allTextContents(), ["Fix tests", "Review files", "Old task"]);
    assert.deepEqual(await page.locator(".presence").evaluateAll(nodes => nodes.map(node => [...node.classList].at(-1))), ["online", "away", "offline"]);
    assert.equal(await page.locator(".unread:not([hidden])").count(), 3);
    for (const size of await page.locator(".presence").evaluateAll(nodes => nodes.map(node => node.getBoundingClientRect().width))) assert(size <= 14);
    assert(!((await page.locator(".conversation-detail").allTextContents()).some(text => text.includes("No messages"))));
    await page.locator(".conversation").filter({ hasText:/Fix tests/ }).click();
    assert.equal(await page.locator("#automation-panel:visible").count(), 0);
    chats[0].agentWaitingUntilMs = Date.now() + 120000;
    await page.locator(".conversation-row:nth-of-type(1) .conversation .presence.waiting").waitFor();
    await page.locator('#chat-host #presence[data-state="waiting"]').waitFor();
    assert.equal(await page.locator(".conversation-row:nth-of-type(1) .conversation .presence circle").count(), 3);
    assert.equal(await page.locator(".conversation-row:nth-of-type(1) .conversation .presence path").getAttribute("fill"), "currentColor");
    assert.equal(new URL(page.url()).searchParams.get("chat"), chats[0].id);
    assert.equal(new URL(page.url()).hash, "#test-token");
    const chatHost = page.locator("#chat-host");
    await chatHost.getByRole("button", { name:"Copy message Markdown", exact:true }).first().click();
    await page.waitForFunction(expected => window.__lastCopied === expected, rawSource);
    assert.equal(copied.at(-1), rawSource);
    assert.equal(await chatHost.locator(".message-copy").first().getAttribute("data-copied"),"true");
    await chatHost.getByRole("button", { name:"Copy whole chat as Markdown", exact:true }).click();
    await page.waitForFunction(expected => window.__lastCopied === expected, wholeTranscript);
    assert.equal(markdownRequests.at(-1), `/api/chats/${chats[0].id}/markdown`);
    assert.equal(copied.at(-1), wholeTranscript);
    assert.equal(await chatHost.locator("#copy-chat").getAttribute("data-copied"),"true");
    await page.waitForTimeout(3100);
    assert.equal(await chatHost.locator(".message-copy").first().getAttribute("data-copied"),null);
    assert.equal(await chatHost.locator("#copy-chat").getAttribute("data-copied"),null);
    await page.evaluate(() => {
      navigator.clipboard.writeText = async () => { throw new Error("Clipboard denied"); };
    });
    await chatHost.getByRole("button", { name:"Copy message Markdown", exact:true }).first().click();
    await chatHost.getByText("Could not copy message Markdown.", { exact:true }).waitFor();
    assert.equal(await chatHost.locator(".message-copy").first().getAttribute("data-copied"),null);
    await page.evaluate(() => {
      navigator.clipboard.writeText = text => window.__writeTestClipboard(text);
    });
    const chat = page.locator("#chat-host .chat-shell #draft");
    await chat.fill("Please run the test suite");
    await page.locator("#chat-host #send:enabled").waitFor();
    await chat.press("Enter");
    await page.locator("#chat-host").getByText("Please run the test suite").waitFor();
    await page.locator('#chat-host #presence[data-state="online"]').waitFor();
    await page.locator(".conversation-row:nth-of-type(1) .conversation .presence.online").waitFor();
    if (process.env.CODEXIFY_WORKSPACE_SCREENSHOTS) {
      mkdirSync(process.env.CODEXIFY_WORKSPACE_SCREENSHOTS, { recursive:true });
      await page.screenshot({ path:`${process.env.CODEXIFY_WORKSPACE_SCREENSHOTS}/${engineName.toLowerCase()}-sidebar.png`, fullPage:true });
    }
    await page.locator(".conversation").filter({ hasText:/Review files/ }).click();
    assert.equal(new URL(page.url()).searchParams.get("chat"), chats[1].id);
    await page.locator("#chat-host").getByText("Please run the test suite").waitFor({ state:"hidden" });
    await page.goBack();
    await page.locator("#chat-host").getByText("Please run the test suite").waitFor();
    assert.equal(new URL(page.url()).searchParams.get("chat"), chats[0].id);
    await page.goBack();
    await page.locator("#placeholder").waitFor();
    assert.equal(new URL(page.url()).searchParams.get("chat"), null);
    await page.goForward();
    await page.locator("#chat-host").getByText("Please run the test suite").waitFor();
    await page.goForward();
    await page.locator("#chat-host").getByText("Please run the test suite").waitFor({ state:"hidden" });
    await page.reload();
    await page.locator('.conversation[aria-current="true"] .conversation-title').getByText("Review files").waitFor();
    await page.locator("#chat-host #draft").waitFor();
    assert.equal(new URL(page.url()).searchParams.get("chat"), chats[1].id);
    assert.equal(new URL(page.url()).hash, "#test-token");
    await page.evaluate(() => sessionStorage.clear());
    await page.goto(`http://127.0.0.1:43210/?chat=${chats[0].id}#test-token`);
    await page.locator("#chat-host").getByText("Please run the test suite").waitFor();
    assert.equal(new URL(page.url()).searchParams.get("chat"), chats[0].id);
    assert.equal(new URL(page.url()).hash, "#test-token");
    assert(seenRequests.length > 0 && seenRequests.every(value => value === "Bearer test-token"));
    const clock = Date.now();
    await page.clock.install({ time:clock });
    chats[1].agentWaitingUntilMs = clock + 20000;
    await page.evaluate(() => window.dispatchEvent(new Event("focus")));
    await page.locator(".conversation-row:nth-of-type(2) .conversation .presence.waiting").waitFor();
    failPolling = true;
    await page.clock.fastForward(20000);
    await page.locator(".conversation-row:nth-of-type(2) .conversation .presence.away").waitFor();
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

test(`${engineName}: project grouping, transcript search, and persistent names`, { timeout:30000 }, async () => {
  const browser = await engine.launch();
  try {
    const page = await ownerTestPage(browser);
    page.setDefaultTimeout(5000);
    const errors = [];
    page.on("pageerror", error => errors.push(error.message));
    const now = Date.now();
    const chats = [
      { id:"1".repeat(64), title:"Newest alpha", workspace:"feature-a", projectId:"alpha-worktree-a", projectPath:"/worktrees/feature-a", projectGroup:{path:"/projects/alpha/same-name",name:"same-name"}, lastEntryAtMs:now },
      { id:"2".repeat(64), title:"Middle beta", workspace:"same-name", projectId:"beta", projectPath:"/projects/beta/same-name", projectGroup:{path:"/projects/beta/same-name",name:"same-name"}, lastEntryAtMs:now - 1000 },
      { id:"3".repeat(64), title:"Older alpha", workspace:"feature-b", projectId:"alpha-worktree-b", projectPath:"/worktrees/feature-b", projectGroup:{path:"/projects/alpha/same-name",name:"same-name"}, lastEntryAtMs:now - 2000 },
      { id:"4".repeat(64), title:"Oldest beta", workspace:"beta-worktree", projectId:"beta-worktree", projectPath:"/worktrees/beta", projectGroup:{path:"/projects/beta/same-name",name:"same-name"}, lastEntryAtMs:now - 3000 }
    ].map(chat => ({ ...chat, customTitle:null, latestTitle:chat.title, lastEntryEnd:100, lastAgentCallAtMs:null, totalToolCalls:0 }));
    let releaseOldSearch = null;
    await routeOwner(page, "http://127.0.0.1:43212/**", async route => {
      const request = route.request(), url = new URL(request.url());
      if (url.pathname === "/") return route.fulfill({ contentType:"text/html", body:ownerHtml });
      assert.equal(request.headers().authorization, "Bearer test-token");
      if (url.pathname === "/api/chats") {
        const query = url.searchParams.get("q") || "";
        const matchedChatIds = query === "buried text" ? [chats[1].id] : query === "delayed" ? [chats[3].id] : chats.filter(chat => chat.title.toLowerCase().includes(query.toLowerCase())).map(chat => chat.id);
        if (query === "delayed") await new Promise(resolve => { releaseOldSearch = resolve; });
        return route.fulfill({ json:{ chats, matchedChatIds, serverTimeMs:Date.now() } });
      }
      const chat = chats.find(chat => chat.id === url.pathname.split("/")[3]);
      if (url.pathname.endsWith("/title")) {
        chat.customTitle = request.postDataJSON().title.trim() || null;
        chat.title = chat.customTitle || chat.latestTitle;
        return route.fulfill({ json:{ title:chat.title, customTitle:chat.customTitle } });
      }
      if (url.pathname.endsWith("/automation")) return route.fulfill({ json:{ enabled:false } });
      return route.fulfill({ json:{ _meta:{ [META]:{
        chat_file:`/private/${chat.id}/CHAT.md`, revision:"100", delivered_through:0, read_through:0,
        last_agent_call_at_ms:null, agent_waiting_until_ms:null, total_tool_calls:0,
        server_time_ms:Date.now(), messages:[], has_more:false, before:null, unchanged:false
      } } } });
    });
    await page.goto(`http://127.0.0.1:43212/?chat=${chats[0].id}#test-token`);
    await page.locator(".project-heading").first().waitFor();
    assert.deepEqual(await page.locator(".conversation-title").allTextContents(), ["Newest alpha", "Older alpha", "Middle beta", "Oldest beta"]);
    assert.deepEqual(await page.locator(".project-heading").evaluateAll(nodes => nodes.map(node => node.title)), ["/projects/alpha/same-name", "/projects/beta/same-name"]);
    assert.deepEqual(await page.locator(".project-heading").allTextContents(), ["same-name", "same-name"], "worktrees share their base project heading; unrelated same-name repositories stay separate");
    await page.getByRole("button", { name:"Recent", exact:true }).click();
    assert.deepEqual(await page.locator(".conversation-title").allTextContents(), ["Newest alpha", "Middle beta", "Older alpha", "Oldest beta"]);
    await page.reload();
    await page.locator(".conversation-title").first().waitFor();
    assert.equal(await page.locator(".project-heading").count(), 0);
    await page.getByRole("button", { name:"Projects", exact:true }).click();

    const search = page.getByRole("searchbox", { name:"Search chats" });
    await search.fill("buried text");
    await page.locator(".conversation").filter({ hasText:/Middle beta/ }).waitFor();
    assert.deepEqual(await page.locator(".conversation-title").allTextContents(), ["Middle beta"]);
    assert.equal(await page.locator("#chat-host #draft").count(), 1, "filtering must keep the selected conversation open");
    assert.equal(new URL(page.url()).searchParams.get("chat"), chats[0].id);
    await search.fill("delayed");
    for (let attempts = 0; !releaseOldSearch && attempts < 50; attempts++) await page.waitForTimeout(20);
    assert(releaseOldSearch);
    await search.fill("Newest alpha");
    await page.locator(".conversation").filter({ hasText:/Newest alpha/ }).waitFor();
    releaseOldSearch();
    await page.waitForTimeout(100);
    assert.deepEqual(await page.locator(".conversation-title").allTextContents(), ["Newest alpha"]);
    await search.fill("no matching content");
    await page.getByText("No matching chats.", { exact:true }).waitFor();
    await search.fill("");
    await page.locator(".conversation").filter({ hasText:/Newest alpha/ }).waitFor();

    const panel = page.locator("#chat-host");
    const assertHeaderMatchesSidebar = async () => {
      const row = page.locator('.conversation[aria-current="true"]');
      assert.equal(await panel.locator("#chat-title").textContent(), await row.locator(".conversation-title").textContent());
      assert.equal(await panel.locator("#chat-subtitle > span").first().textContent(), await row.locator(".conversation-detail").textContent());
      assert.equal(await panel.locator("#chat-subtitle .project-path").textContent(), chats[0].projectPath);
    };
    await assertHeaderMatchesSidebar();
    await panel.getByRole("button", { name:"Rename chat" }).click();
    await panel.getByLabel("Chat name").fill("Saved release name");
    await panel.getByRole("button", { name:"Save name" }).click();
    await page.locator(".conversation").filter({ hasText:/Saved release name/ }).waitFor();
    await assertHeaderMatchesSidebar();
    chats[0].latestTitle = "Latest agent answer";
    await page.reload();
    await page.locator(".conversation").filter({ hasText:/Saved release name/ }).waitFor();
    await panel.getByRole("button", { name:"Rename chat" }).click();
    assert.equal(await panel.getByLabel("Chat name").inputValue(), "Saved release name");
    await panel.getByLabel("Chat name").fill("");
    await panel.getByRole("button", { name:"Save name" }).click();
    await page.locator(".conversation").filter({ hasText:/Latest agent answer/ }).waitFor();
    await assertHeaderMatchesSidebar();
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

test(`${engineName}: model selectors retain per-chat preferences, cached labels and confirmed fallbacks`, {timeout:30000}, async()=>{
 const browser=await engine.launch();
 try {
  const page=await ownerTestPage(browser);page.setDefaultTimeout(5000);
  const now=Date.now(),chats=['b','c'].map((n,i)=>({id:n.repeat(64),title:`Model preference ${i}`,workspace:'test',lastEntryEnd:0,lastEntryAtMs:now-i,totalToolCalls:0}));
  const selections=new Map(chats.map((chat,i)=>[chat.id,{model:'gpt-5.6-sol',effort:i?'medium':'max'}]));
  const model={id:'gpt-5.6-sol',label:'GPT-5.6 Sol',efforts:[{id:'medium',label:'Medium',available:true},{id:'high',label:'High',available:true},{id:'max',label:'Pro',available:true}]};
  const catalogModels=[model,{id:'gpt-6-astra',label:'GPT-6 Astra',efforts:[{id:'max',label:'Pro',available:true}]}];
  let catalogReady=false,authenticated=true;const operations=[],errors=[];
  page.on('pageerror',error=>errors.push(error.message));
  await routeOwner(page,'http://127.0.0.1:43224/**',async route=>{
   const request=route.request(),url=new URL(request.url()),id=url.pathname.split('/')[3];
   if(url.pathname==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(url.pathname==='/api/chats')return route.fulfill({json:{chats,serverTimeMs:Date.now()}});
   if(url.pathname.endsWith('/automation/selection')){const body=request.postDataJSON();operations.push(body);selections.set(id,{model:body.model,effort:body.effort});return route.fulfill({json:{selection:selections.get(id)}});}
   if(url.pathname.endsWith('/automation'))return route.fulfill({json:{enabled:true,pairing:{status:'verified'},selection:selections.get(id),goal:null,action:{status:'idle'},companion:{authenticated,status:authenticated?'idle':'unavailable',catalogContext:authenticated?'account-one':null},catalog:catalogReady&&authenticated?{models:catalogModels}:null,serverTimeMs:Date.now()}});
   assert(!url.pathname.endsWith('/capabilities'),'opening a chat must not discover models');
   return route.fulfill({json:{_meta:{[META]:{chat_file:`/test/${id}/CHAT.md`,revision:'0',delivered_through:0,read_through:0,total_tool_calls:0,server_time_ms:Date.now(),messages:[],has_more:false,unchanged:false}}}});
  });
  await page.goto(`http://127.0.0.1:43224/?chat=${chats[0].id}#test-token`);
  const host=page.locator('#chat-host'),picker=host.locator('#automation-model'),effort=host.locator('#automation-effort');
  await picker.waitFor();assert.equal(await picker.locator('option:checked').textContent(),'GPT-5.6 Sol');
  assert.equal(await effort.locator('option:checked').textContent(),'Pro');
  catalogReady=true;await host.locator('#automation-effort:enabled').waitFor();await effort.selectOption('high');await host.locator('#automation-effort:enabled').waitFor();
  await page.locator(`[data-chat-id="${chats[1].id}"] .conversation`).click();await page.waitForFunction(()=>document.querySelector('#chat-host .chat-shell').shadowRoot.querySelector('#automation-effort')?.value==='medium');
  await page.locator(`[data-chat-id="${chats[0].id}"] .conversation`).click();await page.waitForFunction(()=>document.querySelector('#chat-host .chat-shell').shadowRoot.querySelector('#automation-effort')?.value==='high');
  model.efforts.find(effort=>effort.id==='high').available=false;
  await effort.locator('option[value="high"]:disabled').waitFor({state:'attached'});assert.equal(await effort.inputValue(),'high','availability changes do not silently select another effort');
  authenticated=false;await host.locator('#automation-effort:disabled').waitFor();assert.equal(await effort.locator('option:checked').textContent(),'High');
  await page.reload();await picker.waitFor();assert.equal(await picker.locator('option:checked').textContent(),'GPT-5.6 Sol');assert.equal(await effort.locator('option:checked').textContent(),'High');
  authenticated=true;selections.set(chats[0].id,{model:'gpt-6-astra',effort:'max'});
  await page.waitForFunction(()=>document.querySelector('#chat-host .chat-shell').shadowRoot.querySelector('#automation-model').value==='gpt-6-astra');
  selections.set(chats[0].id,{model:'gpt-5.6-sol',effort:'max'});
  await page.waitForFunction(()=>{const root=document.querySelector('#chat-host .chat-shell').shadowRoot;return root.querySelector('#automation-model').value==='gpt-5.6-sol'&&root.querySelector('#automation-effort').value==='max';});
  assert.equal(await picker.locator('option:checked').textContent(),'GPT-5.6 Sol');assert.equal(await effort.locator('option:checked').textContent(),'Pro');
  selections.set(chats[0].id,{model:'gpt-5.6-sol',effort:'medium'});
  await page.waitForFunction(()=>document.querySelector('#chat-host .chat-shell').shadowRoot.querySelector('#automation-effort').value==='medium');
  assert.equal(await effort.locator('option:checked').textContent(),'Medium','an effort-only fallback also updates the picker');
  assert.equal(operations.length,1,'only the explicit user change saves a selection');assert.deepEqual(errors,[]);
 }finally{await browser.close();}
});

test(`${engineName}: standalone automation controls drive only authenticated owner APIs`, { timeout:45000 }, async () => {
  const browser = await engine.launch();
  try {
    const page = await ownerTestPage(browser,{ viewport:{ width:1280, height:900 } });
    page.setDefaultTimeout(5000);
    const errors = [];
    const operations = [];
    let holdNextState = false, releaseHeldState, heldStateStarted;
    page.on("pageerror", error => errors.push(error.message));
    const now = Date.now();
    const chat = {
      id:"d".repeat(64), title:"Automate task", workspace:"project-d",
      lastEntryEnd:100, lastEntryAtMs:now, lastAgentCallAtMs:now - 360000,
      agentWaitingUntilMs:null, totalToolCalls:4
    };
    const messages = [];
    const catalog = {
      catalogRevision:"catalog-live",
      models:[
        { id:"gpt-6-astra", label:"GPT-6 Astra", efforts:[{ id:"max", label:"Pro", available:true, reason:null }] },
        { id:"gpt-6-sol", label:"GPT-6 Sol", efforts:[
          { id:"low", label:"Instant", available:true, reason:null },
          { id:"medium", label:"Medium", available:true, reason:null },
          { id:"high", label:"High", available:true, reason:null },
          { id:"max", label:"Max", available:false, reason:"Not available for this account" }
        ] },
        { id:"gpt-5.6-sol", label:"GPT-5.6 Sol", efforts:[
          { id:"medium", label:"Medium", available:true, reason:null }
        ] }
      ]
    };
    const automation = {
      enabled:true,
      ownerGeneration:0,
      pairing:{ status:"verified", candidateUrl:null, canonicalUrl:"https://chatgpt.com/c/already-paired", generation:1, verifiedAtMs:now, expiresAtMs:null },
      selection:{ model:"gpt-6-astra", effort:"max", catalogRevision:"saved-catalog" },
      goal:null,
      action:{ id:null, kind:null, status:"idle", pairingGeneration:null, goalRevision:null, lastError:null, lastStableAnswer:null, updatedAtMs:null },
      live:{ actionId:"action-live", phase:"generating", status:"Inspecting the workspace", commentary:"Visible commentary", answer:"Partial browser answer", updatedAtMs:now },
      companion:{ status:"idle", authenticated:true, activeActionId:null, detail:null },
      presence:{ lastAgentCallAtMs:chat.lastAgentCallAtMs, agentWaitingUntilMs:null, totalToolCalls:chat.totalToolCalls },
      queue:{ position:2, total:3 },
      serverTimeMs:now
    };
    const meta = payload => ({ _meta:{ [META]:payload } });
    await routeOwner(page, "http://127.0.0.1:43211/**", async route => {
      const request = route.request();
      const url = new URL(request.url());
      if (url.pathname === "/") return route.fulfill({ contentType:"text/html", body:ownerHtml, headers:{
        "content-security-policy":"default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src blob: data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
      } });
      assert.equal(request.headers().authorization, "Bearer test-token");
      if (url.pathname === "/api/chats") return route.fulfill({ json:{ chats:[chat], serverTimeMs:Date.now() } });
      if (url.pathname === `/api/chats/${chat.id}`) return route.fulfill({ json:meta({
        chat_file:`/private/${chat.id}/CHAT.md`, revision:String(chat.lastEntryEnd),
        delivered_through:0, read_through:0, last_agent_call_at_ms:chat.lastAgentCallAtMs,
        agent_waiting_until_ms:null, total_tool_calls:chat.totalToolCalls, server_time_ms:Date.now(),
        messages, has_more:false, before:null, unchanged:false
      }) });
      if (url.pathname === `/api/chats/${chat.id}/automation` && request.method() === "GET") {
        const snapshot = structuredClone(automation);
        snapshot.catalog = automation.companion.authenticated ? catalog : null;
        if (holdNextState) {
          holdNextState = false;
          snapshot.live.status = "Stale selection poll received";
          const released = new Promise(resolve => { releaseHeldState = resolve; });
          heldStateStarted(); await released;
        }
        return route.fulfill({ json:snapshot });
      }
      if (url.pathname === `/api/chats/${chat.id}/automation/capabilities`) {
        operations.push("capabilities");
        return route.fulfill({ json:{companion:automation.companion,...catalog} });
      }
      if (url.pathname === `/api/chats/${chat.id}/automation/selection`) {
        const body = request.postDataJSON(); operations.push(["selection", body]);
        automation.selection = { model:body.model, effort:body.effort, catalogRevision:"catalog-live" };
        return route.fulfill({ json:{ selection:automation.selection } });
      }
      if (url.pathname === `/api/chats/${chat.id}/automation/pair`) {
        const body = request.postDataJSON(); operations.push(["pair", body]);
        automation.pairing = { status:"unverified", candidateUrl:body.url, canonicalUrl:null, generation:1, verifiedAtMs:null, expiresAtMs:now + 60000 };
        automation.action = { id:"pair-action", kind:"pair_verify", status:"queued", pairingGeneration:1, goalRevision:null, lastError:null, lastStableAnswer:null, updatedAtMs:Date.now() };
        return route.fulfill({ json:{ pairing:automation.pairing, action:automation.action } });
      }
      if (url.pathname === `/api/chats/${chat.id}/automation/unlink`) {
        operations.push(["unlink", request.postDataJSON()]);
        automation.pairing = { status:"unpaired", candidateUrl:null, canonicalUrl:null, generation:2, verifiedAtMs:null, expiresAtMs:null };
        automation.action = { id:null, kind:null, status:"idle", pairingGeneration:null, goalRevision:null, lastError:null, lastStableAnswer:null, updatedAtMs:Date.now() };
        return route.fulfill({ json:{ pairing:automation.pairing } });
      }
      if (url.pathname === `/api/chats/${chat.id}/automation/login`) {
        operations.push("login"); automation.companion.authenticated = true;
        return route.fulfill({ json:{ companion:automation.companion } });
      }
      if (url.pathname === `/api/chats/${chat.id}/automation/reveal`) {
        operations.push("reveal"); return route.fulfill({ json:{ opened:true } });
      }
      if (url.pathname === `/api/chats/${chat.id}/automation/action/cancel`) {
        const body = request.postDataJSON(); operations.push(["cancel", body]);
        if (automation.action.kind === "pair_verify") {
          automation.pairing = { status:"unpaired", candidateUrl:null, canonicalUrl:null, generation:automation.pairing.generation + 1, verifiedAtMs:null, expiresAtMs:null };
          automation.action = { id:null, kind:null, status:"unpaired", pairingGeneration:automation.pairing.generation, goalRevision:null, lastError:null, lastStableAnswer:null, updatedAtMs:Date.now() };
        } else {
          automation.action = { ...automation.action, status:"failed", lastError:"Browser action cancelled before Send", updatedAtMs:Date.now() };
        }
        automation.queue = null;
        return route.fulfill({ json:{ action:automation.action, queue:null } });
      }
      if (url.pathname === `/api/chats/${chat.id}/automation/action/retry`) {
        const body = request.postDataJSON(); operations.push(["retry", body]);
        automation.action = { ...automation.action, id:"retried-action", status:"queued", lastError:null, updatedAtMs:Date.now() };
        automation.queue = { position:1, total:1 };
        return route.fulfill({ json:{ action:automation.action, queue:automation.queue } });
      }
      if (url.pathname === `/api/chats/${chat.id}/send`) {
        const body = request.postDataJSON(); operations.push(["send", body.message]);
        if (body.message.startsWith("/goal set ")) automation.goal = {
          revision:(automation.goal?.revision || 0) + 1, markdown:body.message.slice(10), status:"active", createdAtMs:now,
          updatedAtMs:Date.now(), finishedAtMs:null, summary:null, blockedReason:null
        };
        else if (body.message === "/goal run") automation.goal.status = "active";
        else if (body.message === "/goal stop") automation.goal.status = "configured";
        else if (body.message === "/goal clear") automation.goal = null;
        return route.fulfill({ json:meta({ localCommand:{}, goal:automation.goal, goalCommand:body.message.split(" ")[1], replayed:false }) });
      }
      return route.fulfill({ status:404, body:"Not found" });
    });

    await page.goto(`http://127.0.0.1:43211/?chat=${chat.id}#test-token`);
    const panel = page.locator("#chat-host");
    await panel.locator("#composer-tools").waitFor();
    assert.equal(await panel.locator("#automation-settings").isVisible(), false);
    const model = panel.getByLabel("ChatGPT model");
    const effort = panel.getByLabel("Reasoning effort");
    await panel.locator("#automation-model:enabled").waitFor();
    assert.equal(await model.locator("option:checked").textContent(), "GPT-6 Astra");
    assert.equal(await effort.locator("option:checked").textContent(), "Pro");
    assert.equal(await effort.inputValue(), "max");
    holdNextState = true;
    await new Promise(resolve => { heldStateStarted = resolve; });
    await model.selectOption("gpt-5.6-sol");
    await panel.locator("#automation-model:enabled").waitFor();
    assert.deepEqual(await effort.locator("option").allTextContents(), ["Medium"]);
    await panel.getByRole("button", {name:"Connection settings",exact:true}).click();
    releaseHeldState();
    await panel.getByText("Stale selection poll received", {exact:true}).waitFor();
    assert.equal(await model.inputValue(), "gpt-5.6-sol", "an old poll must not overwrite a successful save");
    assert.equal(await effort.inputValue(), "medium");
    await panel.getByRole("button", {name:"Close connection settings",exact:true}).click();
    await model.selectOption("gpt-6-sol");
    await panel.locator("#automation-model:enabled").waitFor();
    assert.deepEqual(await effort.locator("option").allTextContents(), ["High", "Medium", "Instant"]);
    assert.equal(await effort.inputValue(), "high", "model selection defaults to its highest available effort");
    await effort.selectOption("medium");
    await panel.locator("#automation-effort:enabled").waitFor();
    await page.waitForTimeout(1600);
    assert.equal(await effort.inputValue(), "medium", "background refresh preserves the user's explicit effort choice");
    assert.equal(await effort.locator('option[value="max"]').count(), 0);
    assert.equal(await model.locator('option:has-text("Use conversation model")').count(),0);
    assert.equal(await panel.locator("#composer #automation-model").count(), 1);
    assert.equal(await panel.locator("#composer #automation-effort").count(), 1);
    for (const picker of [model, effort]) {
      const measured = await picker.evaluate(node => {
        const style = getComputedStyle(node), label = document.createElement("span");
        label.style.cssText = "position:absolute;white-space:pre;visibility:hidden";
        for (const property of ["fontFamily", "fontSize", "fontWeight", "fontStyle", "fontStretch", "fontVariant", "letterSpacing"]) label.style[property] = style[property];
        label.textContent = node.selectedOptions[0].textContent; node.parentElement.append(label);
        const text = label.getBoundingClientRect().width; label.remove();
        return { text, width:node.getBoundingClientRect().width, padding:parseFloat(style.paddingLeft) + parseFloat(style.paddingRight), appearance:style.appearance };
      });
      assert.equal(measured.appearance, "none");
      assert(measured.width - measured.padding >= measured.text, "the selected label must fit before the arrow");
      assert(measured.width - measured.padding - measured.text < 3, "the arrow must remain close to the label");
    }

    automation.pairing = { status:"unpaired", candidateUrl:null, canonicalUrl:null, generation:0 };
    await panel.locator("#automation-pair-chat").waitFor();
    await panel.getByRole("button", { name:"Connection settings", exact:true }).click();
    await panel.getByText("Offline", { exact:true }).waitFor();
    await panel.getByText(/Queue 2 of 3/).waitFor();
    await panel.getByText("Inspecting the workspace", { exact:true }).waitFor();
    await panel.getByText("Partial browser answer", { exact:true }).waitFor();
    assert.equal(await panel.getByRole("button", { name:"Open in ChatGPT" }).isDisabled(), true);
    assert.equal(await panel.getByRole("button", { name:"Refresh models" }).count(),0,"shared browser controls live in global settings");
    assert.equal(await panel.getByRole("button", { name:"Start browser" }).count(),0);
    assert.equal(await panel.getByRole("button", { name:"Stop browser" }).count(),0);
    assert.equal(await panel.getByRole("button", { name:"Sign in" }).count(),0);
    await panel.getByLabel("ChatGPT conversation URL").fill("https://chatgpt.com/c/owner-automation");
    await panel.getByRole("button", { name:"Pair conversation" }).click();
    await panel.getByText("Pairing verification queued", { exact:true }).waitFor();
    await panel.getByRole("button", { name:"Cancel queued action" }).click();
    await panel.getByText("Not paired", { exact:true }).waitFor();
    await panel.getByRole("button", { name:"Pair conversation" }).click();
    await panel.getByText("Pairing verification queued", { exact:true }).waitFor();
    automation.companion.activeActionId = "busy-action";
    await panel.getByRole("button", { name:"Open in ChatGPT" }).click();
    automation.companion.activeActionId = null;
    await panel.getByRole("button", { name:"Close connection settings" }).click();

    automation.pairing = { ...automation.pairing, status:"verified", canonicalUrl:automation.pairing.candidateUrl, verifiedAtMs:Date.now() };
    await panel.locator("#automation-pair-chat").waitFor({state:"hidden"});
    await panel.getByRole("button", { name:"Add goal", exact:true }).click();
    await panel.getByLabel("Goal Markdown").fill("Finish the standalone controls.");
    await panel.getByRole("button", { name:"Set goal", exact:true }).click();
    await panel.locator("#goal-bar").getByText("Finish the standalone controls.", { exact:true }).waitFor();
    await panel.getByText("Pursuing goal", { exact:true }).waitFor();
    assert.equal(await panel.locator("#goal-icon svg").count(),1);
    assert.equal(await panel.locator("#goal-icon svg").evaluate(node => getComputedStyle(node).strokeWidth),await panel.locator("#automation-toggle-goal svg").evaluate(node => getComputedStyle(node).strokeWidth));
    assert.equal(await panel.locator("#goal-icon").evaluate(node => getComputedStyle(node).color),"rgb(21, 155, 39)");
    assert.equal(await panel.locator("#goal-icon .goal-outer-ring").evaluate(node=>getComputedStyle(node).animationName),"stop-spin");
    assert.equal(await panel.locator("#goal-icon path:not(.goal-outer-ring)").evaluate(node=>getComputedStyle(node).transform),"none");
    automation.goal.status="blocked";
    await panel.getByText("Goal blocked",{exact:true}).waitFor();
    assert.equal(await panel.locator("#goal-icon").evaluate(node=>getComputedStyle(node).color),"rgb(196, 49, 59)");
    assert.equal(await panel.locator("#goal-icon .goal-outer-ring").evaluate(node=>getComputedStyle(node).animationName),"none");
    await page.emulateMedia({colorScheme:"dark"});
    assert.equal(await panel.locator("#goal-icon").evaluate(node=>getComputedStyle(node).color),"rgb(255, 133, 142)");
    await page.emulateMedia({colorScheme:"light"});
    await panel.getByRole("button",{name:"Run goal",exact:true}).click();
    await panel.getByText("Pursuing goal",{exact:true}).waitFor();
    assert.equal(await panel.locator("#goal-icon").evaluate(node=>getComputedStyle(node).color),"rgb(21, 155, 39)");
    automation.presence.lastAgentCallAtMs = Date.now();
    await panel.getByRole("button", { name:"Edit goal", exact:true }).click();
    const updatedGoal = "Finish the updated standalone controls while the agent keeps working.";
    await panel.getByLabel("Goal Markdown").fill(updatedGoal);
    await panel.getByRole("button", { name:"Set goal", exact:true }).click();
    await panel.locator("#goal-bar").getByText(updatedGoal, {exact:true}).waitFor();
    assert.equal(automation.goal.status,"active");
    assert.equal(automation.goal.revision,2);
    assert(operations.some(operation => Array.isArray(operation) && operation[0] === "send" && operation[1] === `/goal set ${updatedGoal}`));
    automation.presence.lastAgentCallAtMs = null;
    const goalBox = await panel.locator("#goal-bar").boundingBox();
    const composerBox = await panel.locator("#composer").boundingBox();
    assert(goalBox && composerBox && goalBox.y < composerBox.y && goalBox.y + goalBox.height <= composerBox.y + 12);
    if (process.env.CODEXIFY_WORKSPACE_SCREENSHOTS) {
      mkdirSync(process.env.CODEXIFY_WORKSPACE_SCREENSHOTS, { recursive:true });
      await page.screenshot({ path:`${process.env.CODEXIFY_WORKSPACE_SCREENSHOTS}/${engineName.toLowerCase()}-composer.png`, fullPage:true });
    }
    automation.action = {id:"automatic-stop-retry",kind:"offline_goal",status:"failed",goalRevision:2,lastError:"ChatGPT did not stop the previous response; the new message was not sent",retryMode:"retry",sendAccepted:false,automaticRetryAtMs:Date.now()+30000};
    await panel.getByRole("button",{name:"Connection settings",exact:true}).click();
    await panel.locator("#automation-action").getByText("Retrying automatically",{exact:true}).waitFor();
    assert.equal(await panel.locator("#goal-retry").isVisible(),false);
    assert.equal(await panel.locator("#automation-error").isVisible(),true);
    assert((await panel.locator("#automation-error").textContent()).includes("Retrying the goal automatically in"));
    assert.equal(await panel.locator("#goal-caption").textContent(),"Pursuing goal");
    await panel.getByRole("button",{name:"Close connection settings",exact:true}).click();
    automation.action={id:'authentication-goal-backoff',kind:'offline_goal',status:'authentication_required',goalRevision:2,lastError:'Sign in to ChatGPT, then retry this action.',sendAccepted:false,retryMode:'retry',automaticRetryAtMs:Date.now()+180000,automaticRetryAttempts:0,automaticRetryLimit:6};
    await panel.locator('#automation-error').getByText(/Retrying the goal automatically/).waitFor();
    assert.equal(await panel.locator('#goal-caption').textContent(),'Pursuing goal');
    automation.action={...automation.action,automaticRetryAtMs:null,automaticRetryAttempts:6};
    await panel.locator('#automation-error').getByText(/All 6 automatic retries failed/).waitFor();
    await panel.locator('#automation-retry-inline').getByText('Retry',{exact:true}).waitFor();
    automation.action={...automation.action,id:'conversation-goal-backoff',status:'conversation_unavailable',lastError:'ChatGPT conversation temporarily not accessible',automaticRetryAtMs:Date.now()+300000,automaticRetryAttempts:1};
    await panel.locator('#automation-error').getByText(/Retrying the goal automatically/).waitFor();
    assert.equal(await panel.locator('#goal-caption').textContent(),'Pursuing goal');
    assert(!(await panel.locator('#automation-error').textContent()).includes('Sign in'));
    automation.action={...automation.action,automaticRetryAtMs:null,automaticRetryAttempts:6};
    await panel.locator('#automation-error').getByText(/All 6 automatic retries failed/).waitFor();
    assert(!(await panel.locator('#automation-error').textContent()).includes('Sign in'));
    automation.action = { id:"pre-send-attention", kind:"offline_combined", status:"interference", goalRevision:1, lastError:"Clear the draft before retrying", retryMode:"retry", sendAccepted:false };
    await panel.locator("#automation-retry-inline").getByText("Retry", {exact:true}).waitFor();
    assert((await panel.locator("#automation-error").textContent()).includes("The message was not sent"));
    await panel.locator("#automation-retry-inline").click();
    await panel.locator("#automation-retry-inline").waitFor({state:"hidden"});
    automation.action = { id:"goal-attention", kind:"offline_goal", status:"ambiguous", goalRevision:1, lastError:"The previous turn could not be confirmed", retryMode:"continue_goal", sendAccepted:true, automaticRetryAttempts:3, automaticRetryLimit:3 };
    await panel.locator("#automation-retry-inline").getByText("Continue", {exact:true}).waitFor();
    assert((await panel.locator("#automation-error").textContent()).includes("will not be resent"));
    assert((await panel.locator("#automation-error").textContent()).includes("All 3 automatic retries failed"));
    await panel.locator("#automation-retry-inline").click();
    await panel.locator("#automation-retry-inline").waitFor({state:"hidden"});
    automation.action = { id:"message-attention", kind:"offline_chat", status:"ambiguous", goalRevision:null, lastError:"Message submission could not be confirmed", retryMode:null, sendAccepted:true };
    await panel.locator("#goal-retry").getByText("ChatGPT", {exact:true}).waitFor();
    assert.equal(await panel.locator("#automation-retry-action").isDisabled(),true);
    assert((await panel.locator("#automation-error").textContent()).includes("check whether the message was sent"));
    automation.action = { status:"idle", lastError:null, retryMode:null };
    await panel.locator("#goal-retry").waitFor({state:"hidden"});
    await panel.getByRole("button", { name:"Pause goal" }).click();
    await panel.getByRole("button", {name:"Run goal",exact:true}).waitFor();
    assert.equal(await panel.locator("#goal-icon .goal-outer-ring").evaluate(node=>getComputedStyle(node).animationName),"none");
    await panel.getByRole("button", { name:"Clear goal" }).click();
    await panel.locator("#goal-bar").waitFor({ state:"hidden" });
    assert.equal(messages.length, 0, "local goal controls must not enter chat history");

    automation.presence.lastAgentCallAtMs = Date.now();
    await page.waitForTimeout(1600);
    assert.equal(await model.isDisabled(),false,"online agents may choose cached preferences for future turns");
    assert.equal(await effort.isDisabled(),false);
    await model.selectOption("gpt-5.6-sol");
    await panel.locator("#automation-model:enabled").waitFor();
    assert.equal(await model.inputValue(),"gpt-5.6-sol");
    const pickerSize=await model.evaluate(node=>({width:node.getBoundingClientRect().width,label:node.selectedOptions[0].textContent,font:getComputedStyle(node).font}));
    assert(pickerSize.width<150,"picker width must follow the selected label rather than the longest option");
    automation.companion.status="busy";automation.companion.activeActionId="running-action";
    await page.waitForTimeout(1600);
    assert.equal(await model.isDisabled(),false,"cached preferences do not need an idle browser");
    automation.companion.status="idle";automation.companion.activeActionId=null;
    automation.presence.agentWaitingUntilMs=Date.now()+30000;
    await page.waitForTimeout(1600);
    assert.equal(await model.isDisabled(),false);
    automation.presence.agentWaitingUntilMs=null;
    await panel.getByRole("button", { name:"Connection settings", exact:true }).click();
    automation.action = { id:"failed-action", kind:"offline_chat", status:"failed", pairingGeneration:1, goalRevision:null, lastError:"Selector verification failed before Send", lastStableAnswer:null, updatedAtMs:Date.now() };
    automation.queue = null;
    await panel.getByRole("button", { name:"Retry failed action" }).waitFor({ state:"visible" });
    await panel.locator("#automation-retry-action:enabled").waitFor();
    await panel.getByRole("button", { name:"Retry failed action" }).click();
    await panel.locator("#automation-action").getByText("Queued", { exact:true }).waitFor();
    await panel.getByRole("button", { name:"Cancel queued action" }).click();
    await panel.locator("#automation-action").getByText("Failed", { exact:true }).waitFor();

    await panel.getByRole("button", { name:"Unlink conversation" }).click();
    await panel.getByText("Not paired", { exact:true }).waitFor();
    automation.companion.activeActionId = "unrelated-action";
    assert.equal(await panel.getByRole("button", { name:"Open in ChatGPT" }).isDisabled(), true);
    automation.companion.authenticated = false;
    await page.waitForFunction(()=>document.querySelector('#chat-host .chat-shell').shadowRoot.querySelector('#automation-effort').disabled);
    const savedModel=catalog.models.find(model=>model.id===automation.selection.model);
    assert.equal(await model.locator("option:checked").textContent(), savedModel.label);
    assert.equal(await effort.locator("option:checked").textContent(), savedModel.efforts.find(effort=>effort.id===automation.selection.effort).label);
    assert(!operations.includes("capabilities"),"browsing and preference changes use the cached model list");
    assert(operations.includes("reveal"));
    assert(operations.some(entry => Array.isArray(entry) && entry[0] === "selection"));
    assert(operations.some(entry => Array.isArray(entry) && entry[0] === "pair"));
    assert(operations.some(entry => Array.isArray(entry) && entry[0] === "unlink"));
    assert(operations.some(entry => Array.isArray(entry) && entry[0] === "cancel"));
    assert(operations.some(entry => Array.isArray(entry) && entry[0] === "retry"));
    assert.deepEqual(operations.filter(entry => Array.isArray(entry) && entry[0] === "send").map(entry => entry[1]), [
      "/goal set Finish the standalone controls.", "/goal run", `/goal set ${updatedGoal}`, "/goal stop", "/goal clear"
    ]);
    const bodyText = await page.locator("body").textContent();
    assert(!bodyText.includes("owner_account_1"));
    assert(!bodyText.includes("Bearer test-token"));

    await page.setViewportSize({ width:640, height:800 });
    const box = await panel.locator("#composer").boundingBox();
    assert(box && box.x >= 0 && box.x + box.width <= 640);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});
}

for (const [engineName, engine] of [["Chromium", chromium], ["WebKit", webkit]]) {
  test(`${engineName}: goal time ticks locally, pauses, and resumes without counting messages as turns`, { timeout:30000 }, async () => {
    const browser = await engine.launch();
    try {
      const page = await ownerTestPage(browser); page.setDefaultTimeout(5000);
      let clock = Date.now(); await page.clock.install({ time:clock });
      const chat = { id:"f".repeat(64), title:"Timed goal", workspace:"timer-project", lastEntryEnd:0, lastEntryAtMs:clock, lastAgentCallAtMs:null, totalToolCalls:0 };
      const goal = { markdown:"Keep making progress", status:"active", elapsedActiveMs:0, activeSinceMs:clock - 3000, timerUntilMs:clock - 3000 + 300000, turnCount:1392 };
      let failState = false;
      await routeOwner(page, "http://127.0.0.1:43213/**", async route => {
        const request = route.request(), url = new URL(request.url());
        if (url.pathname === "/") return route.fulfill({ contentType:"text/html", body:ownerHtml });
        if (url.pathname === "/api/chats") return route.fulfill({ json:{ chats:[chat], serverTimeMs:clock } });
        if (url.pathname.endsWith("/automation")) return route.fulfill(failState ? { status:503 } : { json:{ enabled:true, goal, serverTimeMs:clock, companion:{ authenticated:false }, presence:{} } });
        if (url.pathname.endsWith("/send")) {
          failState = false;
          const body = request.postDataJSON();
          if (body.message === "/goal stop") { goal.elapsedActiveMs += clock - goal.activeSinceMs; goal.activeSinceMs = null; goal.status = "configured"; }
          if (body.message === "/goal run") { goal.activeSinceMs = clock; goal.status = "active"; }
          return route.fulfill({ json:{ _meta:{ [META]:{ goal } } } });
        }
        return route.fulfill({ json:{ _meta:{ [META]:{ chat_file:"/private/timer/CHAT.md", revision:"0", delivered_through:0, read_through:0, last_agent_call_at_ms:null, total_tool_calls:0, server_time_ms:clock, messages:[], unchanged:false, has_more:false } } } });
      });
      await page.goto(`http://127.0.0.1:43213/?chat=${chat.id}#test-token`);
      const metrics = page.locator("#goal-metrics");
      await metrics.getByText("3s (1392 turns)", { exact:true }).waitFor();
      failState = true; clock += 1000; await page.clock.fastForward(1000);
      await metrics.getByText("4s (1392 turns)", { exact:true }).waitFor();
      clock += 60000; await page.clock.fastForward(60000);
      await metrics.getByText("1m 4s (1392 turns)", { exact:true }).waitFor();
      await page.getByRole("button", { name:"Pause goal", exact:true }).click();
      await page.getByRole("button", { name:"Run goal", exact:true }).waitFor();
      failState = true; clock += 10000; await page.clock.fastForward(10000);
      assert.equal(await metrics.textContent(), "1m 4s (1392 turns)");
      await page.getByRole("button", { name:"Run goal", exact:true }).click();
      await page.getByRole("button", { name:"Pause goal", exact:true }).waitFor();
      failState = true; clock += 3000; await page.clock.fastForward(3000);
      await metrics.getByText("1m 7s (1392 turns)", { exact:true }).waitFor();
      goal.elapsedActiveMs += clock - goal.activeSinceMs;
      goal.activeSinceMs = null; goal.status = "blocked"; goal.blockedReason = "Waiting for input";
      failState = false; clock += 2000; await page.clock.fastForward(2000);
      await page.getByText("Goal blocked", { exact:true }).waitFor();
      assert.equal(await metrics.textContent(), "1m 7s (1392 turns)");
      const play = page.getByRole("button", { name:"Run goal", exact:true });
      assert.equal(await play.isEnabled(), true, "blocked goals must allow Play");
      await play.click();
      await page.getByText("Pursuing goal", { exact:true }).waitFor();
      failState = true; clock += 3000; await page.clock.fastForward(3000);
      await metrics.getByText("1m 10s (1392 turns)", { exact:true }).waitFor();
      const remaining = goal.timerUntilMs - clock;
      clock += remaining; await page.clock.fastForward(remaining);
      const atOffline = await metrics.textContent();
      clock += 60000; await page.clock.fastForward(60000);
      assert.equal(await metrics.textContent(), atOffline, "offline time must not increase even while polling fails");
      goal.elapsedActiveMs += goal.timerUntilMs - goal.activeSinceMs;
      goal.activeSinceMs = clock; goal.timerUntilMs = clock + 300000;
      failState = false; await page.clock.fastForward(2000);
      await metrics.getByText(atOffline, {exact:true}).waitFor();
      failState = true; clock += 3000; await page.clock.fastForward(3000);
      assert.notEqual(await metrics.textContent(), atOffline, "fresh agent activity resumes the timer");
      failState = false; goal.turnCount = 0;
      await page.clock.fastForward(2000);
      await metrics.getByText(/\(1 turn\)$/).waitFor();
    } finally { await browser.close(); }
  });
}

for (const [engineName, engine] of [["Chromium",chromium],["WebKit",webkit]]) {
  test(`${engineName}: new chats discover models and preserve workspace choices`,{timeout:30000},async()=>{
    const browser=await engine.launch();
    try {
      const page=await ownerTestPage(browser);page.setDefaultTimeout(5000);
      const chats=[], submitted=[], createdProjects=[], errors=[];let preparations=0;
      page.on('pageerror',error=>errors.push(error.message));
      await routeOwner(page, 'http://127.0.0.1:43215/**',async route=>{
        const request=route.request(),url=new URL(request.url());
        if(url.pathname==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
        assert.equal(request.headers().authorization,'Bearer test-token');
        if(url.pathname==='/api/new-chat'){
          const efforts=[{id:'low',label:'Instant',available:true},{id:'medium',label:'Medium',available:true},{id:'high',label:'High',available:true},{id:'xhigh',label:'Extra High',available:true},{id:'max',label:'Pro',available:true}];
          preparations++;return route.fulfill({json:{catalogRevision:'fresh-project-models',chatgptProjectUrl:'https://chatgpt.com/g/g-p-fixture/project',models:[
            {id:'live-model',label:'Live model',efforts},
            {id:'limited-model',label:'Limited model',efforts:efforts.map(effort=>({...effort,available:effort.id!=='max'}))},
            {id:'future-model',label:'Future model',efforts:[{id:'variable',label:'Variable choice',available:true},{id:'expanded',label:'Expanded choice',available:true}]}
          ]}});
        }
        if(url.pathname==='/api/projects'&&request.method()==='POST'){
          const body=request.postDataJSON();createdProjects.push(body);
          if(body.name==='duplicate')return route.fulfill({status:409,body:'A file or folder with that name already exists'});
          return route.fulfill({json:{path:`/workspace/${body.name}`,name:body.name}});
        }
        if(url.pathname==='/api/projects')return route.fulfill({json:{createWorktree:true,catalog:{access_root:'/workspace',projects:[{selector:'repo',name:'Existing repo'}],total:1,warnings:[]}}});
        if(url.pathname==='/api/projects/worktrees')return route.fulfill({json:{worktrees:[{name:'Saved worktree',path:'/workspace/saved'}]}});
        if(url.pathname==='/api/chats'&&request.method()==='POST'){
          const body=request.postDataJSON();submitted.push(body);
          const id=String(submitted.length).repeat(64);chats.push({id,title:'New conversation',workspace:'repo',projectId:'repo',projectPath:'/workspace/repo',lastEntryEnd:0,lastEntryAtMs:Date.now(),lastAgentCallAtMs:null,totalToolCalls:0});
          return route.fulfill({json:{id,queued:true}});
        }
        if(url.pathname==='/api/chats')return route.fulfill({json:{chats,serverTimeMs:Date.now()}});
        if(url.pathname.endsWith('/automation'))return route.fulfill({json:{enabled:false,serverTimeMs:Date.now()}});
        return route.fulfill({json:{_meta:{[META]:{chat_file:'/private/new/CHAT.md',workspace_path:'/workspace/repo',messages:[],revision:'0',delivered_through:0,read_through:0,last_agent_call_at_ms:null,agent_waiting_until_ms:null,total_tool_calls:0,server_time_ms:Date.now(),has_more:false,before:null,unchanged:false}}}});
      });
      await page.goto('http://127.0.0.1:43215/#test-token');
      const dialog=page.getByRole('dialog',{name:'New chat'});
      const open=async()=>{
        const back=page.locator('#chat-host').getByRole('button',{name:'Back to chats',exact:true});
        if(await back.isVisible())await back.click();
        await page.getByRole('button',{name:'New chat',exact:true}).click();
        await dialog.getByRole('button',{name:'Existing repo',exact:true}).waitFor();
        await dialog.locator('#new-chat-model:enabled').waitFor();
        assert.deepEqual(await dialog.getByLabel('Reasoning effort').locator('option').allTextContents(),['Pro','Extra High','High','Medium','Instant']);
        assert.equal(await dialog.getByLabel('Reasoning effort').inputValue(),'max');
      };
      await open();
      const model=dialog.getByLabel('ChatGPT model'),effort=dialog.getByLabel('Reasoning effort');
      await effort.selectOption('medium');
      await model.selectOption('limited-model');
      assert.equal(await effort.inputValue(),'xhigh','default skips the locked highest option');
      assert.equal(await effort.locator('option[value="max"]').isDisabled(),true);
      await model.selectOption('future-model');
      assert.deepEqual(await effort.locator('option').allTextContents(),['Expanded choice','Variable choice']);
      assert.equal(await effort.inputValue(),'expanded','new effort labels follow the discovered slider order');
      await model.selectOption('live-model');
      assert.equal(await effort.inputValue(),'max','switching models resets the default to the highest available effort');
      const url=dialog.getByLabel('Project path or Git URL'),create=dialog.getByRole('button',{name:'Create chat',exact:true});
      assert.equal(await create.isDisabled(),true,'a workspace or nonempty path is required');
      await url.fill('https://github.com/example/ignored.git');
      assert.equal(await create.isEnabled(),true,'typing a path enables creation without confirmation');
      const checkbox=dialog.getByRole('checkbox',{name:'Create a worktree for the selected project'});
      assert.equal(await checkbox.isChecked(),true);await checkbox.uncheck();
      await dialog.getByLabel('Search projects').fill('existing');await page.waitForTimeout(350);
      assert.equal(await checkbox.isChecked(),false,'searching must preserve an explicit worktree override');
      await dialog.getByRole('button',{name:'Existing repo',exact:true}).click();
      await effort.selectOption('medium');
      await dialog.getByRole('button',{name:'Create chat',exact:true}).click();await dialog.waitFor({state:'hidden'});
      assert.equal(submitted[0].path,'repo');assert.equal(submitted[0].createWorktree,false);assert.equal(submitted[0].effort,'medium');
      await open();await url.fill('https://github.com/example/ignored.git');await dialog.getByRole('button',{name:'Chat without a project',exact:true}).click();
      await dialog.getByRole('button',{name:'Create chat',exact:true}).click();await dialog.waitFor({state:'hidden'});
      assert.equal(submitted[1].withoutProject,true);assert.equal(submitted[1].effort,'max');assert(!Object.hasOwn(submitted[1],'createWorktree'));assert(!Object.hasOwn(submitted[1],'path'));
      await open();await url.fill('https://github.com/example/project/pull/42');
      await url.fill('   ');assert.equal(await create.isDisabled(),true,'clearing an unselected path disables creation');
      await url.fill('  https://github.com/example/project/pull/42  ');
      await dialog.getByRole('button',{name:'Create chat',exact:true}).click();await dialog.waitFor({state:'hidden'});
      assert.equal(submitted[2].path,'https://github.com/example/project/pull/42');assert.equal(submitted[2].createWorktree,true);
      await open();await url.fill('https://github.com/example/ignored.git');await dialog.getByRole('button',{name:'Existing worktrees for Existing repo',exact:true}).click();await dialog.getByRole('button',{name:'Saved worktree',exact:true}).click();
      await dialog.getByRole('button',{name:'Create chat',exact:true}).click();await dialog.waitFor({state:'hidden'});
      assert.equal(submitted[3].path,'repo');assert.equal(submitted[3].worktreePath,'/workspace/saved');assert(!Object.hasOwn(submitted[3],'createWorktree'));
      await open();await dialog.getByRole('button',{name:'Existing repo',exact:true}).click();
      await url.fill('https://github.com/example/override.git');
      await dialog.getByRole('button',{name:'Confirm',exact:true}).click();
      await create.click();await dialog.waitFor({state:'hidden'});
      assert.equal(submitted[4].path,'https://github.com/example/override.git','Confirm explicitly overrides the selected project');
      assert.equal(submitted[4].createWorktree,true);
      await open();await url.fill('/workspace/local-project');await checkbox.uncheck();
      await create.click();await dialog.waitFor({state:'hidden'});
      assert.equal(submitted[5].path,'/workspace/local-project','filesystem paths also submit directly');
      assert.equal(submitted[5].createWorktree,false);
      await open();await dialog.getByRole('button',{name:'Existing worktrees for Existing repo',exact:true}).click();await dialog.getByRole('button',{name:'Saved worktree',exact:true}).click();
      assert.equal(await checkbox.isEnabled(),true);assert.equal(await checkbox.isChecked(),false);
      await url.fill('/workspace/confirmed');await dialog.getByRole('button',{name:'Confirm',exact:true}).click();
      assert.equal(await checkbox.isEnabled(),true,'Confirm releases the selected existing worktree');
      await url.fill('/workspace/latest');
      await create.click();await dialog.waitFor({state:'hidden'});
      assert.equal(submitted[6].path,'/workspace/latest','the path field stays current after confirmation');
      assert(!Object.hasOwn(submitted[6],'worktreePath'));assert.equal(submitted[6].createWorktree,false,'Confirm preserves the current checkbox choice');
      await page.setViewportSize({width:390,height:844});
      await open();await dialog.getByRole('button',{name:'Existing worktrees for Existing repo',exact:true}).click();
      const from=dialog.getByRole('button',{name:'Create a new worktree from Saved worktree',exact:true});
      await from.waitFor();const fromBox=await from.boundingBox();assert(fromBox.x>=0&&fromBox.x+fromBox.width<=390);
      await from.click();assert.equal(await checkbox.isChecked(),true);assert.equal(await checkbox.isEnabled(),true);
      if(process.env.WORKTREE_BASE_SCREENSHOTS){mkdirSync(process.env.WORKTREE_BASE_SCREENSHOTS,{recursive:true});await page.screenshot({path:`${process.env.WORKTREE_BASE_SCREENSHOTS}/${engineName.toLowerCase()}-base.png`});}
      await create.click();await dialog.waitFor({state:'hidden'});
      assert.equal(submitted[7].path,'repo');assert.equal(submitted[7].worktreePath,'/workspace/saved');assert.equal(submitted[7].createWorktree,true);
      await open();await dialog.getByRole('button',{name:'Existing worktrees for Existing repo',exact:true}).click();await dialog.getByRole('button',{name:'Saved worktree',exact:true}).click();
      await checkbox.check();await dialog.locator('#new-chat-selection').getByText('New worktree from /workspace/saved',{exact:true}).waitFor();
      await checkbox.uncheck();await dialog.locator('#new-chat-selection').getByText('/workspace/saved',{exact:true}).waitFor();
      await checkbox.check();await create.click();await dialog.waitFor({state:'hidden'});
      assert.equal(submitted[8].path,'repo');assert.equal(submitted[8].worktreePath,'/workspace/saved');assert.equal(submitted[8].createWorktree,true);
      await open();await url.fill('/workspace/saved');await create.click();await dialog.waitFor({state:'hidden'});
      assert.equal(submitted[9].path,'/workspace/saved');assert.equal(submitted[9].createWorktree,true);
      await open();await dialog.getByRole('button',{name:'Existing repo',exact:true}).click();
      await dialog.getByRole('button',{name:'New project',exact:true}).click();
      const projectName=dialog.getByLabel('New project name'),createProject=dialog.getByRole('button',{name:'Create project',exact:true});
      assert.equal(await createProject.isDisabled(),true);
      for(const control of [projectName,createProject]){
        const box=await control.boundingBox();assert(box.x>=0&&box.x+box.width<=390,'project creation controls fit on mobile');
      }
      await dialog.getByText('Creates a folder in /workspace.',{exact:true}).waitFor();
      await projectName.fill('duplicate');await createProject.click();
      await dialog.getByRole('alert').getByText('A file or folder with that name already exists',{exact:true}).waitFor();
      assert.equal(await dialog.locator('#new-chat-selection').textContent(),'Existing repo','a failed folder creation preserves the selection');
      await projectName.fill('  My new project  ');await projectName.press('Enter');
      await dialog.locator('#new-chat-selection').getByText('/workspace/My new project',{exact:true}).waitFor();
      assert.equal(await projectName.isHidden(),true);
      assert.equal(await url.inputValue(),'/workspace/My new project');
      assert.equal(await checkbox.isChecked(),false,'new empty folders are used directly');
      assert.equal(submitted.length,10,'creating a project must not create or send a chat');
      assert.deepEqual(createdProjects,[{name:'duplicate'},{name:'My new project'}]);
      await create.click();await dialog.waitFor({state:'hidden'});
      assert.equal(submitted[10].path,'/workspace/My new project');assert.equal(submitted[10].createWorktree,false);
      await open();assert.equal(await projectName.isHidden(),true);assert.equal(await projectName.inputValue(),'');
      await dialog.getByRole('button',{name:'Close new chat',exact:true}).click();
      assert.equal(preparations,12,'every New chat must freshly discover its destination-specific models');
      assert.deepEqual(errors,[]);
    }finally{await browser.close();}
  });
}

test('verified pairing skips away after its short presence lease while normal activity retains the normal timeout',()=>{
 const source=ownerHtml.match(/function presence\([^)]*\) \{[\s\S]*?\n  \}/)?.[0];const presence = Function('offlineAfterMs', `return (${source})`)(300000);
 assert.equal(presence(1000,5999,null,6000),'online');assert.equal(presence(1000,6000,null,6000),'offline');
 assert.equal(presence(1000,180000,null,6000),'offline');assert.equal(presence(1000,6000,9000,6000),'waiting');
 assert.equal(presence(2000,6000,null,null),'online');
});
for(const [engineName,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${engineName}: verified pairing becomes offline after five seconds and resumes on real activity`,{timeout:30000},async()=>{
 const browser=await engine.launch();
 try {
  const page=await ownerTestPage(browser);page.setDefaultTimeout(5000);const now=Date.now();await page.clock.install({time:now});
  const id='d'.repeat(64),errors=[];let serverNow=now,lastCall=now,until=now+5000;page.on('pageerror',error=>errors.push(error.message));
  await routeOwner(page, 'http://127.0.0.1:43232/**',route=>{
   const path=new URL(route.request().url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/chats')return route.fulfill({json:{chats:[{id,title:'Just paired',workspace:'pairing',lastEntryEnd:0,lastEntryAtMs:now,lastAgentCallAtMs:lastCall,agentPresenceUntilMs:until,totalToolCalls:2}],serverTimeMs:serverNow}});
   if(path.endsWith('/automation'))return route.fulfill({json:{enabled:true,pairing:{status:'verified'},goal:null,action:{status:'idle'},selection:{},companion:{authenticated:true,status:'idle'},presence:{lastAgentCallAtMs:lastCall,agentPresenceUntilMs:until},serverTimeMs:serverNow}});
   return route.fulfill({json:{_meta:{[META]:{chat_file:'/private/pairing/CHAT.md',revision:'0',delivered_through:0,read_through:0,last_agent_call_at_ms:lastCall,agent_presence_until_ms:until,total_tool_calls:2,server_time_ms:serverNow,messages:[],has_more:false,unchanged:true}}}});
  });
  await page.goto(`http://127.0.0.1:43232/?chat=${id}#test-token`);const host=page.locator('#chat-host');
  await host.locator('#presence .presence-label').getByText('online',{exact:true}).waitFor();await page.locator('.conversation .presence.online').waitFor();
  serverNow=now+4000;await page.clock.fastForward(4000);assert.equal(await host.locator('#presence .presence-label').textContent(),'online');
  serverNow=now+5000;await page.clock.fastForward(1000);await host.locator('#presence .presence-label').getByText('offline',{exact:true}).waitFor();
  assert.equal(await page.locator('.conversation .presence.away').count(),0);await page.locator('.conversation .presence.offline').waitFor({state:'attached'});
  assert.equal(await host.locator('#tool-total').textContent(),'2 tool calls');
  // A new working turn clears the special deadline on the next metadata update.
  lastCall=now+5000;until=null;serverNow=now+7500;await page.clock.fastForward(2500);
  await host.locator('#presence .presence-label').getByText('online',{exact:true}).waitFor();await page.locator('.conversation .presence.online').waitFor();
  assert.deepEqual(errors,[]);
 }finally{await browser.close();}
});


for (const [engineName, engine] of [["Chromium", chromium], ["WebKit", webkit]]) {
  test(`${engineName}: Dismissing errors clears the saved failure and cancels retries across polling`, {timeout:30000}, async()=>{
    const browser=await engine.launch();
    try {
      const page=await ownerTestPage(browser,{viewport:{width:1280,height:900}});page.setDefaultTimeout(5000);
      const id='d'.repeat(64),now=Date.now(),errors=[],cancellations=[];
      page.on('pageerror',error=>errors.push(error.message));
      const chat={id,title:'Retry cancellation',workspace:'test',lastEntryEnd:1,lastEntryAtMs:now,lastAgentCallAtMs:null,totalToolCalls:1};
      const goal={revision:1,markdown:'Preserve this goal',status:'active',turnCount:1};
      let action={id:'backoff',kind:'offline_goal',status:'failed',lastError:'Browser unavailable',sendAccepted:false,retryMode:'retry',automaticRetryAtMs:now+30000,automaticRetryAttempts:0};
      await routeOwner(page, 'http://127.0.0.1:43225/**',route=>{
        const request=route.request(),url=new URL(request.url());
        if(url.pathname==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
        if(url.pathname==='/api/chats')return route.fulfill({json:{chats:[{...chat,automationError:action.lastError}],serverTimeMs:Date.now()}});
        if(url.pathname.endsWith('/automation'))return route.fulfill({json:{enabled:true,goal,action,catalog:{models:[]},pairing:{status:'verified'},companion:{authenticated:true,status:'idle'},serverTimeMs:Date.now(),presence:{lastAgentCallAtMs:null}}});
        if(url.pathname.endsWith('/action/cancel')){
          const body=request.postDataJSON();assert.equal(body.actionId,action.id);cancellations.push(body);
          action={...action,status:'cancelled',lastError:null,automaticRetryAtMs:null,retryMode:null};
          return route.fulfill({json:{action,queue:null}});
        }
        return route.fulfill({json:{_meta:{[META]:{chat_file:'/private/test/CHAT.md',revision:'1',delivered_through:0,read_through:0,last_agent_call_at_ms:null,total_tool_calls:1,server_time_ms:Date.now(),messages:[],has_more:false,unchanged:false}}}});
      });
      await page.goto(`http://127.0.0.1:43225/?chat=${id}#test-token`);
      const host=page.locator('#chat-host'),dismiss=host.getByRole('button',{name:'Dismiss error',exact:true}),error=host.locator('#automation-error');
      for(const failure of [action,{...action,id:'exhausted',automaticRetryAtMs:null,automaticRetryAttempts:5},{...action,id:'uncertain',status:'ambiguous',sendAccepted:true,retryMode:null,automaticRetryAtMs:null}]) {
        action=failure;
        await error.waitFor();await dismiss.waitFor();
        const controls=host.locator('#automation-error-row');
        const row=await controls.boundingBox(),cancelBox=await dismiss.boundingBox();
        assert(cancelBox.x>=row.x&&cancelBox.x+cancelBox.width<=row.x+row.width+1);
        if(failure.id==='backoff') {
          await error.getByText(/Retrying the goal automatically/).waitFor();
          const retryBox=await host.locator('#automation-retry-inline').boundingBox();
          assert(Math.abs(retryBox.y-cancelBox.y)<2,'Dismiss and Retry align on the same row');
          mkdirSync('/tmp/chat-error-cancel-ui',{recursive:true});
          await page.screenshot({path:`/tmp/chat-error-cancel-ui/${engineName.toLowerCase()}-before.png`});
        }
        const before=structuredClone(action);
        await dismiss.click();await error.waitFor({state:'hidden'});await dismiss.waitFor({state:'hidden'});
        await page.waitForTimeout(1700);
        assert.equal(await error.isVisible(),false,'polling and countdown changes do not restore a dismissed error');
        assert.equal(action.status,'cancelled');assert.equal(action.lastError,null);assert.equal(action.automaticRetryAtMs,null);
        assert.equal(action.sendAccepted,before.sendAccepted,'dismiss preserves delivery evidence');
        assert.equal(goal.status,'active');
        await host.getByRole('button',{name:'Connection settings',exact:true}).click();
        assert.equal(await host.locator('#connection-settings-error').isVisible(),false,'dismiss clears the stored error everywhere');
        await host.getByRole('button',{name:'Close connection settings',exact:true}).click();
        assert.equal(await host.locator('#goal-text').textContent(),goal.markdown);
        if(failure.id==='backoff')await page.screenshot({path:`/tmp/chat-error-cancel-ui/${engineName.toLowerCase()}-after.png`});
      }
      assert.equal(cancellations.length,3);assert.equal(action.sendAccepted,true);assert.deepEqual(errors,[]);
    }finally{await browser.close();}
  });
}


for (const [engineName, engine] of [["Chromium", chromium], ["WebKit", webkit]]) {
  test(`${engineName}: submitted pairing has a grace period, keeps the created URL, and clears a late verification error`, {timeout:30000}, async()=>{
    const browser=await engine.launch();
    try {
      const page=await ownerTestPage(browser,{viewport:{width:1280,height:900}});page.setDefaultTimeout(5000);
      let clock=Date.now();await page.clock.install({time:clock});
      const id='e'.repeat(64),createdUrl='https://chatgpt.com/c/created-before-restoration-error',errors=[];
      page.on('pageerror',error=>errors.push(error.message));
      const waitUntil=clock+60000;
      let paired=false;
      const chat={id,title:'Late pairing',workspace:'test',lastEntryEnd:1,lastEntryAtMs:clock,lastAgentCallAtMs:null,totalToolCalls:1};
      const model={id:'gpt-6-sol',label:'GPT-6 Sol',efforts:[{id:'medium',label:'Medium',available:true}]};
      await routeOwner(page, 'http://127.0.0.1:43226/**',route=>{
        const url=new URL(route.request().url());
        if(url.pathname==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
        if(url.pathname==='/api/chats')return route.fulfill({json:{chats:[{...chat,automationError:!paired&&clock>=waitUntil?'Pairing verification has not arrived yet':null}],serverTimeMs:clock}});
        if(url.pathname.endsWith('/automation'))return route.fulfill({json:{enabled:true,goal:null,action:{id:'creation-action',kind:'pair_verify',status:paired?'idle':'ambiguous',lastError:paired?null:'Preference restoration timed out',sendAccepted:true,retryMode:null},catalog:{catalogRevision:'fixture',models:[model]},pairing:{status:paired?'verified':'unverified',creatingChat:!paired,canonicalUrl:createdUrl,candidateUrl:paired?null:'https://chatgpt.com/g/g-p-project/project',lastTriedUrl:null,verificationWaitUntilMs:waitUntil,expiresAtMs:paired?null:waitUntil+540000},companion:{authenticated:true,status:'idle'},serverTimeMs:clock,presence:{lastAgentCallAtMs:null}}});
        return route.fulfill({json:{_meta:{[META]:{chat_file:'/private/test/CHAT.md',revision:'1',delivered_through:0,read_through:0,last_agent_call_at_ms:null,total_tool_calls:1,server_time_ms:clock,messages:[],has_more:false,unchanged:false}}}});
      });
      await page.goto(`http://127.0.0.1:43226/?chat=${id}#test-token`);
      const host=page.locator('#chat-host'),pending=host.getByRole('button',{name:'Pairing…',exact:true});
      await pending.waitFor();assert.equal(await pending.isDisabled(),true);
      assert.equal(await host.locator('#automation-error').isVisible(),false);
      await host.getByRole('button',{name:'Connection settings',exact:true}).click();
      await host.getByLabel('ChatGPT conversation URL',{exact:true}).waitFor();
      assert.equal(await host.getByLabel('ChatGPT conversation URL',{exact:true}).inputValue(),createdUrl);
      await host.getByRole('button',{name:'Close connection settings',exact:true}).click();
      clock+=61000;await page.clock.fastForward(61000);
      await host.getByRole('button',{name:'Retry chat pairing',exact:true}).waitFor();
      await host.locator('#automation-error').waitFor();await page.locator('.conversation .presence.error').waitFor();
      await host.getByRole('button',{name:'Retry chat pairing',exact:true}).click();
      assert.equal(await host.getByLabel('ChatGPT URL',{exact:true}).inputValue(),createdUrl);
      await host.getByRole('button',{name:'Close pairing',exact:true}).click();
      paired=true;clock+=6000;await page.clock.fastForward(6000);
      await host.locator('#automation-pair-chat').waitFor({state:'hidden'});
      await host.locator('#automation-error').waitFor({state:'hidden'});
      await page.locator('.conversation .presence.error').waitFor({state:'detached'});
      assert.deepEqual(errors,[]);
    }finally{await browser.close();}
  });
}

async function mobileNavigationFixture(browser, options = {}) {
  const {awaiting=false,...pageOptions}=options;
  const page = await ownerTestPage(browser, {viewport:{width:390,height:844}, ...pageOptions});
  page.setDefaultTimeout(6000);
  const errors = [], writes = [], now = Date.now(), origin = 'http://127.0.0.1:43228';
  const chats = Array.from({length:60}, (_, index) => ({
    id:(index+1).toString(16).padStart(64,'0'), title:index ? `Conversation ${index+1}` : 'Bioshock optimization',
    workspace:'mach-o-vita',projectPath:'/Volumes/data/dev/mach-o-vita',lastEntryEnd:200,
    lastEntryAtMs:now-index*1000,lastAgentCallAtMs:now,totalToolCalls:16998,
  }));
  page.on('pageerror', error => errors.push(error.message));
  await routeOwner(page, origin+'/**', route => {
    const request=route.request(), url=new URL(request.url());
    if(request.method()==='POST')writes.push(url.pathname);
    if(url.pathname==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
    if(url.pathname==='/api/chats')return route.fulfill({json:{chats,matchedChatIds:chats.filter(chat=>chat.title.toLowerCase().includes((url.searchParams.get('q')||'').toLowerCase())).map(chat=>chat.id),serverTimeMs:Date.now()}});
    const chat=chats.find(chat=>chat.id===url.pathname.split('/')[3]);assert(chat,'known mobile chat request: '+url.pathname);
    if(url.pathname.endsWith('/organization')){
      Object.assign(chat,request.postDataJSON().pinned ? {pinnedAtMs:now} : {pinnedAtMs:null});
      return route.fulfill({json:{pinnedAtMs:chat.pinnedAtMs,archivedAtMs:null}});
    }
    if(url.pathname.endsWith('/automation'))return route.fulfill({json:{enabled:true,
      pairing:{status:'verified'},selection:{model:'gpt-5.6-sol',effort:'max'},action:{status:'idle'},
      catalog:{models:[{id:'gpt-5.6-sol',label:'GPT-5.6',efforts:[{id:'max',label:'Pro',available:true}]}]},
      goal:{status:'active',revision:1,markdown:'Finish the implementation and verify the results.',elapsedActiveMs:281900000,turnCount:84},
      companion:{authenticated:true,status:'idle'},presence:{lastAgentCallAtMs:now},serverTimeMs:Date.now(),
    }});
    return route.fulfill({json:{_meta:{[META]:{chat_file:`/fixture/${chat.id}/CHAT.md`,revision:'200',read_through:200,delivered_through:200,total_tool_calls:16998,last_agent_call_at_ms:now,agent_waiting_until_ms:awaiting?Date.now()+60000:null,server_time_ms:Date.now(),has_more:false,unchanged:false,messages:[
      {id:'update',role:'agent',markdown:`Update for ${chat.title}. The focused checks pass. I’m reviewing the remaining results before proceeding.`,start:0,end:200,tool_call_count:16998,created_at_ms:now},
    ]}}}});
  });
  return {page,chats,errors,writes,origin};
}

for (const [name,engine] of [['Chromium',chromium],['WebKit',webkit]]) {
  test(`${name}: mobile navigation gives the list and chat separate full-width views`, {timeout:30000}, async()=>{
    const browser=await engine.launch();
    try {
      const {page,chats,errors,writes,origin}=await mobileNavigationFixture(browser);
      await page.goto(origin+'/#test-token');
      const host=page.locator('#chat-host'),nav=page.locator('#conversations'),sidebar=page.locator('#layout > aside'),main=page.locator('#layout > main');
      const row=chat=>page.locator(`[data-chat-id="${chat.id}"] .conversation`);
      const fullWidth=async node=>{const box=await node.boundingBox();assert(box&&Math.abs(box.x)<1&&Math.abs(box.width-page.viewportSize().width)<1,'the visible pane fills the viewport');};
      const capture=async label=>{if(process.env.CODEXIFY_MOBILE_NAV_SCREENSHOTS){mkdirSync(process.env.CODEXIFY_MOBILE_NAV_SCREENSHOTS,{recursive:true});await page.screenshot({path:`${process.env.CODEXIFY_MOBILE_NAV_SCREENSHOTS}/${name.toLowerCase()}-${label}.png`});}};
      await row(chats[0]).waitFor();assert(await main.isHidden());await fullWidth(sidebar);await capture('list');
      await page.getByRole('button',{name:'Recent',exact:true}).click();
      const search=page.getByRole('searchbox',{name:'Search chats',exact:true});await search.fill('Conversation');
      await row(chats[20]).scrollIntoViewIfNeeded();const scroll=await nav.evaluate(node=>node.scrollTop);
      await row(chats[20]).click();await host.locator('#draft').waitFor();await host.locator('#goal-metrics').getByText(/84 turns/).waitFor();
      assert(await sidebar.isHidden());await fullWidth(main);
      const back=host.getByRole('button',{name:'Back to chats',exact:true});await back.waitFor();assert((await back.boundingBox()).width>=44);
      for(const width of [320,390,760]){
        await page.setViewportSize({width,height:844});await fullWidth(main);
        assert.equal(await page.locator('body').evaluate(node=>node.scrollWidth>innerWidth),false);
        const send=await host.locator('#send').boundingBox();assert(send.x>=0&&send.x+send.width<=width,'composer action fits');
      }
      await page.setViewportSize({width:390,height:844});await capture('chat');
      await host.locator('#draft').fill('Keep this unsent draft');await back.click();
      assert(await main.isHidden());await fullWidth(sidebar);assert.equal(new URL(page.url()).searchParams.get('chat'),null);
      assert.equal(await search.inputValue(),'Conversation');assert.equal(await page.getByRole('button',{name:'Recent',exact:true}).getAttribute('aria-pressed'),'true');
      assert(Math.abs(await nav.evaluate(node=>node.scrollTop)-scroll)<2,'back restores list scroll');
      await row(chats[20]).click();await host.locator('#draft').waitFor();assert.equal(await host.locator('#draft').inputValue(),'Keep this unsent draft');
      await page.goBack();await sidebar.waitFor();assert(await main.isHidden());
      await page.goForward();await host.locator('#draft').waitFor();assert(await sidebar.isHidden());
      await page.reload();await host.locator('#draft').waitFor();assert(await sidebar.isHidden());assert.equal(await host.locator('#draft').inputValue(),'Keep this unsent draft');
      await host.getByRole('button',{name:'Back to chats',exact:true}).click();
      await row(chats[21]).click();await host.locator('#draft').waitFor();assert.equal(await host.locator('#draft').inputValue(),'');
      await host.locator('#messages').getByText(`Update for ${chats[21].title}.`,{exact:false}).waitFor();
      assert((await host.locator('#messages').textContent()).includes(chats[21].title));
      await page.setViewportSize({width:1280,height:900});await sidebar.waitFor();assert(await main.isVisible());assert(await host.getByRole('button',{name:'Back to chats',exact:true}).isHidden());await capture('desktop');
      assert(!writes.some(path=>path.endsWith('/send')||path.includes('/goal')||path.endsWith('/action/cancel')),'navigation never sends, cancels, or pauses agent work');assert.deepEqual(errors,[]);
    }finally{await browser.close();}
  });
  test(`${name}: touch landscape and archived-list navigation remain single-pane`, {timeout:20000}, async()=>{
    const browser=await engine.launch();
    try {
      const {page,chats,errors,origin}=await mobileNavigationFixture(browser,{viewport:{width:844,height:390},isMobile:true,hasTouch:true});
      await page.goto(origin+'/?chat='+chats[0].id+'#test-token');
      const host=page.locator('#chat-host');await host.locator('#draft').waitFor();
      assert(await page.locator('#layout > aside').isHidden());assert.equal(Math.round((await page.locator('#layout > main').boundingBox()).width),844);
      await host.getByRole('button',{name:'Back to chats',exact:true}).click();assert(await page.locator('#layout > main').isHidden());
      await page.getByRole('button',{name:'Archived chats',exact:true}).click();await page.getByRole('button',{name:'Back to agent chats',exact:true}).click();
      assert(await page.locator('#layout > main').isHidden(),'returning from archived chats stays in the chat list');
      await page.locator(`[data-chat-id="${chats[0].id}"] [data-action="pin"]`).click();
      assert(await page.locator('#layout > main').isHidden(),'pinning a chat does not open it');assert.deepEqual(errors,[]);
    }finally{await browser.close();}
  });
}


for (const [name,engine] of [['Chromium',chromium],['WebKit',webkit]]) {
  test(`${name}: mobile viewport is fixed and composer focus outlines are rounded`, {timeout:30000}, async()=>{
    const browser=await engine.launch();
    try {
      for(const awaiting of [false,true]){
        const {page,chats,errors,origin}=await mobileNavigationFixture(browser,{awaiting});
        await page.goto(origin+'/?chat='+chats[0].id+'#test-token');
        const host=page.locator('#chat-host'),input=host.locator('#draft');await input.waitFor();
        if(awaiting)await host.locator('#chat[data-awaiting="true"]').waitFor();
        const viewport=()=>page.locator('meta[name="viewport"]').getAttribute('content');
        assert((await viewport()).includes('maximum-scale=1'));assert((await viewport()).includes('user-scalable=no'));
        assert.equal(await page.locator('html').evaluate(e=>getComputedStyle(e).touchAction),'pan-x pan-y');
        assert.equal(await page.locator('body').evaluate(e=>getComputedStyle(e).overflow),'hidden');
        const scale=await page.evaluate(()=>visualViewport.scale);
        await input.fill('Keep this draft while the viewport changes.');await input.focus();
        const appearance=await input.evaluate(e=>{const s=getComputedStyle(e);return {radius:s.borderTopLeftRadius,size:s.fontSize,outline:s.outlineColor,width:s.outlineWidth};});
        assert.equal(appearance.radius,'12px');assert.equal(appearance.size,'16px');assert.equal(appearance.width,'2px');
        assert.equal(appearance.outline,awaiting?'rgb(196, 49, 59)':'rgb(19, 111, 80)');
        assert.equal(await page.evaluate(()=>visualViewport.scale),scale);
        for(const height of [500,844]){
          await page.setViewportSize({width:390,height});
          await page.waitForFunction(()=>Math.abs(document.querySelector('#layout').getBoundingClientRect().height-visualViewport.height)<1);
          const send=await host.locator('#send').boundingBox();assert(send.y>=0&&send.y+send.height<=height,'composer remains inside the visible viewport');
          assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
          assert.equal(await input.inputValue(),'Keep this draft while the viewport changes.');
        }
        assert.equal(await page.evaluate(()=>document.dispatchEvent(new Event('gesturestart',{cancelable:true}))),false,'single-pane mode suppresses Safari pinch gestures');
        await page.setViewportSize({width:761,height:900});
        await page.waitForFunction(()=>!document.querySelector('meta[name="viewport"]').content.includes('maximum-scale'));
        assert.equal(await page.locator('html').evaluate(e=>getComputedStyle(e).touchAction),'auto');
        assert.equal(await page.evaluate(()=>document.dispatchEvent(new Event('gesturestart',{cancelable:true}))),true,'desktop zoom stays available');
        await page.setViewportSize({width:760,height:844});
        await page.waitForFunction(()=>document.querySelector('meta[name="viewport"]').content.includes('maximum-scale'));
        for(const field of await page.locator('input,select,textarea').all())if(await field.isVisible()){
          const picker=['automation-model','automation-effort'].includes(await field.getAttribute('id'));
          if(picker)assert.equal(await field.evaluate(e=>getComputedStyle(e).fontSize),'13px','composer pickers use compact mobile text');
          else assert(await field.evaluate(e=>parseFloat(getComputedStyle(e).fontSize)>=16),'other visible mobile form fields retain focus-zoom protection');
          await field.focus();assert.equal(await page.evaluate(()=>visualViewport.scale),scale,'focusing mobile fields preserves the fixed viewport');
        }
        if(process.env.CODEXIFY_MOBILE_NAV_SCREENSHOTS){mkdirSync(process.env.CODEXIFY_MOBILE_NAV_SCREENSHOTS,{recursive:true});await input.focus();await page.screenshot({path:`${process.env.CODEXIFY_MOBILE_NAV_SCREENSHOTS}/${name.toLowerCase()}-composer-${awaiting?'red':'green'}.png`});}
        assert.deepEqual(errors,[]);await page.close();
      }
    }finally{await browser.close();}
  });
}
