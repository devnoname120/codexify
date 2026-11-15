import assert from 'node:assert/strict';
import test from 'node:test';
import {mkdirSync} from 'node:fs';
import {createRequire} from 'node:module';
import {ownerHtml,routeOwner} from './owner-chat-source.mjs';
const {chromium,webkit}=createRequire(import.meta.url)('playwright');
const origin='http://127.0.0.1:43359',META='io.github.devnoname120/codexify/markdown-chat';
for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: goal bars render synchronously from the chat list and reconcile without leaking across chats`,{timeout:20000},async()=>{
 const browser=await engine.launch(),page=await browser.newPage({viewport:{width:1280,height:900}});page.setDefaultTimeout(5000);
 const pending=[];let readReady,enabled=true,failedChats=0;const now=Date.now(),a='a'.repeat(64),b='b'.repeat(64),errors=[],writes=[];
 const initial={revision:1,status:'active',markdown:'Continue the investigation.',createdAtMs:now,updatedAtMs:now,elapsedActiveMs:60000,activeSinceMs:null,timerUntilMs:null,turnCount:3};let serverGoal=initial;
 const chats=[{id:a,title:'Goal chat',goalStatus:'active',goal:initial},{id:b,title:'No goal',goalStatus:null,goal:null}].map((c,i)=>({...c,workspace:'instant-goals',lastAgentCallAtMs:now,lastEntryAtMs:now-i,lastEntryEnd:0,totalToolCalls:12}));
 try{
  await page.addInitScript(()=>{window.setInterval=()=>0;});page.on('pageerror',e=>errors.push(e.message));
  await routeOwner(page,origin+'/**',async route=>{
   const req=route.request(),path=new URL(req.url()).pathname;if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   assert.equal(req.headers().authorization,'Bearer goal-preview-token');if(req.method()!=='GET')writes.push(path);
   if(path==='/api/global')return route.fulfill({json:{version:'2.2.1',settingsAvailable:false}});
   if(path==='/api/chats')return route.fulfill({json:{chats,automationEnabled:enabled,serverTimeMs:Date.now()}});
   if(path.endsWith('/automation')){
    const id=path.split('/')[3],goal=id===a?serverGoal:null,active=enabled,recoveryCount=failedChats;
    if(id===a){let release;const wait=new Promise(resolve=>release=resolve),entry={release,goal};pending.push(entry);readReady?.(entry);readReady=null;await wait;}
    return route.fulfill({json:{enabled:active,goal,conversationRecovery:{failedChatCount:recoveryCount},pairing:{status:'verified'},action:{status:'idle'},selection:{},catalog:{models:[]},companion:{authenticated:true,status:'idle'},serverTimeMs:Date.now()}});
   }
   if(path===`/api/chats/${a}`||path===`/api/chats/${b}`)return route.fulfill({json:{_meta:{[META]:{chat_file:'/fixture/CHAT.md',revision:'0',delivered_through:0,read_through:0,last_agent_call_at_ms:now,total_tool_calls:12,server_time_ms:Date.now(),messages:[],has_more:false}}}});
   return route.fulfill({status:404});
  });
  const read=()=>new Promise(resolve=>readReady=resolve);
  // Inspect the DOM in the click's own task, before any async response can arrive.
  const click=id=>page.evaluate(id=>{document.querySelector(`[data-chat-id="${id}"] .conversation`).click();const root=document.querySelector('#chat-host .chat-shell').shadowRoot,bar=root.getElementById('goal-bar');return {visible:bar.getClientRects().length>0&&getComputedStyle(bar).visibility!=='hidden',text:root.getElementById('goal-text').textContent,caption:root.getElementById('goal-caption').textContent,metrics:root.getElementById('goal-metrics').textContent,pending:root.getElementById('automation-toggle-goal').disabled};},id);
  await page.goto(origin+'/#goal-preview-token');await page.locator('.conversation').last().waitFor();
  const firstRead=read(),first=await click(a),old=await firstRead;assert.equal(first.visible,true);assert.equal(first.text,initial.markdown);assert.equal(first.caption,'Pursuing goal');assert.equal(first.metrics,'1m 0s (3 turns · 0 failed chats)');assert.equal(first.pending,true,'mutations wait for the authoritative automation state');
  const out=process.env.GOAL_PREVIEW_SCREENSHOTS;if(out){mkdirSync(out,{recursive:true});await page.screenshot({path:`${out}/${name.toLowerCase()}-instant-goal.png`,animations:'disabled'});}
  assert.equal((await click(b)).visible,false,'a different chat never inherits the previous goal');
  serverGoal={...initial,revision:2,status:'configured',markdown:'An updated goal from the server.',updatedAtMs:now+1,elapsedActiveMs:120000,turnCount:4};failedChats=2;
  const secondRead=read();assert.equal((await click(a)).text,initial.markdown);const fresh=await secondRead;fresh.release();
  const host=page.locator('#chat-host');await host.locator('#goal-text').getByText(serverGoal.markdown,{exact:true}).waitFor();assert.equal(await host.locator('#goal-caption').textContent(),'Goal paused');assert.equal(await host.locator('#goal-metrics').textContent(),'2m 0s (4 turns · 2 failed chats)');
  const late=page.waitForResponse(r=>new URL(r.url()).pathname===`/api/chats/${a}/automation`);old.release();await late;await page.waitForTimeout(30);assert.equal(await host.locator('#goal-text').textContent(),serverGoal.markdown,'a closed chat ignores its late response');assert.equal(await host.locator('#goal-metrics').textContent(),'2m 0s (4 turns · 2 failed chats)');
  await click(b);serverGoal=null;const clearingRead=read(),cached=await click(a),clearing=await clearingRead;assert.equal(cached.text,'An updated goal from the server.','fresh automation state updates the global goal snapshot');assert.equal(cached.caption,'Goal paused');clearing.release();await host.locator('#goal-bar').waitFor({state:'hidden'});
  await click(b);const absentRead=read();assert.equal((await click(a)).visible,false,'an acknowledged goal removal is cached immediately');(await absentRead).release();
  // A disabled automation configuration must not briefly show saved goals.
  enabled=false;await page.goto(origin+'/#goal-preview-token');await page.locator('.conversation').last().waitFor();const disabledRead=read();assert.equal((await click(a)).visible,false);(await disabledRead).release();
  enabled=true;serverGoal=initial;await page.setViewportSize({width:390,height:844});await page.goto(origin+'/#goal-preview-token');await page.locator('.conversation').last().waitFor();const mobileRead=read(),mobile=await click(a);assert.equal(mobile.visible,true);assert.equal(mobile.text,initial.markdown);const mobileEntry=await mobileRead;if(out)await page.screenshot({path:`${out}/${name.toLowerCase()}-instant-goal-mobile.png`,animations:'disabled'});mobileEntry.release();
  assert.deepEqual(errors,[]);assert.deepEqual(writes,[]);
 }finally{for(const entry of pending)entry.release();await browser.close();}
});
