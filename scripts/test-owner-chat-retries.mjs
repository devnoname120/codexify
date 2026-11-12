import assert from 'node:assert/strict';
import test from 'node:test';
import {mkdirSync} from 'node:fs';
import {createRequire} from 'node:module';
import {ownerHtml,routeOwner} from './owner-chat-source.mjs';
const {chromium,webkit}=createRequire(import.meta.url)('playwright');
const origin='http://127.0.0.1:43357',META='io.github.devnoname120/codexify/markdown-chat';
for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: saved conversation cooldowns count down locally across polls, reopening and expiry`,{timeout:30000},async()=>{
 const browser=await engine.launch(),page=await browser.newPage({viewport:{width:1280,height:820}});page.setDefaultTimeout(5000);
 const id='9'.repeat(64),base=Date.now(),errors=[],writes=[];
 let serverNow=base,action,hold=false,pending=[],notifyHeld=null;
 const goal={revision:1,status:'active',markdown:'Keep this goal',turnCount:1};
 const release=async()=>{const queued=pending;pending=[];await Promise.all(queued.map(resolve=>resolve()));};
 try{
  // Browser and server clocks differ by an hour; only elapsed time matters.
  await page.clock.install({time:base+3600000});page.on('pageerror',error=>errors.push(error.message));
  await routeOwner(page,origin+'/**',async route=>{
   const req=route.request(),path=new URL(req.url()).pathname;
   if(req.method()!=='GET')writes.push(path);
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return route.fulfill({json:{version:'2.3.9',settingsAvailable:false}});
   if(path==='/api/analytics/extras')return route.fulfill({status:503});
   if(path==='/api/chats')return route.fulfill({json:{chats:[{id,title:'Cooling down',workspace:'test',goal,lastEntryAtMs:base,lastEntryEnd:0,totalToolCalls:0}],serverTimeMs:serverNow}});
   if(path.endsWith('/automation')){
    if(hold)await new Promise(resolve=>{pending.push(resolve);notifyHeld?.();notifyHeld=null;});
    return route.fulfill({json:{enabled:true,goal,action,pairing:{status:'verified'},catalog:{models:[]},companion:{authenticated:true,status:'idle'},presence:{},serverTimeMs:serverNow}});
   }
   return route.fulfill({json:{_meta:{[META]:{chat_file:'/fixture/CHAT.md',revision:'0',delivered_through:0,read_through:0,total_tool_calls:0,server_time_ms:serverNow,messages:[],has_more:false}}}});
  });
  const open=()=>page.goto(`${origin}/?chat=${id}#retry-test-token`);
  const error=page.locator('#chat-host #automation-error');
  const advance=async ms=>{serverNow+=ms;await page.clock.fastForward(ms);};
  for(const [status,seconds,prefix]of [
   ['rate_limited',70,'ChatGPT conversation loading is cooling down. Another conversation can load in '],
   ['conversation_unavailable',300,'ChatGPT conversation temporarily not accessible. Shared browser cooldown: conversation loading can retry in '],
   ['rate_limited',3600,'ChatGPT conversation loading is cooling down. Another conversation can load in '],
  ]){
   action={id:status,kind:'offline_goal',status,lastError:`${prefix}${seconds}s`,updatedAtMs:serverNow,sendAccepted:false,retryMode:'retry',automaticRetryAtMs:null};
   await open();await error.getByText(new RegExp(`${seconds}s`)).waitFor();
   assert.equal(await page.locator('#chat-host #goal-retry').isVisible(),false,'manual Retry stays out of the goal bar');
   assert.equal(await page.locator('#chat-host #automation-retry-inline').isVisible(),true);
   assert.doesNotMatch(await error.textContent(),/Wait for ChatGPT's limit to reset/,'local throttling is not a provider rate limit');
   await advance(1000);await error.getByText(new RegExp(`${seconds-1}s`)).waitFor();
   await advance(4000);await error.getByText(new RegExp(`${seconds-5}s`)).waitFor();
   await page.waitForLoadState('networkidle');
   const heldPoll=new Promise(resolve=>notifyHeld=resolve);
   hold=true;await advance(3000);await heldPoll;await error.getByText(new RegExp(`${seconds-8}s`)).waitFor();
   assert(pending.length>0,'a pending poll must not freeze the local countdown');
   hold=false;await release();await page.reload();await error.getByText(new RegExp(`${seconds-8}s`)).waitFor();
   await advance(seconds*1000);await error.getByText(/Use Retry to try again/).waitFor();
   assert.doesNotMatch(await error.textContent(),/cooldown|\bin \d+s/,'expired cooldowns are omitted');
   assert.doesNotMatch(await error.textContent(),/\b0s\b|\bin -\d+s/,'elapsed cooldowns do not leave a frozen zero or negative timer');
   assert.match(await error.textContent(),/Use Retry to try again/);
   await page.reload();await error.getByText(/Use Retry to try again/).waitFor();
   assert.doesNotMatch(await error.textContent(),/cooldown|\bin \d+s/);
   action={...action,id:`${status}-long-retry`,updatedAtMs:serverNow,automaticRetryAtMs:serverNow+seconds*1000+3600000};
   await open();await error.getByText(/Retrying the goal automatically in/).waitFor();
   assert.doesNotMatch(await error.textContent(),/Another conversation can load in|conversation loading can retry in|has elapsed/,'the shorter cooldown does not compete with the retry countdown');
   await advance((seconds+1)*1000);await error.getByText(/Retrying the goal automatically in/).waitFor();
   assert.doesNotMatch(await error.textContent(),/cooling down|Another conversation can load in|conversation loading can retry in|has elapsed/,'expired cooldowns stay omitted while automatic retry remains pending');
   action={...action,id:`${status}-short-retry`,updatedAtMs:serverNow,automaticRetryAtMs:serverNow+10000};
   await open();await error.getByText(new RegExp(`${seconds}s`)).waitFor();
   assert.match(await error.textContent(),/Retrying the goal automatically in 10s/,'a cooldown longer than the automatic retry is still relevant');
  }
  assert.deepEqual(writes,[],'a timer reaching zero must not submit or retry anything');assert.deepEqual(errors,[]);
 }finally{hold=false;await release();await browser.close();}
});
for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: chat switches use cached models and never request browser discovery`,{timeout:30000},async()=>{
 const browser=await engine.launch(),page=await browser.newPage({viewport:{width:1280,height:820}});page.setDefaultTimeout(5000);
 const now=Date.now(),a='d'.repeat(64),b='f'.repeat(64),errors=[],operations=[];
 const busy='Browser companion rejected the operation because its state changed';
 const goal={revision:1,status:'active',markdown:'Continue the current goal',elapsedActiveMs:60000,turnCount:1};
 const models=[{id:'gpt-6-astra',label:'GPT-6 Astra',efforts:[{id:'max',label:'Pro',available:true}]}];
 let actionFailed=false,cached=true,context='fixture-account-build';
 try{
  await page.addInitScript(()=>{window.setInterval=()=>0;});page.on('pageerror',error=>errors.push(error.message));
  await routeOwner(page,origin+'/**',route=>{
   const request=route.request(),path=new URL(request.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(request.method()!=='GET')operations.push(path);
   if(path==='/api/global')return route.fulfill({json:{version:'2.3.3',settingsAvailable:false}});
   if(path==='/api/analytics/extras')return route.fulfill({status:503});
   if(path==='/api/chats')return route.fulfill({json:{chats:[a,b].map((id,i)=>({id,title:'Conversation '+i,workspace:'model-errors',goal,goalStatus:'active',lastEntryAtMs:now-i,lastEntryEnd:0,totalToolCalls:0})),serverTimeMs:Date.now()}});
   if(path.endsWith('/automation/capabilities'))return route.fulfill({status:500,contentType:'text/plain',body:busy});
   if(path.endsWith('/automation'))return route.fulfill({json:{enabled:true,goal,action:actionFailed?{id:'genuine-failure',kind:'offline_goal',status:'failed',lastError:busy,retryMode:'retry'}:{status:'idle'},pairing:{status:'verified'},selection:{model:'gpt-6-astra',effort:'max'},catalog:cached?{models}:null,companion:{authenticated:true,status:'idle',activeActionId:null,catalogContext:context},presence:{},serverTimeMs:Date.now()}});
   return route.fulfill({json:{_meta:{[META]:{chat_file:'/fixture/CHAT.md',revision:'0',delivered_through:0,read_through:0,total_tool_calls:0,server_time_ms:Date.now(),messages:[],has_more:false}}}});
  });
  await page.goto(origin+'/#model-error-token');await page.locator('.conversation').last().waitFor();
  const host=page.locator('#chat-host');
  for(const id of [a,b,a]){
   await page.locator(`[data-chat-id="${id}"] .conversation`).click();await host.locator('#automation-model:enabled').waitFor();
   assert.equal(await host.locator('#automation-error').isVisible(),false);
   assert.equal(await host.locator('#goal-caption').textContent(),'Pursuing goal');
   assert.equal(await host.locator('#automation-model').inputValue(),'gpt-6-astra');
   assert.equal(await host.locator('#automation-effort').inputValue(),'max');
   assert.equal(await host.locator('#automation-model option:checked').textContent(),'GPT-6 Astra');
  }
  // A different account with no cache keeps saved IDs, without reusing the
  // previous account's labels or attempting discovery merely to render a chat.
  cached=false;context='other-account-build';
  await page.evaluate(()=>window.dispatchEvent(new Event('codexify-browser-change')));
  await host.locator('#automation-model option:checked').filter({hasText:/^GPT-6 Astra$/}).waitFor({state:'attached'});
  assert.equal(await host.locator('#automation-effort option:checked').textContent(),'Pro');
  assert.equal(await host.locator('#automation-model').isDisabled(),true);
  assert.equal(await host.locator('#automation-model').inputValue(),'gpt-6-astra');
  assert.match(await host.locator('#automation-model').getAttribute('title'),/Model list unavailable/);
  assert.equal(await host.locator('#automation-error').isVisible(),false);
  await page.locator(`[data-chat-id="${b}"] .conversation`).click();
  await host.locator('#automation-model option:checked').filter({hasText:/^GPT-6 Astra$/}).waitFor({state:'attached'});
  assert.equal(await host.locator('#automation-error').isVisible(),false);
  actionFailed=true;
  const refresh=page.waitForResponse(response=>new URL(response.url()).pathname===`/api/chats/${b}/automation`);
  await page.evaluate(()=>window.dispatchEvent(new Event('codexify-browser-change')));await refresh;
  await host.locator('#automation-error').getByText(new RegExp(busy)).waitFor();
  assert.equal(await host.locator('#automation-error').isVisible(),true,'a genuine action failure with the same message stays visible');
  actionFailed=false;cached=true;
  await page.evaluate(()=>window.dispatchEvent(new Event('codexify-browser-change')));
  await host.locator('#automation-model:enabled').waitFor();
  assert.equal(await host.locator('#automation-error').isVisible(),false);
  assert.doesNotMatch(await host.locator('#automation-model').getAttribute('title'),/unavailable/);
  assert.equal(await host.locator('#automation-model').inputValue(),'gpt-6-astra');
  assert.deepEqual(operations,[],'switches, polls and catalog recovery use cached state without invoking browser operations');
  assert.deepEqual(errors,[]);
 }finally{await browser.close();}
});
for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: goal backoff exposes the permitted manual action beside its countdown`,{timeout:30000},async()=>{
 const browser=await engine.launch(),page=await browser.newPage({viewport:{width:1280,height:820}});page.setDefaultTimeout(5000);
 try{
  const now=Date.now(),id='e'.repeat(64),errors=[],retries=[],sends=[];
  const goal={revision:4,status:'active',markdown:'Continue the current goal',elapsedActiveMs:16214000,turnCount:5};
  let action={id:'initial',status:'idle',retryMode:null};
  await page.clock.install({time:now});page.on('pageerror',e=>errors.push(e.message));
  await routeOwner(page,origin+'/**',route=>{
   const request=route.request(),path=new URL(request.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return route.fulfill({json:{version:'2.3.0',settingsAvailable:false}});
   if(path==='/api/settings/connector')return route.fulfill({json:{configured:true,connectorName:'Fixture connector'}});
   if(path==='/api/analytics/extras')return route.fulfill({status:503});
   if(path==='/api/chats')return route.fulfill({json:{chats:[{id,title:'Retrying goal',workspace:'test',goal,goalStatus:'active',lastEntryEnd:0,totalToolCalls:0}],serverTimeMs:Date.now()}});
   if(path.endsWith('/automation'))return route.fulfill({json:{enabled:true,goal,action,pairing:{status:'verified'},catalog:{models:[]},companion:{authenticated:true,status:'busy'},presence:{},serverTimeMs:Date.now()}});
   if(path.endsWith('/action/retry')){
    const body=request.postDataJSON();assert.equal(body.actionId,action.id);retries.push(body);
    action={...action,status:'queued',retryMode:null,lastError:null,automaticRetryAtMs:null};
    return route.fulfill({json:{action}});
   }
   if(path.endsWith('/send'))sends.push(request.postDataJSON());
   return route.fulfill({json:{_meta:{[META]:{chat_file:'/fixture/CHAT.md',revision:'0',delivered_through:0,read_through:0,total_tool_calls:0,server_time_ms:Date.now(),messages:[],has_more:false}}}});
  });
  await page.goto(`${origin}/?chat=${id}#retry-test-token`);
  const host=page.locator('#chat-host'),error=host.locator('#automation-error'),button=host.locator('#automation-retry-inline');
  for(const width of [1280,390]){
   await page.setViewportSize({width,height:844});
   for(const [failure,mode,accepted]of [['conversation_unavailable','retry',false],['authentication_required','retry',false],['rate_limited','retry',false],['failed','retry',false],['ambiguous','continue_goal',true]]){
    action={id:`${failure}-${width}`,kind:'offline_goal',status:failure,goalRevision:4,lastError:failure==='conversation_unavailable'?'ChatGPT conversation temporarily not accessible':'Browser action needs recovery',sendAccepted:accepted,messagesFenced:accepted,retryMode:mode,automaticRetryAtMs:Date.now()+180000,automaticRetryAttempts:0,automaticRetryLimit:6};
    await page.clock.fastForward(1600);
    const verb=mode==='continue_goal'?'continue':'retry';
    await error.getByText(new RegExp(`You can also ${verb} now\\.`)).waitFor();
    await button.waitFor();assert.equal(await button.isEnabled(),true);
    assert.equal(await button.textContent(),mode==='continue_goal'?'Continue':'Retry');
    assert.equal(await host.locator('#goal-retry').isVisible(),false,'the countdown action is beside the error');
    const box=await button.boundingBox();assert(box.x>=0&&box.x+box.width<=width,'the retry button stays within the viewport');
    if(process.env.RETRY_BUTTON_SCREENSHOTS&&failure==='conversation_unavailable'){
     mkdirSync(process.env.RETRY_BUTTON_SCREENSHOTS,{recursive:true});await page.screenshot({path:`${process.env.RETRY_BUTTON_SCREENSHOTS}/${name.toLowerCase()}-${width}.png`});
    }
    const count=retries.length;await button.click();await button.waitFor({state:'hidden'});
    assert.equal(retries.length,count+1,'one explicit click retries the current action once');
   }
   action={id:`exhausted-cooldown-${width}`,kind:'offline_goal',status:'rate_limited',lastError:'ChatGPT conversation loading is cooling down. Another conversation can load in 70s',updatedAtMs:Date.now(),sendAccepted:false,retryMode:'retry',automaticRetryAtMs:null,automaticRetryAttempts:6,automaticRetryLimit:6};
   await page.clock.fastForward(1600);await error.getByText(/All 6 automatic retries failed/).waitFor();
   assert.equal(await host.locator('#goal-retry').isVisible(),false);assert.equal(await button.isVisible(),true);
   action={id:`cancelled-before-send-${width}`,kind:'offline_goal',status:'cancelled',lastError:'Browser action cancelled before Send',sendAccepted:false,retryMode:'retry',automaticRetryAtMs:null};
   await page.clock.fastForward(1600);await error.getByText(/Browser action cancelled before Send/).waitFor();
   await button.waitFor();await button.click();await button.waitFor({state:'hidden'});
   action={id:`no-manual-retry-${width}`,kind:'turn_renewal',status:'ambiguous',lastError:'Handoff confirmation unavailable',sendAccepted:true,retryMode:null,automaticRetryAtMs:Date.now()+60000};
   await page.clock.fastForward(1600);await error.getByText(/Handoff confirmation unavailable/).waitFor();
   assert.match(await error.textContent(),/Retrying automatically/);
   assert.doesNotMatch(await error.textContent(),/You can also (continue|retry) now/,'do not advertise a manual action the server has not permitted');
   assert.equal(await button.isVisible(),false);
  }
  assert.equal(retries.length,12);assert.deepEqual(sends,[],'Continue must use the retry API rather than resend a user message');assert.deepEqual(errors,[]);
 }finally{await browser.close();}
});
for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: sidebar recovery spins red through backoff and execution, then recovers or stops on terminal failure`,{timeout:30000},async()=>{
 const browser=await engine.launch(),page=await browser.newPage({viewport:{width:1280,height:820}});page.setDefaultTimeout(5000);
 try{
  const now=Date.now(),errors=[],writes=[],chats=[
   {id:'a'.repeat(64),title:'Healthy conversation',goalStatus:null,lastAgentCallAtMs:now},
   {id:'b'.repeat(64),title:'Retrying conversation',goalStatus:null,lastAgentCallAtMs:null,automationError:'Browser operation timed out',automationRetrying:true},
   {id:'c'.repeat(64),title:'Retrying objective',goalStatus:'active',lastAgentCallAtMs:null,automationError:'Browser operation timed out',automationRetrying:true}
  ].map((chat,i)=>({workspace:'retry-icons',lastEntryAtMs:now-i,lastEntryEnd:0,totalToolCalls:12,...chat}));
  await page.clock.install({time:now});page.on('pageerror',e=>errors.push(e.message));
  await routeOwner(page,origin+'/**',route=>{
   const req=route.request(),path=new URL(req.url()).pathname;
   if(req.method()!=='GET')writes.push(path);
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   assert.equal(req.headers().authorization,'Bearer retry-test-token');
   if(path==='/api/global')return route.fulfill({json:{version:'2.2.1',plan:null,settingsAvailable:false}});
   if(path==='/api/chats')return route.fulfill({json:{chats,serverTimeMs:Date.now()}});
   if(path.endsWith('/automation'))return route.fulfill({json:{enabled:false}});
   if(path===`/api/chats/${chats[0].id}`)return route.fulfill({json:{_meta:{[META]:{chat_file:'/fixture/CHAT.md',revision:'0',delivered_through:0,read_through:0,last_agent_call_at_ms:now,total_tool_calls:12,server_time_ms:Date.now(),messages:[],has_more:false}}}});
   return route.fulfill({status:404});
  });
  await page.goto(`${origin}/?chat=${chats[0].id}#retry-test-token`);
  const icon=i=>page.locator(`.conversation-row[data-chat-id="${chats[i].id}"] .presence`),circle=icon(1).locator('svg'),ring=icon(2).locator('.goal-outer-ring');
  await icon(1).filter({has:page.locator('.presence-spinner')}).waitFor();await ring.waitFor();
  const checkRed=async()=>{
   assert.equal(await icon(1).getAttribute('class'),'presence retrying');assert.equal(await icon(2).getAttribute('class'),'presence goal-retrying');
   for(const i of [1,2]){assert.equal(await icon(i).evaluate(n=>getComputedStyle(n).color),'rgb(196, 49, 59)');assert.match(await icon(i).getAttribute('aria-label'),/retrying automatically/);assert(await icon(i).isVisible());}
   for(const spin of [circle,ring]){assert.equal(await spin.evaluate(n=>getComputedStyle(n).animationName),'presence-spin');assert.equal(await spin.evaluate(n=>getComputedStyle(n).animationIterationCount),'infinite');}
   assert.equal(await icon(2).locator('path:not(.goal-outer-ring)').evaluate(n=>getComputedStyle(n).animationName),'none','the goal center stays still');
   assert.equal(await page.locator('.presence.error').count(),0);
  };
  await checkRed();assert.equal(await icon(0).evaluate(n=>getComputedStyle(n).color),'rgb(21, 155, 39)');
  for(const spin of [circle,ring]){
   const rotations=await spin.evaluate(n=>{const a=n.getAnimations()[0];a.pause();a.currentTime=0;const before=getComputedStyle(n).transform;a.currentTime=500;const after=getComputedStyle(n).transform;a.play();return {before,after};});
   assert.notEqual(rotations.before,rotations.after,'red recovery icons rotate');
  }
  const out=process.env.RETRY_ICON_SCREENSHOTS;
  if(out){mkdirSync(out,{recursive:true});await page.screenshot({path:`${out}/${name.toLowerCase()}-retrying.png`,animations:'disabled'});}
  await page.emulateMedia({colorScheme:'dark'});for(const i of [1,2])assert.equal(await icon(i).evaluate(n=>getComputedStyle(n).color),'rgb(255, 133, 142)');await page.emulateMedia({colorScheme:'light'});
  // A slow queue remains recovery even after the advertised retry time passes.
  await page.clock.fastForward(60000);await checkRed();
  // The retry clears lastError while preparing and sending, but keeps its marker.
  for(const chat of chats.slice(1))chat.automationError=null;
  await page.clock.fastForward(2100);await page.waitForFunction(()=>![...document.querySelectorAll('.presence[aria-label]')].some(n=>n.getAttribute('aria-label').includes('timed out')));await checkRed();
  for(const chat of chats.slice(1)){chat.automationRetrying=false;chat.automationError='Automatic recovery exhausted; needs attention';}
  await page.clock.fastForward(2100);await icon(1).locator('path[d="M12 6v7m0 4h.01"]').waitFor();await icon(2).locator('path[d="M12 6v7m0 4h.01"]').waitFor();
  for(const i of [1,2]){assert.equal(await icon(i).getAttribute('class'),'presence error');assert.equal(await icon(i).locator('.goal-outer-ring,.presence-spinner').count(),0);assert.equal(await icon(i).locator('svg').evaluate(n=>getComputedStyle(n).animationName),'none');}
  if(out)await page.screenshot({path:`${out}/${name.toLowerCase()}-terminal.png`,animations:'disabled'});
  for(const chat of chats.slice(1)){chat.automationError=null;chat.lastAgentCallAtMs=now+65000;}
  await page.clock.fastForward(2100);await page.locator('.conversation .presence.goal-running').waitFor();assert.equal(await icon(1).getAttribute('class'),'presence online');for(const i of [1,2])assert.equal(await icon(i).evaluate(n=>getComputedStyle(n).color),'rgb(21, 155, 39)');
  for(const chat of chats.slice(1)){chat.automationRetrying=true;chat.automationError='Browser operation timed out';}
  await page.clock.fastForward(2100);await page.locator('.conversation .presence.goal-retrying').waitFor();
  await page.setViewportSize({width:390,height:844});await page.locator('#chat-host').getByRole('button',{name:'Back to chats',exact:true}).click();await checkRed();
  assert.equal(await page.locator('#layout').evaluate(n=>n.scrollWidth>n.clientWidth),false);
  if(out)await page.screenshot({path:`${out}/${name.toLowerCase()}-retrying-mobile.png`,animations:'disabled'});
  assert.deepEqual(errors,[]);assert.deepEqual(writes,[]);
 }finally{await browser.close();}
});

