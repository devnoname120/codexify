import assert from 'node:assert/strict';
import test from 'node:test';
import {createRequire} from 'node:module';
import {ownerHtml,routeOwner} from './owner-chat-source.mjs';
const {chromium,webkit}=createRequire(import.meta.url)('playwright');
const origin='http://127.0.0.1:43277',id='f'.repeat(64),META='io.github.devnoname120/codexify/markdown-chat';
const queued={phase:'queued',label:'Queued to wake',description:'Message queued to wake the agent; submission to ChatGPT has not been confirmed yet.'};
const sent={phase:'waiting',label:'Sent · waiting for agent',description:"Message sent to ChatGPT; waiting for the agent's first tool call."};

for(const [name,engine] of [['Chromium',chromium],['WebKit',webkit]])for(const mode of ['queue','steer']) {
 test(`${name}: offline ${mode} messages show real wakeup phases until first activity`,{timeout:30000},async()=>{
  const browser=await engine.launch();
  try {
   const page=await browser.newPage({viewport:{width:390,height:844}});page.setDefaultTimeout(5000);await page.clock.install();
   const errors=[],messages=[];let now=Date.now(),lastCall=null,wake=null,queue=[],action={status:'idle'},holdPoll=false,releasePoll,notifyPoll,releaseSend;
   page.on('pageerror',error=>errors.push(error.message));
   const goal={revision:1,status:'active',markdown:'Keep working',turnCount:1};
   const metadata=()=>({chat_file:'/fixture/CHAT.md',revision:String(messages.length*100),delivered_through:0,read_through:0,total_tool_calls:lastCall?3:2,last_agent_call_at_ms:lastCall,server_time_ms:now,messages:[...messages],queued_messages:[...queue],default_message_mode:mode,wake_status:wake,has_more:false});
   await routeOwner(page,origin+'/**',async route=>{
    const request=route.request(),path=new URL(request.url()).pathname;
    if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
    if(path==='/api/settings/connector')return route.fulfill({json:{configured:true,connectorName:'Fixture connector'}});
    if(path==='/api/global')return route.fulfill({json:{version:'2.3.12',latest:{updateAvailable:false},settingsAvailable:false,restartAvailable:false}});
    if(path==='/api/analytics/extras')return route.fulfill({status:503});
    if(path==='/api/chats')return route.fulfill({json:{chats:[{id,title:'Waking conversation',workspace:'repo',lastEntryEnd:messages.length*100,lastEntryAtMs:now,lastAgentCallAtMs:lastCall,totalToolCalls:2,goalStatus:'active',goal,automationWake:wake,automationError:action.lastError||null}],automationEnabled:true,serverTimeMs:now}});
    if(path.endsWith('/automation'))return route.fulfill({json:{enabled:true,goal,action,pairing:{status:'verified'},selection:{model:'gpt-6-astra',effort:'max'},catalog:{models:[{id:'gpt-6-astra',label:'GPT-6 Astra',efforts:[{id:'max',label:'Pro',available:true}]}]},companion:{authenticated:true,status:'idle'},presence:{lastAgentCallAtMs:lastCall},serverTimeMs:now}});
    if(path.endsWith('/send')){
     const body=request.postDataJSON();assert.equal(body.message_mode,mode);
     await new Promise(resolve=>{releaseSend=resolve;});
     wake=queued;
     if(mode==='queue') {
      const item={id:body.request_id,markdown:body.message,attachments:[],created_at_ms:now,revision:1};queue=[item];
      return route.fulfill({json:{_meta:{[META]:{queue_receipt:{queued:item,sent:null,discarded:false},queued_messages:queue,wake_status:wake}}}});
     }
     messages.push({id:body.request_id,role:'user',markdown:body.message,start:0,end:100,created_at_ms:now,tool_call_count:2});
     return route.fulfill({json:{_meta:{[META]:{sent:{id:body.request_id,end:100,created_at_ms:now},wake_status:wake}}}});
    }
    if(path==='/api/chats/'+id){
     const value=metadata();
     if(holdPoll){holdPoll=false;await new Promise(resolve=>{releasePoll=resolve;notifyPoll?.();});}
     return route.fulfill({json:{_meta:{[META]:value}}});
    }
    return route.fulfill({json:{ownerReadThrough:100}});
   });
   await page.goto(`${origin}/?chat=${id}#test-token`);
   const header=page.locator('#chat-host #presence'),label=header.locator('.presence-label'),spinner=header.locator('.wake-spinner');
   await header.locator('.presence-label').filter({hasText:'offline'}).waitFor();
   const draft=page.locator('#chat-host #draft');await draft.waitFor();
   const held=new Promise(resolve=>{notifyPoll=resolve;});holdPoll=true;now+=2200;await page.clock.fastForward(2200);await held;
   await draft.fill('Please continue working');await draft.press('Enter');
   await label.filter({hasText:'Saving message'}).waitFor();await spinner.waitFor();
   assert.equal(await spinner.evaluate(node=>getComputedStyle(node).animationDuration),'2s');
   assert.equal(await spinner.evaluate(node=>getComputedStyle(node).stroke),'rgb(133, 137, 141)');
   releaseSend();await label.filter({hasText:'Queued to wake'}).waitFor();
   releasePoll();await page.waitForTimeout(80);
   assert.equal(await label.textContent(),'Queued to wake','the pre-send poll cannot erase the wake indicator');
   const sidebar=page.locator(`.conversation-row[data-chat-id="${id}"] .presence`);
   await sidebar.locator('.presence-spinner').waitFor({state:'attached'});assert.equal(await sidebar.getAttribute('class'),'presence waking');
   const tick=async()=>{now+=2500;await page.clock.fastForward(2500);};
   wake={phase:'sending',label:'Sending to ChatGPT',description:'Opening the conversation and submitting the wake-up message to ChatGPT.'};action={id:'wake',kind:'offline_goal',status:'sending'};
   await tick();await label.filter({hasText:'Sending to ChatGPT'}).waitFor();
   wake=sent;action={id:'wake',kind:'offline_goal',status:'idle',sendAccepted:true};
   await tick();await label.filter({hasText:'Sent · waiting for agent'}).waitFor();assert.match(await header.getAttribute('title'),/Message sent to ChatGPT/);
   await page.screenshot({path:`/tmp/chat-wakeup-${name.toLowerCase()}-${mode}.png`});
   await page.reload();await label.filter({hasText:'Sent · waiting for agent'}).waitFor();
   lastCall=now;wake=null;await tick();await label.filter({hasText:'online'}).waitFor();assert.equal(await spinner.count(),0);
   await page.clock.fastForward(301000);now+=301000;await tick();await label.filter({hasText:'offline'}).waitFor();assert.equal(await spinner.count(),0,'the old accepted Send must not restart the spinner later');
   wake=queued;await tick();await label.filter({hasText:'Queued to wake'}).waitFor();
   wake=null;action={id:'failed',kind:'offline_goal',status:'failed',lastError:'Could not submit message',sendAccepted:false,retryMode:'retry'};
   await tick();await label.filter({hasText:'offline'}).waitFor();assert.equal(await spinner.count(),0);
   assert.deepEqual(errors,[]);
  } finally {await browser.close();}
 });
}

