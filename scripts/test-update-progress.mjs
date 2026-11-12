import assert from 'node:assert/strict';
import test from 'node:test';
import {createRequire} from 'node:module';
import {mkdirSync} from 'node:fs';
import {setTimeout as delay} from 'node:timers/promises';
import {ownerHtml,routeOwner} from './owner-chat-source.mjs';
const {chromium,webkit}=createRequire(import.meta.url)('playwright');
const META='io.github.devnoname120/codexify/markdown-chat';
const origin='http://127.0.0.1:43266',id='d'.repeat(64);

async function fixture(browser,{online=false}={}) {
 const page=await browser.newPage({viewport:{width:390,height:844}});page.setDefaultTimeout(5000);await page.clock.install();
 const now=Date.now(),errors=[],requests=[],sends=[],reads={health:0},transport={down:false};
 const value={version:'2.3.0',instanceId:'100',latest:{updateAvailable:true},readiness:{ready:true,loading:false,phase:'ready'},connectorRefresh:null,update:null,settingsAvailable:true,restartAvailable:true};
 let releaseUpdate;const held=new Promise(resolve=>{releaseUpdate=resolve;});
 page.on('pageerror',error=>errors.push(error.message));
 await routeOwner(page,origin+'/**',async route=>{
  const request=route.request(),path=new URL(request.url()).pathname;
  if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
  if(transport.down&&path.startsWith('/api/'))return route.fulfill({status:503,body:'Service starting'});
  if(path==='/api/global')return route.fulfill({json:value});
  if(path==='/api/health'){reads.health++;return route.fulfill({json:{status:'ok',readiness:value.readiness}});}
  if(path==='/api/chats')return route.fulfill({json:{chats:[{id,title:'Update feedback',workspace:'fixture',lastEntryEnd:0,lastIncomingEntryEnd:0,lastEntryAtMs:now,lastAgentCallAtMs:online?now:null,totalToolCalls:0}],serverTimeMs:now}});
  if(path==='/api/settings/update'){
   requests.push('update');await held;
   return route.fulfill({json:{status:'scheduled',targetVersion:'2.4.0',updateId:'update-test'}});
  }
  if(path==='/api/settings/restart'){requests.push('restart');return route.fulfill({json:{scheduled:true}});}
  if(path==='/api/settings')return route.fulfill({json:{document:{},defaults:{},raw:'{}',revision:'1'}});
  if(path.endsWith('/automation'))return route.fulfill({json:{enabled:true,pairing:{status:'verified'},goal:null,action:{status:'idle'},selection:{model:'gpt-6-astra',effort:'max'},companion:{authenticated:true,status:'idle'},catalog:{models:[{id:'gpt-6-astra',label:'GPT-6 Astra',efforts:[{id:'max',label:'Pro',available:true}]}]},presence:{lastAgentCallAtMs:online?now:null},serverTimeMs:now}});
  if(path.endsWith('/send')){
   const body=request.postDataJSON();sends.push(body);
   return route.fulfill({json:{_meta:{[META]:{sent:{id:body.request_id,end:100,created_at_ms:now,tool_call_count:0}}}}});
  }
  if(path==='/api/chats/'+id)return route.fulfill({json:{_meta:{[META]:{chat_file:'/fixture/CHAT.md',revision:'0',messages:[],delivered_through:0,read_through:0,last_agent_call_at_ms:online?now:null,total_tool_calls:0,server_time_ms:now,has_more:false,unchanged:false}}}});
  return route.fulfill({status:404});
 });
 await page.goto(origin+'/?chat='+id+'#test-token');
 const draft=page.locator('#chat-host #draft');await draft.waitFor();await page.waitForFunction(()=>document.getElementById('global-update')?.hidden===false);
 await draft.fill('Keep this draft through startup');
 const send=page.locator('#chat-host #send');await send.waitFor({state:'visible'});
 await page.waitForFunction(()=>document.querySelector('#chat-host .chat-shell')?.shadowRoot?.getElementById('send')?.disabled===false);
 return {page,draft,send,value,errors,requests,sends,reads,transport,releaseUpdate};
}
async function tick(page,title) {
 for(let attempt=0;attempt<8;attempt++){
  await page.clock.fastForward(2000);await delay(25);
  if(await page.locator('#global-progress').isVisible()&&(await page.locator('#global-progress-title').textContent()).includes(title))break;
 }
 await page.locator('#global-progress-title').filter({hasText:title}).waitFor();
}
const assertDraft=async f=>{
 assert.equal(await f.draft.inputValue(),'Keep this draft through startup');
 assert.doesNotMatch(await f.page.locator('#global-progress-detail').textContent(),/Your draft is kept/);
 assert.doesNotMatch(await f.page.locator('#chat-host #status').textContent(),/Your draft is kept/);
 assert.equal(await f.send.isDisabled(),true);
 await f.draft.press('Enter');
 assert.equal(await f.draft.inputValue(),'Keep this draft through startup');
 assert.equal(f.sends.length,0);
};