for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: pausing a retry retains its error and dismiss clears it across reopening`,{timeout:30000},async()=>{
 const browser=await engine.launch(),page=await browser.newPage({viewport:{width:390,height:844}});page.setDefaultTimeout(5000);
 try{
  const id='c'.repeat(64),now=Date.now(),operations=[],errors=[];
  const goal={revision:1,status:'active',markdown:'Keep this goal',turnCount:1};
  let action={id:'failure',kind:'offline_goal',status:'rate_limited',lastError:'ChatGPT conversation loading is cooling down. Another conversation can load in 70s',updatedAtMs:now,sendAccepted:false,retryMode:'retry',automaticRetriesEnabled:true,canPauseRetries:true,automaticRetryAtMs:now+180000,automaticRetryAttempts:0,automaticRetryLimit:6};
  await page.clock.install({time:now});page.on('pageerror',e=>errors.push(e.message));
  await routeOwner(page,origin+'/**',route=>{
   const req=route.request(),path=new URL(req.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return route.fulfill({json:{version:'2.3.3',settingsAvailable:false}});
   if(path==='/api/analytics/extras')return route.fulfill({status:503});
   if(path==='/api/chats')return route.fulfill({json:{chats:[{id,title:'Paused retries',workspace:'test',goal,lastEntryAtMs:now,lastEntryEnd:0,totalToolCalls:0,automationError:action.lastError}],serverTimeMs:Date.now()}});
   if(path.endsWith('/automation'))return route.fulfill({json:{enabled:true,goal,action,pairing:{status:'verified'},catalog:{models:[]},companion:{authenticated:true,status:'idle'},presence:{},serverTimeMs:Date.now()}});
   if(path.endsWith('/action/pause-retries')||path.endsWith('/action/cancel')){
    const body=req.postDataJSON();assert.equal(body.actionId,action.id);operations.push(path);
    action={...action,automaticRetryPaused:true,canPauseRetries:false,automaticRetryAtMs:null};
    if(path.endsWith('/cancel'))action={...action,status:'cancelled',lastError:null,retryMode:null};
    return route.fulfill({json:{action}});
   }
   return route.fulfill({json:{_meta:{[META]:{chat_file:'/fixture/CHAT.md',revision:'0',delivered_through:0,read_through:0,total_tool_calls:0,server_time_ms:Date.now(),messages:[],has_more:false}}}});
  });
  const url=`${origin}/?chat=${id}#retry-test-token`;
  await page.goto(url);
  let host=page.locator('#chat-host'),error=host.locator('#automation-error'),pause=host.locator('#automation-pause-retries');
  await pause.waitFor();await error.getByText(/Retrying the goal automatically/).waitFor();
  for(const button of [pause,host.locator('#automation-retry-inline'),host.locator('#automation-dismiss-inline')]){
   const box=await button.boundingBox();assert(box.x>=0&&box.x+box.width<=390,'retry controls stay in the mobile viewport');
  }
  await pause.click();await error.getByText(/Automatic retries paused/).waitFor();assert.equal(await pause.isVisible(),false);
  assert.equal(await host.locator('#goal-retry').isVisible(),false);
  assert.equal(await host.locator('#automation-retry-inline').isVisible(),true);
  if(process.env.RETRY_BUTTON_SCREENSHOTS){mkdirSync(process.env.RETRY_BUTTON_SCREENSHOTS,{recursive:true});await page.screenshot({path:`${process.env.RETRY_BUTTON_SCREENSHOTS}/${name.toLowerCase()}-paused.png`});}
  await page.reload();host=page.locator('#chat-host');error=host.locator('#automation-error');
  await error.getByText(/Automatic retries paused/).waitFor();
  await host.locator('#automation-dismiss-inline').click();await error.waitFor({state:'hidden'});
  await page.reload();host=page.locator('#chat-host');await host.locator('#goal-text').getByText(goal.markdown).waitFor();
  for(const width of [390,320]){
   await page.setViewportSize({width,height:844});
   assert(await host.locator('#goal-text').isVisible(),'retry dismissal preserves a visible goal on narrow screens');
   const text=await host.locator('#goal-text').boundingBox();assert(text.width>=30,'goal metrics leave room for the goal text');
   const metrics=await host.locator('#goal-metrics').boundingBox();assert(metrics.y>=text.y+text.height,'mobile metrics wrap beneath the goal');
  }
  await page.clock.fastForward(2000);assert.equal(await host.locator('#automation-error').isVisible(),false);
  assert.equal(goal.status,'active');assert.equal(action.sendAccepted,false);assert.equal(operations.length,2);assert.deepEqual(errors,[]);
 }finally{await browser.close();}
});