for(const [name,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: configured offline timeout agrees in the sidebar and conversation header`,{timeout:20000},async()=>{
 const browser=await engine.launch();
 try {
  const page=await browser.newPage();page.setDefaultTimeout(5000);
  let now=Date.now(),lastCall=now-6*60000,offlineAfter=600000;const errors=[],sends=[];
  await page.clock.install({time:now});page.on('pageerror',error=>errors.push(error.message));
  await routeOwner(page,origin+'/**',route=>{
   const request=route.request(),path=new URL(request.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return route.fulfill({json:{version:'fixture',settingsAvailable:false}});
   if(path==='/api/analytics/extras')return route.fulfill({status:503});
   if(path==='/api/chats')return route.fulfill({json:{chats:[{id,title:'Slow agent',workspace:'repo',lastEntryAtMs:now,lastAgentCallAtMs:lastCall,totalToolCalls:1}],offlineAfterMs:offlineAfter,automationEnabled:false,serverTimeMs:now}});
   if(path==='/api/chats/'+id)return route.fulfill({json:{_meta:{[META]:{chat_file:'/fixture/CHAT.md',revision:'1',delivered_through:0,read_through:0,total_tool_calls:1,last_agent_call_at_ms:lastCall,offline_after_ms:offlineAfter,server_time_ms:now,messages:[],queued_messages:[],has_more:false}}}});
   if(path.endsWith('/send'))sends.push(request.postDataJSON());
   return route.fulfill({json:{enabled:false,ownerReadThrough:0}});
  });
  await page.goto(`${origin}/?chat=${id}#test-token`);
  const header=page.locator('#chat-host #presence .presence-label'),sidebar=page.locator('#conversations .presence');
  await header.filter({hasText:'last seen 6 mins ago'}).waitFor();assert(await sidebar.evaluate(node=>node.classList.contains('away')));
  now+=239999;await page.clock.fastForward(239999);
  await header.filter({hasText:'last seen 9 mins ago'}).waitFor();assert(await sidebar.evaluate(node=>node.classList.contains('away')));
  now+=1;await page.clock.fastForward(1);
  await header.filter({hasText:/^offline$/}).waitFor();assert(await sidebar.evaluate(node=>node.classList.contains('offline')));
  // Timeouts shorter than the normal three-minute "online" period must still
  // go offline at their configured boundary, rather than waiting three minutes.
  offlineAfter=60000;lastCall=now-60000;
  await page.reload();await header.filter({hasText:/^offline$/}).waitFor();assert(await sidebar.evaluate(node=>node.classList.contains('offline')));
  assert.deepEqual(errors,[]);assert.deepEqual(sends,[]);
 }finally{await browser.close();}
});