for(const [name,engine] of [['Chromium',chromium],['WebKit',webkit]]) {
 test(`${name}: sidebar update shows immediate progress and keeps drafts until browser and connector startup finish`,{timeout:30000},async()=>{
  const browser=await engine.launch();
  try {
   const f=await fixture(browser),{page,value}=f;
   // On mobile the sidebar is hidden while reading a chat. Open it to update.
   await page.locator('#chat-host').getByRole('button',{name:'Back to chats',exact:true}).click();
   await page.locator('#global-update').click();
   await page.locator('#global-progress-title').filter({hasText:'Downloading and verifying'}).waitFor();
   assert.equal(await page.locator('#settings-dialog').isVisible(),false);
   assert.equal(await page.locator('#global-update').getAttribute('aria-busy'),'true');
   assert.equal(await page.locator('#global-update').isDisabled(),true);
   await page.locator('.conversation').click();await assertDraft(f);
   value.update={state:'installing',updateId:'update-test',targetVersion:'2.4.0'};
   await tick(page,'Installing update');
   f.releaseUpdate();
   f.transport.down=true;await tick(page,'Starting Codexify');await assertDraft(f);
   assert.equal(await page.locator('#chat-host #status').evaluate(node=>node.classList.contains('error')),false,'expected restart disconnection is loading, rather than a chat error');
   assert.equal(await page.locator('#error').isVisible(),false);
   f.transport.down=false;
   value.version='2.4.0';value.instanceId='200';value.latest.updateAvailable=false;value.update.state='succeeded';
   value.readiness={ready:false,loading:true,browserOnly:true,phase:'starting_browser',detail:'Starting the browser companion…'};
   value.connectorRefresh={phase:'starting_browser',updateId:'update-test',targetVersion:'2.4.0'};
   await tick(page,'Starting the browser companion');await assertDraft(f);
   if(process.env.CODEXIFY_UPDATE_SCREENSHOTS){mkdirSync(process.env.CODEXIFY_UPDATE_SCREENSHOTS,{recursive:true});await page.screenshot({path:`${process.env.CODEXIFY_UPDATE_SCREENSHOTS}/${name.toLowerCase()}-mobile-startup.png`});}
   assert.equal(await page.locator('#global-update').isVisible(),false,'sidebar stays hidden while reading; progress stays visible above the chat');
   assert.equal(await page.locator('#global-progress').isVisible(),true);
   value.readiness={ready:false,loading:true,phase:'refreshing_connector',detail:'Refreshing ChatGPT connector tools…'};value.connectorRefresh.phase='running';
   await tick(page,'Refreshing ChatGPT connector tools');await assertDraft(f);
   assert.equal(await page.locator('#global-progress-detail').textContent(),'');
   value.readiness={ready:true,loading:false,phase:'ready'};value.connectorRefresh.phase='completed';
   await tick(page,'Codexify updated.');
   await page.waitForFunction(()=>document.querySelector('#chat-host .chat-shell')?.shadowRoot?.getElementById('send')?.disabled===false);
   assert.equal(await f.draft.inputValue(),'Keep this draft through startup');assert.equal(f.sends.length,0,'startup must not automatically replay a message');
   const sent=page.waitForResponse(response=>new URL(response.url()).pathname.endsWith('/send'));
   await f.send.click();await sent;
   assert.equal(f.sends.length,1);assert.equal(f.sends[0].message,'Keep this draft through startup');
   assert.deepEqual(f.requests,['update']);assert.deepEqual(f.errors,[]);
   for(const width of [390,1280]){
    await page.setViewportSize({width,height:844});
    assert.equal(await page.locator('#global-progress').evaluate(node=>node.scrollWidth>node.clientWidth),false);
   }
  }finally{await browser.close();}
 });

 test(`${name}: a rolled-back update stays visible and permits retry without losing a draft`,{timeout:20000},async()=>{
  const browser=await engine.launch();
  try {
   const f=await fixture(browser),{page,value}=f;
   await page.locator('#chat-host').getByRole('button',{name:'Back to chats',exact:true}).click();
   f.releaseUpdate();await page.locator('#global-update').click();
   value.update={state:'rolled_back',updateId:'update-test',failureDetail:'Startup failed; the previous version was restored.'};
   await tick(page,'Update rolled back');
   await page.waitForFunction(()=>!document.querySelector('#global-update')?.disabled);
   assert.match(await page.locator('#global-progress-detail').textContent(),/previous version was restored/);
   assert.equal(await page.locator('#global-progress').getAttribute('data-error'),'true');
   assert.equal(await page.locator('#global-progress-action').textContent(),'Retry update');
   assert.equal(await page.locator('#global-progress-action').isDisabled(),false);
   await page.emulateMedia({colorScheme:'dark'});
   assert.equal(await page.locator('#global-progress-title').evaluate(node=>getComputedStyle(node).color),'rgb(255, 152, 155)');
   assert.deepEqual(f.requests,['update']);assert.equal(f.sends.length,0);
   await page.locator('.conversation').click();assert.equal(await f.draft.inputValue(),'Keep this draft through startup');
   await page.getByRole('button',{name:'Dismiss update status',exact:true}).click();
   assert.equal(await page.locator('#global-progress').isVisible(),false);assert.deepEqual(f.errors,[]);
  }finally{await browser.close();}
 });

 test(`${name}: restart-required status explains the action and an old healthy process cannot finish a restart`,{timeout:25000},async()=>{
  const browser=await engine.launch();
  try {
   const f=await fixture(browser),{page,value}=f;
   value.connectorRefresh={phase:'waiting_for_restart',originPid:100,requiresRestart:true,refreshId:'schema-test'};
   await page.clock.fastForward(16000);await page.locator('#global-progress-title').filter({hasText:'Restart required to refresh tools'}).waitFor();
   await page.getByRole('button',{name:'Restart Codexify',exact:true}).click();
   for(let attempt=0;attempt<10&&f.reads.health<3;attempt++){await page.clock.fastForward(2000);await delay(30);}
   assert(f.reads.health>=3,'observe several healthy responses from the old process');
   assert.equal(await f.send.isDisabled(),true);assert.deepEqual(f.requests,['restart']);
   assert.doesNotMatch(await page.locator('#global-progress-title').textContent(),/Codexify restarted\./,'a live old PID is not a completed restart');
   value.instanceId='200';value.connectorRefresh.phase='starting_browser';
   value.readiness={ready:false,loading:true,browserOnly:true,phase:'starting_browser',detail:'Starting the browser companion…'};
   await tick(page,'Starting the browser companion');await assertDraft(f);
   value.connectorRefresh.phase='completed';value.readiness={ready:true,loading:false,phase:'ready'};
   await tick(page,'Codexify restarted.');
   await page.waitForFunction(()=>document.querySelector('#chat-host .chat-shell')?.shadowRoot?.getElementById('send')?.disabled===false);
   assert.equal(await f.draft.inputValue(),'Keep this draft through startup');assert.deepEqual(f.errors,[]);
  }finally{await browser.close();}
 });

 test(`${name}: browser recovery keeps native chat usable while service startup holds messages`,{timeout:20000},async()=>{
  const browser=await engine.launch();
  try {
   const f=await fixture(browser,{online:true}),{page,value}=f;
   value.readiness={ready:false,loading:true,browserOnly:true,phase:'starting_browser',detail:'Starting the browser companion…'};
   await page.clock.fastForward(16000);await page.locator('#global-progress-title').filter({hasText:'Starting the browser companion'}).waitFor();
   assert.equal(await f.send.isDisabled(),false,'an online native agent does not need the browser to receive a queued message');
   assert.equal(await f.draft.inputValue(),'Keep this draft through startup');
   value.readiness={ready:false,loading:true,browserOnly:false,phase:'starting_tunnels',detail:'Connecting ChatGPT tunnels…'};
   await tick(page,'Connecting ChatGPT tunnels');await assertDraft(f);
   assert.deepEqual(f.requests,[]);assert.deepEqual(f.errors,[]);
  }finally{await browser.close();}
 });
}