for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: queued browser cooldown is a neutral live wait with one timer and manual override`,{timeout:30000},async()=>{
 const browser=await engine.launch(),page=await browser.newPage({viewport:{width:1280,height:820}});page.setDefaultTimeout(5000);
 const id='c'.repeat(64),base=Date.now(),writes=[],errors=[];
 let serverNow=base;
 const goal={revision:1,status:'active',markdown:'Keep working',turnCount:1};
 let action={id:'deferred-action',kind:'offline_goal',status:'queued',cooldownUntilMs:base+164000,sendAccepted:false,lastError:null,retryMode:'retry',automaticRetryAtMs:null,automaticRetryAttempts:0,automaticRetriesEnabled:false,canPauseRetries:false};
 let queue={position:2,total:3};
 try{
  await page.clock.install({time:base+3600000});page.on('pageerror',error=>errors.push(error.message));
  await routeOwner(page,origin+'/**',async route=>{
   const req=route.request(),path=new URL(req.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return route.fulfill({json:{version:'2.3.10',settingsAvailable:false}});
   if(path==='/api/analytics/extras')return route.fulfill({status:503});
   if(path==='/api/chats')return route.fulfill({json:{chats:[{id,title:'Waiting chat',workspace:'test',goal,lastEntryAtMs:base,lastEntryEnd:0,totalToolCalls:0}],serverTimeMs:serverNow}});
   if(req.method()!=='GET'){
    writes.push(path);action={status:'idle'};
    return route.fulfill({json:{}});
   }
   if(path.endsWith('/automation'))return route.fulfill({json:{enabled:true,goal,action,queue,pairing:{status:'verified'},catalog:{models:[]},companion:{authenticated:true,status:'idle'},presence:{},serverTimeMs:serverNow}});
   return route.fulfill({json:{_meta:{[META]:{chat_file:'/fixture/CHAT.md',revision:'0',delivered_through:0,read_through:0,total_tool_calls:0,server_time_ms:serverNow,messages:[],has_more:false}}}});
  });
  await page.goto(`${origin}/?chat=${id}#queued-wait-token`);
  const host=page.locator('#chat-host'),notice=host.locator('#automation-error');
  await notice.getByText(/available in 164s/).waitFor();
  const doNow=host.getByRole('button',{name:'Do now',exact:true});
  assert.equal(await doNow.textContent(),'Do now');
  assert.match(await doNow.getAttribute('title'),/shared browser cooldown/);
  assert.match(await notice.textContent(),/^Conversation loading available in 164s \(position in queue: 2\)\./);
  for(const width of [320,390,600,1280]){
   await page.setViewportSize({width,height:820});
   const layout=await notice.evaluate(node=>{
    const box=node.getBoundingClientRect(),text=document.createRange();text.selectNodeContents(node);
    const lines=[...text.getClientRects()].map(line=>({left:line.left,right:line.right,top:line.top,bottom:line.bottom}));
    const retry=node.getRootNode().getElementById('automation-retry-inline'),button=retry.getBoundingClientRect();
    const label=document.createRange();label.selectNodeContents(retry);
    return {box:{left:box.left,right:box.right,top:box.top,bottom:box.bottom},lines,overflow:node.scrollWidth>node.clientWidth,buttonTop:button.top,labelTop:label.getBoundingClientRect().top};
   });
   assert.equal(layout.overflow,false,'the countdown must wrap instead of overflowing');
   assert(layout.lines.every(line=>line.left>=layout.box.left-1&&line.right<=layout.box.right+1&&line.bottom<=layout.box.bottom+1),'all notice text stays visible');
   if(width<=600){
    assert(layout.buttonTop-layout.box.bottom<=3,'actions sit directly below the full notice');
    assert(layout.labelTop-layout.lines.at(-1).bottom<=12,'text-to-action spacing stays compact');
   }
   if(width===390)await page.screenshot({path:`/tmp/browser-wait-layout-${name.toLowerCase()}.png`});
  }
  assert.equal(await notice.getAttribute('role'),'status');
  assert.match(await notice.getAttribute('class'),/waiting-for-browser/);
  assert.doesNotMatch(await notice.textContent(),/retry \d|Retrying|failed|disabled|not sent/);
  assert.equal(await host.locator('#goal-caption').textContent(),'Pursuing goal');
  assert.equal(await host.locator('#goal-retry').isVisible(),false);
  assert.equal(await host.locator('#automation-pause-retries').isVisible(),false,'an initial queued action is not an automatic error retry');
  assert.equal(await host.locator('#automation-retry-inline').isVisible(),true);
  serverNow+=1000;await page.clock.fastForward(1000);await notice.getByText(/available in 163s/).waitFor();
  await page.reload();await notice.getByText(/available in 163s/).waitFor();
  assert.match(await notice.textContent(),/position in queue: 2/,'reopening preserves the reported queue position');
  queue={position:1,total:2};await page.clock.fastForward(2000);await notice.getByText(/position in queue: 1/).waitFor();
  assert.doesNotMatch(await notice.textContent(),/position in queue: 2/,'the position updates as earlier conversations leave the queue');
  queue=null;await page.clock.fastForward(2000);await notice.getByText(/^Conversation loading available in/).waitFor();
  assert.doesNotMatch(await notice.textContent(),/position in queue/,'an unknown position is not invented');
  queue={position:1,total:1};await page.clock.fastForward(2000);await notice.getByText(/position in queue: 1/).waitFor();
  serverNow+=163000;await page.clock.fastForward(163000);await notice.getByText('Waiting for the shared browser (position in queue: 1).',{exact:true}).waitFor();
  assert.doesNotMatch(await notice.textContent(),/elapsed|\b0s\b|retry \d|…|\.\.\./);
  assert.deepEqual(writes,[],'waiting for the clock never posts a browser action from the UI');
  await doNow.click();
  await notice.waitFor({state:'hidden'});
  assert.deepEqual(writes,[`/api/chats/${id}/automation/action/retry`]);
  action={id:'actual-failure',kind:'offline_goal',status:'failed',lastError:'Browser action failed',sendAccepted:false,retryMode:'retry'};
  await page.clock.fastForward(2000);await notice.getByText(/Browser action failed/).waitFor();
  assert.equal(await host.getByRole('button',{name:'Retry now',exact:true}).textContent(),'Retry','actual failures retain their retry label');
  assert.deepEqual(errors,[]);
 }finally{await browser.close();}
});
