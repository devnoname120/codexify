import assert from 'node:assert/strict';
import {mkdirSync} from 'node:fs';
import {createRequire} from 'node:module';
import test from 'node:test';
import {ownerHtml,routeOwner} from './owner-chat-source.mjs';

const {chromium,webkit}=createRequire(import.meta.url)('playwright');
const META='io.github.devnoname120/codexify/markdown-chat';
const origin='http://127.0.0.1:43261';
const now=Date.parse('2026-10-15T12:00:00+02:00');
const day=24*60*60*1000;

async function fixture(browser, goalMarkdown='Goal completed: Review and analyze this PR', goalTimestamp=now-30000, latest=null, browserOptions={}) {
 const page=await browser.newPage({viewport:{width:1280,height:960},timezoneId:'Europe/Zurich',locale:'en-US'});
 await page.clock.setFixedTime(new Date(now));page.setDefaultTimeout(6000);
 await page.addInitScript(()=>{
  window.testHidden=false;
  Object.defineProperty(document,'hidden',{configurable:true,get:()=>window.testHidden});
 });
 const errors=[],operations=[],browserOperations=[];let shift=0,held=false,release;
 let browserState={authenticated:!browserOptions.unavailable,status:browserOptions.unavailable?'unavailable':'idle',manuallyStopped:false};
 let browserCatalog=[{id:'gpt-6-astra',label:'GPT-6 Astra',efforts:[{id:'max',label:'Pro',available:true}]}];
 let holdBrowser=false,releaseBrowser,failBrowser=false,browserGate=Promise.resolve();
 let gate=Promise.resolve();
 const chats=[0,1,3,8,20,50,0,2].map((days,i)=>({
  id:String(i+1).repeat(64),title:`Conversation ${i+1}`,customTitle:null,
  workspace:i===5?'f'.repeat(64):'packages',isScratch:i===5,
  projectPath:i===5?'/private/scratch/'+ 'f'.repeat(64):'/Volumes/data/dev/projects/packages',projectId:`project-${i}`,
  lastEntryAtMs:now-days*day,lastEntryEnd:200,totalToolCalls:32,lastAgentCallAtMs:now-1000,
  pinnedAtMs:i===6?now:null,archivedAtMs:i===7?now:null,
 }));
 if(latest)chats[0].title=latest.title;
 const messages=[
  {id:'before',role:'agent',markdown:'Before completion',start:0,end:100,tool_call_count:1,created_at_ms:now-60000},
  {id:'goal-completed-1-2',role:'goal',markdown:goalMarkdown,start:100,end:100,tool_call_count:11,created_at_ms:goalTimestamp,goal_status:browserOptions.goalStatus,goal_report:browserOptions.goalReport},
  {id:'after',role:'agent',markdown:latest?.markdown||'After completion',start:100,end:200,tool_call_count:32,created_at_ms:now-1000},
 ];
 page.on('pageerror',error=>errors.push(error.message));
 await routeOwner(page,origin+'/**',async route=>{
  const request=route.request(),url=new URL(request.url());
  if(url.pathname==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
  if(held&&(url.pathname==='/api/chats'||/^\/api\/chats\/[0-9a-f]+$/.test(url.pathname)))await gate;
  if(url.pathname==='/api/settings/connector')return route.fulfill({json:{configured:true,connectorName:'Fixture connector'}});
  if(url.pathname==='/api/global')return route.fulfill({json:{version:'1.7.0',plan:{kind:'pro',label:'Pro 20x'},latest:{updateAvailable:false},settingsAvailable:true,restartAvailable:false}});
  if(url.pathname==='/api/analytics/extras')return route.fulfill({status:503,body:'Optional extras unavailable'});
  if(url.pathname==='/api/analytics/turn-cutoffs')return route.fulfill({json:{samples:0,models:[]}});
  if(url.pathname==='/api/settings')return route.fulfill({json:{revision:'one',document:{},defaults:browserOptions.noChats?{agentChat:{automation:{enabled:true,autoApproveToolCalls:true,headless:false}}}:{},raw:'{}',path:'/fixture/config.json',restartRequired:false}});
  if(url.pathname==='/api/health')return route.fulfill({json:{status:'ok'}});
  if(url.pathname==='/api/projects')return route.fulfill({json:{catalog:{projects:[],total:0},createWorktree:false}});
  if(url.pathname==='/api/new-chat')return route.fulfill({json:{models:[{id:'gpt-6-astra',label:'GPT-6 Astra',efforts:[{id:'max',label:'Pro',available:true}]}],catalogRevision:'one',multiProject:true,createWorktree:false}});
  if(url.pathname==='/api/browser')return route.fulfill({json:{companion:{...browserState}}});
  if(url.pathname==='/api/browser/login'){browserOperations.push('login');browserState={authenticated:!browserOptions.deferLogin,status:browserOptions.deferLogin?'authentication_required':'idle',manuallyStopped:false};return route.fulfill({json:{companion:{...browserState}}});}
  if(url.pathname==='/api/browser/capabilities'){browserOperations.push('capabilities');assert.equal(request.postDataJSON().refresh,true);browserCatalog=[...browserCatalog,{id:'gpt-5.6-sol',label:'GPT-5.6 Sol',efforts:[{id:'max',label:'Pro',available:true}]}];return route.fulfill({json:{companion:{...browserState},catalogRevision:'refreshed',models:browserCatalog}});}
  if(url.pathname==='/api/browser/start'||url.pathname==='/api/browser/stop'){
   const operation=url.pathname.split('/').at(-1);browserOperations.push(operation);assert(request.postDataJSON().requestId);
   if(failBrowser){failBrowser=false;browserState.manuallyStopped=false;return route.fulfill({status:503,body:'Browser startup failed'});}
   browserState=operation==='stop'?{authenticated:false,status:'unavailable',manuallyStopped:true}:{authenticated:true,status:'idle',manuallyStopped:false};
   const result={companion:{...browserState},lifecycle:{manuallyStopped:browserState.manuallyStopped}};
   if(operation==='start'&&holdBrowser)await browserGate;
   return route.fulfill({json:result});
  }
  if(url.pathname==='/api/chats')return route.fulfill({json:{chats:browserOptions.noChats?[]:chats,matchedChatIds:chats.map(chat=>chat.id),serverTimeMs:now+shift}});
  const id=url.pathname.split('/')[3],chat=chats.find(chat=>chat.id===id);assert(chat,'known chat request: '+url.pathname);
  if(url.pathname.endsWith('/title')){
   const body=request.postDataJSON();operations.push({kind:'title',...body});chat.title=body.title||'Latest message';chat.customTitle=body.title||null;
   return route.fulfill({json:{title:chat.title,customTitle:chat.customTitle}});
  }
  if(url.pathname.endsWith('/project')){
   const body=request.postDataJSON();operations.push({kind:'project',...body});assert.equal(body.expectedPath,chat.projectPath);
   chat.projectPath=body.path;chat.workspace='changed';chat.isScratch=false;chat.projectId='changed';
   return route.fulfill({json:{projectPath:chat.projectPath,workspace:chat.workspace,isScratch:false,projectId:'changed'}});
  }
  if(url.pathname.endsWith('/automation'))return route.fulfill({json:{enabled:true,pairing:{status:'verified'},selection:{model:'gpt-6-astra',effort:'max'},catalog:{models:browserCatalog},action:{status:'idle'},goal:null,companion:{...browserState},presence:{lastAgentCallAtMs:now+shift-1000},serverTimeMs:now+shift}});
  return route.fulfill({json:{_meta:{[META]:{chat_file:'/fixture/'+id+'/CHAT.md',revision:'200-'+shift,read_through:200,delivered_through:200,messages,total_tool_calls:32,last_agent_call_at_ms:now+shift-1000,server_time_ms:now+shift,has_more:false,unchanged:false}}}});
 });
 await page.goto(origin+'/'+(browserOptions.noChats?'':'?chat='+chats[0].id)+'#test-token');if(browserOptions.noChats)await page.getByText('No persisted conversations yet.',{exact:true}).waitFor();else await page.locator('.conversation-row').first().waitFor();
 return {page,chats,errors,operations,browserOperations,setBrowserState(value){browserState=value;},holdBrowserStart(){holdBrowser=true;browserGate=new Promise(resolve=>releaseBrowser=resolve);},releaseBrowserStart(){holdBrowser=false;releaseBrowser?.();},failBrowserStart(){failBrowser=true;},hold(){held=true;gate=new Promise(resolve=>release=resolve);},release(){held=false;release?.();},advance(ms){shift+=ms;for(const chat of chats)chat.lastAgentCallAtMs=now+shift-1000;}};
}

for(const [name,engine] of [['Chromium',chromium],['WebKit',webkit]]) {
 test(`${name}: global browser controls work without chats, recover, stop deliberately and ignore a stale start response`,{timeout:30000},async()=>{
  const browser=await engine.launch();let f;
  try {
   f=await fixture(browser,undefined,undefined,null,{unavailable:true,noChats:true,deferLogin:true});const {page}=f;
   await page.getByRole('button',{name:'Codexify settings',exact:true}).click();const settings=page.getByRole('dialog',{name:'Codexify settings',exact:true});await settings.getByRole('button',{name:'Browser',exact:true}).click();
   const start=settings.getByRole('button',{name:'Start browser',exact:true}),stop=settings.getByRole('button',{name:'Stop browser',exact:true}),state=settings.locator('#settings-browser-state'),models=settings.getByRole('button',{name:'Refresh models',exact:true});
   await page.waitForFunction(()=>!document.getElementById('settings-browser-start').disabled);
   assert(await start.isEnabled());assert(await stop.isEnabled());
   await start.click();await state.filter({hasText:'Connected'}).waitFor();
   assert(await start.isDisabled());await stop.click();await state.filter({hasText:'Browser auto-start paused'}).waitFor();
   assert(await start.isEnabled());assert(await stop.isDisabled());assert(await models.isDisabled());
   f.holdBrowserStart();await start.click();await settings.getByRole('button',{name:'Starting…',exact:true}).waitFor();
   assert(await stop.isEnabled(),'Stop remains available during startup');await stop.click();
   await state.filter({hasText:'Browser auto-start paused'}).waitFor();f.releaseBrowserStart();
   await page.waitForTimeout(100);assert.equal(await state.textContent(),'Browser auto-start paused');
   f.failBrowserStart();await start.click();await settings.locator('#settings-browser-feedback').filter({hasText:'Browser startup failed'}).waitFor();assert(await start.isEnabled());
   assert.deepEqual(f.browserOperations,['start','stop','start','stop','start']);assert.deepEqual(f.operations,[]);
   await settings.getByRole('button',{name:'Sign in',exact:true}).click();await settings.getByText('Complete sign-in in the browser.',{exact:true}).waitFor();
   f.setBrowserState({authenticated:true,status:'idle',manuallyStopped:false});await settings.getByRole('button',{name:'Browser',exact:true}).click();await state.filter({hasText:'Connected'}).waitFor();await settings.getByText('Signed in.',{exact:true}).waitFor();assert(await settings.getByRole('button',{name:'Sign in',exact:true}).isDisabled());
   await models.click();await settings.getByText('Models refreshed.',{exact:true}).waitFor();
   f.setBrowserState({authenticated:true,status:'busy',activeActionId:'other-action'});await settings.getByRole('button',{name:'Browser',exact:true}).click();await page.waitForFunction(()=>document.getElementById('settings-browser-models').disabled);assert(await stop.isEnabled());
   f.setBrowserState({authenticated:true,status:'idle',activeActionId:null});await settings.getByRole('button',{name:'Browser',exact:true}).click();await page.waitForFunction(()=>!document.getElementById('settings-browser-models').disabled);
   assert.deepEqual(f.browserOperations,['start','stop','start','stop','start','login','capabilities']);assert.equal(await page.locator('.conversation-row').count(),0);
   if(process.env.CODEXIFY_UI_POLISH_SCREENSHOTS){mkdirSync(process.env.CODEXIFY_UI_POLISH_SCREENSHOTS,{recursive:true});await page.screenshot({path:`${process.env.CODEXIFY_UI_POLISH_SCREENSHOTS}/${name.toLowerCase()}-browser-controls.png`});}
   await page.setViewportSize({width:390,height:860});assert.equal(await settings.evaluate(n=>n.scrollWidth>n.clientWidth),false);
   if(process.env.CODEXIFY_UI_POLISH_SCREENSHOTS)await page.screenshot({path:`${process.env.CODEXIFY_UI_POLISH_SCREENSHOTS}/${name.toLowerCase()}-browser-controls-mobile.png`});
   assert.deepEqual(f.errors,[]);
  }finally{f?.releaseBrowserStart();await browser.close();}
 });

 test(`${name}: global model refresh updates an open conversation without moving shared controls back into it`,{timeout:20000},async()=>{
  const browser=await engine.launch();
  try{
   const f=await fixture(browser),{page}=f,host=page.locator('#chat-host'),model=host.getByLabel('ChatGPT model',{exact:true});
   await model.locator('option[value="gpt-6-astra"]').waitFor({state:'attached'});
   await host.getByRole('button',{name:'Connection settings',exact:true}).click();
   const conversation=host.getByRole('dialog',{name:'Conversation settings',exact:true});
   for(const name of ['Start browser','Stop browser','Sign in','Refresh models'])assert.equal(await conversation.getByRole('button',{name,exact:true}).count(),0);
   assert.equal(await conversation.locator('[aria-labelledby="connection-pairing-heading"]').getByRole('button',{name:'Open in ChatGPT',exact:true}).count(),1);
   await conversation.getByRole('button',{name:'Done',exact:true}).click();
   await page.getByRole('button',{name:'Codexify settings',exact:true}).click();const settings=page.getByRole('dialog',{name:'Codexify settings',exact:true});await settings.getByRole('button',{name:'Browser',exact:true}).click();
   await page.waitForFunction(()=>!document.getElementById('settings-browser-models').disabled);await settings.getByRole('button',{name:'Refresh models',exact:true}).click();
   await model.locator('option[value="gpt-5.6-sol"]').waitFor({state:'attached'});assert.equal(await model.inputValue(),'gpt-6-astra');assert.equal(await host.getByLabel('Reasoning effort',{exact:true}).inputValue(),'max');
   await settings.getByRole('button',{name:'Close settings',exact:true}).click();assert.equal(await page.locator('.conversation-row[aria-current=true]').getAttribute('data-chat-id'),f.chats[0].id);
   assert.deepEqual(f.browserOperations,['capabilities']);assert.deepEqual(f.operations,[]);assert.deepEqual(f.errors,[]);
  }finally{await browser.close();}
 });

 test(`${name}: plain titles stay consistent while message Markdown is preserved`,{timeout:30000},async()=>{
  const browser=await engine.launch();
  try {
   const title='It should not require a major redesign. Use session.sh and the guide';
   const markdown='**It should not require a major redesign.** Use `session.sh` and [the guide](https://example.com/guide)';
   const {page,chats,errors}=await fixture(browser,undefined,undefined,{title,markdown}),host=page.locator('#chat-host');
   await host.locator('#chat-title').filter({hasText:title}).waitFor();
   const row=page.locator(`.conversation-row[data-chat-id="${chats[0].id}"]`);
   assert.equal(await row.locator('.conversation-title').textContent(),title);
   assert.equal(await host.locator('#chat-title').textContent(),title);
   assert.equal(await host.locator('#chat-title').getAttribute('title'),title);
   assert.equal(await row.locator('.conversation').getAttribute('title'),`${title} · packages`);
   const message=host.locator('#messages .message').last();
   assert.equal(await message.locator('strong').textContent(),'It should not require a major redesign.');
   assert.equal(await message.locator('code').textContent(),'session.sh');
   assert.equal(await message.locator('a').textContent(),'the guide');
   await host.getByRole('button',{name:'Connection settings',exact:true}).click();
   assert.equal(await host.locator('#conversation-settings-name').textContent(),title);
   await host.getByRole('button',{name:'Close connection settings',exact:true}).click();
   await host.locator('#chat-title').click();
   assert.equal(await host.getByRole('textbox',{name:'Chat name',exact:true}).inputValue(),title);
   await host.getByRole('textbox',{name:'Chat name',exact:true}).press('Escape');
   if(process.env.CODEXIFY_UI_POLISH_SCREENSHOTS){mkdirSync(process.env.CODEXIFY_UI_POLISH_SCREENSHOTS,{recursive:true});await page.screenshot({path:`${process.env.CODEXIFY_UI_POLISH_SCREENSHOTS}/${name.toLowerCase()}-plain-titles.png`});}
   assert.deepEqual(errors,[]);
  }finally{await browser.close();}
 });

 test(`${name}: completed goals stay compact and expand without losing their timeline position`,{timeout:30000},async()=>{
  const browser=await engine.launch();
  try {
   const objective='Survey PHP libraries and implementations for generating CBR/CBZ thumbnails, including source quality, performance, maintenance, relevant GitHub issues, and the newly posted thenamelessthing/comicpreviews implementation; execute representative tests.\n\nCompare **performance** and `CBZ` support.\n\n'+ 'long-unbroken-project-name'.repeat(12);
   const {page,errors}=await fixture(browser,'Goal completed: '+objective,now-day-30000),host=page.locator('#chat-host'),event=host.locator('.goal-completed-event'),summary=event.locator('summary');
   await event.waitFor();
   const capture=async label=>{
    if(!process.env.CODEXIFY_UI_POLISH_SCREENSHOTS)return;
    mkdirSync(process.env.CODEXIFY_UI_POLISH_SCREENSHOTS,{recursive:true});
    await host.locator('#messages').screenshot({path:`${process.env.CODEXIFY_UI_POLISH_SCREENSHOTS}/${name.toLowerCase()}-goal-${label}.png`});
   };
   const assertCompact=async()=>{
    assert.equal(await event.locator('svg').evaluate(element=>getComputedStyle(element).color),await host.locator('#goal-icon').evaluate(element=>getComputedStyle(element).color),'timeline and goal bar use the same completed-goal green');
    const bounds=await event.boundingBox();assert(bounds.height<=36,'collapsed completion is one compact row');
    assert(await event.locator('.markdown').isHidden(),'full goal is collapsed by default');
    const metrics=await event.evaluate(element=>{
     const box=element.getBoundingClientRect(),stamp=element.querySelector('.receipt').getBoundingClientRect(),label=element.querySelector('.goal-completed-label').getBoundingClientRect();
     const svg=element.querySelector('svg'),view=svg.viewBox.baseVal;
     return {fits:stamp.right<=box.right&&stamp.left>=label.right,viewport:[view.x,view.y,view.width,view.height],paths:[...svg.querySelectorAll('path')].map(path=>{const b=path.getBBox();return [b.x,b.y,b.x+b.width,b.y+b.height];})};
    });
    assert(metrics.fits,'timestamp never collides with the completion label: '+JSON.stringify(metrics));
    assert.deepEqual(metrics.viewport,[0,0,24,24]);
    assert(metrics.paths.every(([x,y,right,bottom])=>x>=1&&y>=1&&right<=23&&bottom<=23),'the complete icon and stroke fit inside the viewport');
    assert.equal(await page.locator('body').evaluate(body=>body.scrollWidth>innerWidth),false,'no horizontal overflow');
   };
   await assertCompact();await capture('desktop');
   assert.equal(await event.locator('time').getAttribute('datetime'),new Date(now-day-30000).toISOString(),'compact date retains its exact timestamp');
   assert((await event.locator('time').textContent()).includes('Oct 14'),'historical completion still shows its date');
   const preview=await event.locator('.goal-completed-preview').evaluate(element=>({width:element.clientWidth,content:element.scrollWidth}));
   assert(preview.content>preview.width,'long preview stays on one ellipsized line');
   await summary.focus();await summary.press('Space');assert(await event.locator('.markdown').isVisible());
   assert((await event.locator('.markdown').textContent()).includes('long-unbroken-project-name'.repeat(12)),'the full objective is available without truncation');
   assert.equal(await event.locator('.markdown strong').textContent(),'performance');
   await capture('expanded');
   const refreshed=page.waitForResponse(response=>response.url().endsWith('/api/chats'));
   await page.evaluate(()=>window.dispatchEvent(new Event('focus')));await refreshed;
   assert.equal(await event.getAttribute('open'),'','refresh preserves the disclosure state');
   await summary.press('Enter');await assertCompact();
   for(const width of [390,320]){
    await page.setViewportSize({width,height:960});await capture('mobile-'+width);await assertCompact();
    await summary.click();assert(await event.locator('.markdown').isVisible());
    assert.equal(await host.locator('#messages').evaluate(element=>element.scrollWidth>element.clientWidth),false,'expanded long words wrap in narrow layouts');
    await summary.click();
   }
   await page.emulateMedia({colorScheme:'dark'});await assertCompact();await capture('dark-mobile');
   assert.deepEqual(errors,[]);
  }finally{await browser.close();}
 });
 for(const outcome of ['blocked','completed']) test(`${name}: ${outcome} goal reports show their explanation and next steps inline`,{timeout:30000},async()=>{
  const browser=await engine.launch();
  try {
   const detail=outcome==='blocked'?'The **test device** is offline; acceptance could not run.':'Implemented the changes and passed **42 checks**.';
   const next=outcome==='blocked'?'Reconnect the device, then confirm access in this conversation.':'Review the results; no further implementation is needed.';
   const report=`### ${outcome==='blocked'?'Why blocked':'Work completed'}\n\n${detail}\n\n### ${outcome==='blocked'?'To unblock':'Suggested next steps'}\n\n${next}`;
   const {page,errors}=await fixture(browser,`Goal ${outcome}: Verify the deployment`,undefined,null,{goalStatus:outcome,goalReport:report});
   const host=page.locator('#chat-host'),event=host.locator('.goal-completed-event'),body=event.locator('.markdown');
   await event.waitFor();assert.equal(await event.getAttribute('open'),'','a report is expanded immediately');
   assert.equal(await event.locator('.goal-completed-label').textContent(),`Goal ${outcome}`);
   assert((await body.textContent()).includes(next));assert.equal(await body.locator('h3').count(),2);
   assert.equal(await event.evaluate(e=>e.classList.contains('goal-blocked-event')),outcome==='blocked');
   assert.equal(await host.locator('#messages .goal-completed-event').count(),1);
   for(const width of [390,320]){
    await page.setViewportSize({width,height:960});
    assert(await body.isVisible());
    const bounds=await body.boundingBox();assert(bounds.width>150,'report has room to wrap');
    assert.equal(await host.locator('#messages').evaluate(e=>e.scrollWidth>e.clientWidth),false,'full report fits mobile width');
   }
   if(process.env.CODEXIFY_UI_POLISH_SCREENSHOTS){mkdirSync(process.env.CODEXIFY_UI_POLISH_SCREENSHOTS,{recursive:true});await host.locator('#messages').screenshot({path:`${process.env.CODEXIFY_UI_POLISH_SCREENSHOTS}/${name.toLowerCase()}-goal-report-${outcome}.png`});}
   await event.locator('summary').click();assert(await body.isHidden());
   const refreshed=page.waitForResponse(r=>r.url().endsWith('/api/chats'));await page.evaluate(()=>window.dispatchEvent(new Event('focus')));await refreshed;
   assert(await body.isHidden(),'polling preserves a user-collapsed report');
   await event.locator('summary').click();assert(await body.isVisible());
   assert((await body.textContent()).includes(next));assert.deepEqual(errors,[]);
  }finally{await browser.close();}
 });
 test(`${name}: goal timeline, project path editing, inline rename and backdrop dismissal`,{timeout:45000},async()=>{
  const browser=await engine.launch();
  try {
   const {page,chats,errors,operations}=await fixture(browser),host=page.locator('#chat-host');
   const capture=async label=>{if(process.env.CODEXIFY_UI_POLISH_SCREENSHOTS){mkdirSync(process.env.CODEXIFY_UI_POLISH_SCREENSHOTS,{recursive:true});await (label.startsWith('compact-header')?host.locator('.header'):page).screenshot({path:`${process.env.CODEXIFY_UI_POLISH_SCREENSHOTS}/${name.toLowerCase()}-${label}.png`});}};
   await host.locator('.goal-completed-event').waitFor();
   assert.deepEqual(await host.locator('#messages > .tool-call-marker, #messages > .goal-completed-event').evaluateAll(nodes=>nodes.map(node=>node.querySelector('.goal-completed-label')?.textContent||node.textContent)),['10 tool calls','Goal completed','21 tool calls']);
   assert.equal(await host.locator('.goal-completed-event').count(),1);
   assert.equal(await host.locator('.project-path').textContent(),chats[0].projectPath);
   const assertSingleMetadataLine=async()=>{
    const metrics=await host.locator('#chat-subtitle').evaluate(element=>({height:element.getBoundingClientRect().height,lineHeight:parseFloat(getComputedStyle(element).lineHeight)}));
    assert(metrics.height<=metrics.lineHeight+1,'project, date, and path share one metadata line');
   };
   await assertSingleMetadataLine();await capture('compact-header');
   await host.locator('#chat-title').click();
   const input=host.getByRole('textbox',{name:'Chat name',exact:true});await input.fill('Renamed inline');await capture('inline-name');await input.press('Enter');
   await host.getByRole('button',{name:'Rename conversation',exact:true}).filter({hasText:'Renamed inline'}).waitFor();
   assert.equal(operations.filter(item=>item.kind==='title').length,1);
   await host.locator('#chat-title').click();await input.fill('Discard this');await input.press('Escape');
   assert.equal(await host.locator('#chat-title').textContent(),'Renamed inline');
   await host.getByRole('button',{name:'Connection settings',exact:true}).click();
   const path=host.getByRole('textbox',{name:'Full project path',exact:true});assert.equal(await path.inputValue(),chats[0].projectPath);
   await capture('conversation-settings');
   await path.fill('/Volumes/data/dev/projects/changed');await host.getByRole('button',{name:'Save project path',exact:true}).click();
   await host.locator('.project-path').filter({hasText:'/Volumes/data/dev/projects/changed'}).waitFor();
   assert.equal(operations.filter(item=>item.kind==='project').length,1);
   await page.mouse.click(2,2);await host.locator('#automation-settings').waitFor({state:'hidden'});
   await page.getByRole('button',{name:'Codexify settings',exact:true}).click();await page.locator('#settings-dialog[open]').waitFor();
   await page.mouse.click(2,2);await page.locator('#settings-dialog').waitFor({state:'hidden'});
   await page.getByRole('button',{name:'New chat',exact:true}).click();await page.locator('#new-chat-dialog[open]').waitFor();
   await page.mouse.click(2,2);await page.locator('#new-chat-dialog').waitFor({state:'hidden'});
   await page.setViewportSize({width:390,height:960});
   await assertSingleMetadataLine();await capture('compact-header-mobile');
   await host.locator('#chat-title').click();await capture('inline-name-mobile');
   assert.equal(await page.locator('body').evaluate(body=>body.scrollWidth>innerWidth),false,'inline naming fits a narrow viewport');
   await input.press('Escape');await page.setViewportSize({width:1280,height:960});
   if(process.env.CODEXIFY_UI_POLISH_SCREENSHOTS){mkdirSync(process.env.CODEXIFY_UI_POLISH_SCREENSHOTS,{recursive:true});await page.screenshot({path:`${process.env.CODEXIFY_UI_POLISH_SCREENSHOTS}/${name.toLowerCase()}-timeline.png`});}
   assert.deepEqual(errors,[]);
  }finally{await browser.close();}
 });
 test(`${name}: recent groups are disjoint and scratch and archived labels stay readable`,{timeout:30000},async()=>{
  const browser=await engine.launch();
  try {
   const {page,chats,errors}=await fixture(browser);
   await page.getByRole('button',{name:'Recent',exact:true}).click();
   if(process.env.CODEXIFY_UI_POLISH_SCREENSHOTS){mkdirSync(process.env.CODEXIFY_UI_POLISH_SCREENSHOTS,{recursive:true});await page.screenshot({path:`${process.env.CODEXIFY_UI_POLISH_SCREENSHOTS}/${name.toLowerCase()}-recent.png`});}
   assert.deepEqual(await page.locator('.recent-heading').allTextContents(),['Pinned','Today','Yesterday','This week','Last week','Last month','Older']);
   const ids=await page.locator('.conversation-row').evaluateAll(rows=>rows.map(row=>row.dataset.chatId));assert.equal(new Set(ids).size,ids.length);
   const scratch=page.locator(`.conversation-row[data-chat-id="${chats[5].id}"]`);
   assert((await scratch.textContent()).includes('Scratch conversation'));assert(!(await scratch.textContent()).includes('f'.repeat(32)));
   await scratch.locator('.conversation').click();await page.locator('#chat-host #chat-title').filter({hasText:chats[5].title}).waitFor();
   assert((await page.locator('#chat-host #chat-subtitle').textContent()).includes('Scratch conversation'));
   await page.getByRole('button',{name:'Archived chats',exact:true}).click();
   const fill=await page.locator('.restore-action .archive-body').evaluate(path=>getComputedStyle(path).fill);assert.notEqual(fill,'none');
   for(const width of [1280,390]){
    await page.setViewportSize({width,height:960});
    const back=page.locator('#chat-host').getByRole('button',{name:'Back to chats',exact:true});if(await back.isVisible())await back.click();
    assert.equal(await page.locator('body').evaluate(body=>body.scrollWidth>innerWidth),false,'no horizontal overflow');
    if(process.env.CODEXIFY_UI_POLISH_SCREENSHOTS)await page.screenshot({path:`${process.env.CODEXIFY_UI_POLISH_SCREENSHOTS}/${name.toLowerCase()}-archived-${width}.png`});
   }
   assert.deepEqual(errors,[]);
  }finally{await browser.close();}
 });
 test(`${name}: returning from a hidden tab keeps previous presence until fresh responses arrive`,{timeout:30000},async()=>{
  const browser=await engine.launch();
  try {
   const f=await fixture(browser),{page,chats,errors}=f;
   const sidebar=page.locator(`.conversation-row[data-chat-id="${chats[0].id}"] .presence`),header=page.locator('#chat-host #presence');
   await sidebar.locator('.presence-spinner').waitFor();await page.waitForFunction(()=>document.querySelector('#chat-host .chat-shell').shadowRoot.querySelector('#presence').dataset.state==='online');
   f.hold();await page.evaluate(()=>{window.testHidden=true;document.dispatchEvent(new Event('visibilitychange'));});
   f.advance(10*60*1000);await page.clock.setFixedTime(new Date(now+10*60*1000));
   await page.evaluate(()=>{window.testHidden=false;document.dispatchEvent(new Event('visibilitychange'));window.dispatchEvent(new Event('focus'));});
   const observations=await page.evaluate(async()=>{
    const result=[];for(let i=0;i<12;i++){await new Promise(requestAnimationFrame);result.push({sidebar:document.querySelector('.conversation-row[data-chat-id="'+ '1'.repeat(64)+'"] .presence').className,header:document.querySelector('#chat-host .chat-shell').shadowRoot.querySelector('#presence').dataset.state});}return result;
   });
   assert(observations.every(value=>value.sidebar.includes('online')&&value.header==='online'),'no temporary away/offline state while refreshing');
   f.release();await page.waitForFunction(()=>document.querySelector('#chat-host .chat-shell').shadowRoot.querySelector('#presence').dataset.state==='online');
   assert.equal(await header.getAttribute('data-state'),'online');assert.deepEqual(errors,[]);
  }finally{await browser.close();}
 });
}
