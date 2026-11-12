import assert from 'node:assert/strict';
import test from 'node:test';
import {createRequire} from 'node:module';
import {mkdirSync} from 'node:fs';
import {ownerHtml,routeOwner} from './owner-chat-source.mjs';
const {chromium,webkit}=createRequire(import.meta.url)('playwright');
const META='io.github.devnoname120/codexify/markdown-chat',ids=['a'.repeat(64),'b'.repeat(64)],modelIds=['gpt-6-astra\x1fmax','gpt-5.6-sol\x1fhigh'];
function fixture(range,historicalModel=false){
 const n=range==='all'?90:Number(range),today=new Date().toISOString().slice(0,10),daily=[],totals={turns:0,toolCalls:0,activeMs:0,goals:0},models=[{id:modelIds[0],model:'gpt-6-astra',effort:'max',turns:0,toolCalls:0,activeMs:0,goals:0},{id:modelIds[1],model:'gpt-5.6-sol',effort:'high',turns:0,toolCalls:0,activeMs:0,goals:0}];
 for(let i=0;i<n;i++){const day={date:new Date(Date.parse(today)-(n-1-i)*86400000).toISOString().slice(0,10),models:{}};for(let j=0;j<2;j++){const value={turns:(i+j)%4+1,toolCalls:(i+j+1)*8,activeMs:((i+j)%5+1)*12*60000,goals:(i+j)%5===0?1:0};day.models[modelIds[j]]=value;for(const key of Object.keys(totals)){totals[key]+=value[key];models[j][key]+=value[key];}}daily.push(day);}
 if(historicalModel&&range==='7'){const id='gpt-5.5-sol\x1fmedium',value={turns:4,toolCalls:64,activeMs:1200000,goals:0};daily[0].models[id]=value;models.push({id,model:'gpt-5.5-sol',effort:'medium',...value});for(const key of Object.keys(totals))totals[key]+=value[key];}
 return {today,timezone:'CEST',since:Date.now()-120*86400000,daily,models,totals,lifetime:Object.fromEntries(Object.entries(totals).map(([k,v])=>[k,v*3])),activeDays:n,streak:4,conversations:[{id:ids[0],turns:27,toolCalls:900,activeMs:5400000,goals:3},{id:ids[1],turns:14,toolCalls:1600,activeMs:10800000,goals:1}]};
}
for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: colorful analytics use the full panel, stay always on, and preserve charts and silent failures`,{timeout:45000},async()=>{
 const browser=await engine.launch();try{
  const page=await browser.newPage({viewport:{width:1500,height:1000}});page.setDefaultTimeout(7000);
  const errors=[],writes=[],reads=[];page.on('pageerror',e=>errors.push(e.message));let fail=false,malformed=false,revision=1,mixMinutes=null,historicalModel=false;
  const config={agentChat:{analyticsEnabled:false,turnRenewal:{enabled:false,limits:[]}}},snapshot=()=>({document:config,defaults:{agentChat:{analyticsEnabled:true}},raw:JSON.stringify(config),path:'/fixture/config.json',revision:String(revision),restartRequired:false});
  await routeOwner(page,'http://127.0.0.1:43318/**',async route=>{
   const request=route.request(),url=new URL(request.url()),path=url.pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   assert.equal(request.headers().authorization,'Bearer test-token');
   if(request.method()!=='GET')writes.push({path,body:request.postDataJSON()});
   if(path==='/api/global')return route.fulfill({json:{version:'2.0.0',settingsAvailable:true,restartAvailable:true}});
   if(path==='/api/chats')return route.fulfill({json:{chats:ids.map((id,i)=>({id,title:['BioShock optimization','Reader annotations'][i],workspace:['mach-o-vita','epubviewer'][i],lastEntryAtMs:Date.now(),totalToolCalls:0})),serverTimeMs:Date.now()}});
   if(path==='/api/settings'&&request.method()==='GET')return route.fulfill({json:snapshot()});
   if(path==='/api/settings')assert.fail('Analytics must never write a collection setting');
   if(path==='/api/analytics/turn-cutoffs')return route.fulfill({json:{models:[],samples:0}});
   if(path==='/api/analytics/usage'){reads.push(url.searchParams.get('days'));if(fail)return route.fulfill({status:500,body:'private disk error'});const value=fixture(url.searchParams.get('days'),historicalModel);if(mixMinutes!==null){value.models[0].activeMs=mixMinutes*60000;value.models[1].activeMs=0;}return route.fulfill({json:malformed?{models:[null,42],daily:[null,{date:'bad'}],conversations:[null],totals:{turns:-1}}:value});}
   if(path.endsWith('/automation'))return route.fulfill({json:{enabled:false,pairing:{status:'unpaired'},action:{status:'idle'},catalog:{models:[]},selection:{},presence:{},serverTimeMs:Date.now()}});
   if(ids.some(id=>path===`/api/chats/${id}`))return route.fulfill({json:{_meta:{[META]:{messages:[],revision:'0',chat_file:'/fixture/CHAT.md',last_agent_call_at_ms:null,server_time_ms:Date.now(),total_tool_calls:0,has_more:false}}}});
   return route.fulfill({status:404});
  });
  await page.goto('http://127.0.0.1:43318/#test-token');await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Codexify settings'}),nav=dialog.locator('#settings-nav');assert.equal(await nav.locator('button').last().textContent(),'Analytics');
  await dialog.getByRole('button',{name:'Analytics',exact:true}).click();const dashboard=dialog.locator('#usage-analytics');await dashboard.getByText('CEST',{exact:false}).first().waitFor();
  assert(await dialog.locator('#settings-sidebar').isHidden(),'the vertical settings ribbon is removed from analytics');
  assert(await dialog.locator('.settings-footer').isHidden(),'maintenance controls do not occupy dashboard space');
  assert(await dialog.getByRole('button',{name:'Back to settings',exact:true}).isVisible());
  assert.equal(await dashboard.getByLabel('Collect analytics',{exact:true}).count(),0);
  const backgrounds=await dashboard.locator('.analytics-kpi').evaluateAll(nodes=>nodes.map(n=>getComputedStyle(n).backgroundColor));assert.equal(new Set(backgrounds).size,4,'each metric has its own color');
  const main=await dialog.locator('#settings-main').boundingBox(),bounds=await dialog.boundingBox();assert(main.width>bounds.width*.9,'the dashboard uses the full dialog width');
  assert.equal(await dashboard.getByLabel('Analytics date range').inputValue(),'7');assert.deepEqual(reads,['7']);assert.equal(await dashboard.locator('.analytics-kpi').count(),4);assert.equal(await dashboard.getByRole('img').count(),3);
  // The total must stay on one centered line within the donut, including the
  // reported duration and longer lifetime totals, on desktop and mobile.
  for(const width of [1500,375]){
   await page.setViewportSize({width,height:1000});
   for(const font of [null,'Arial Black'])for(const [minutes,label]of [[45*60+1,'45h 1m'],[99*60+59,'99h 59m'],[100*60,'100h'],[167*60+33,'167h'],[99999*60+59,'99999h'],[0,'0h 0m']]){
    mixMinutes=minutes;await dashboard.getByRole('button',{name:'Refresh analytics',exact:true}).click();
    const value=dashboard.locator('.analytics-mix-center strong');await value.filter({hasText:new RegExp(`^${label}$`)}).waitFor();
    const layout=await value.evaluate((node,font)=>{node.style.fontFamily=font||'';const center=node.parentElement.getBoundingClientRect(),range=document.createRange();range.selectNodeContents(node);const text=range.getBoundingClientRect(),caption=node.nextElementSibling.getBoundingClientRect();return {lines:range.getClientRects().length,left:text.left-center.left,right:center.right-text.right,centerOffset:Math.abs((text.left+text.right-center.left-center.right)/2),gap:caption.top-text.bottom,font:getComputedStyle(node).font};},font);
    assert.equal(layout.lines,1,`${label} must not wrap`);assert(layout.left>=3&&layout.right>=3,`${label} at ${width}px must fit inside the donut: ${JSON.stringify(layout)}`);assert(layout.centerOffset<1,`${label} must stay centered`);assert(layout.gap>=2,'the caption must remain separate');
    if(minutes===167*60+33&&process.env.ANALYTICS_SCREENSHOT_DIR){mkdirSync(process.env.ANALYTICS_SCREENSHOT_DIR,{recursive:true});await dashboard.locator('.analytics-mix-ring').locator('..').locator('..').screenshot({path:`${process.env.ANALYTICS_SCREENSHOT_DIR}/${name.toLowerCase()}-mix-${width}.png`});}
   }
  }
  mixMinutes=null;await page.setViewportSize({width:1500,height:1000});
  const card=dashboard.locator('.analytics-card').filter({hasText:'Conversation league'});assert.equal(await card.locator('.analytics-chat-link').first().textContent(),'BioShock optimization');await card.getByRole('button',{name:'Active time',exact:true}).click();assert.equal(await card.locator('.analytics-chat-link').first().textContent(),'Reader annotations');
  const legend=dashboard.locator('.analytics-legend button').first();await legend.click();assert.equal(await dashboard.locator('.analytics-legend button').first().getAttribute('aria-pressed'),'false');await dashboard.locator('.analytics-legend button').first().click();
  historicalModel=true;
  await dashboard.getByLabel('Analytics date range').selectOption('1');await page.waitForFunction(()=>document.querySelector('.analytics-hero h4')?.textContent==='Today'&&!document.querySelector('[aria-label="Refresh analytics"]:disabled'));assert.deepEqual(reads.slice(-2),['1','7']);assert.equal(await dashboard.locator('.analytics-kpi[data-metric=turns] .analytics-kpi-value').textContent(),String(fixture('1').totals.turns));assert.equal(await dashboard.locator('.analytics-chart g[role=button]').count(),21,'the three daily charts show seven days');
  assert.equal(await dashboard.locator('.analytics-legend button').filter({hasText:'GPT-5.5 Sol · Medium'}).count(),3,'the charts include models used earlier in the week');
  assert.equal(await dashboard.locator('.analytics-model-name').filter({hasText:'GPT-5.5 Sol'}).count(),0,'model mix still covers today');
  assert.equal(await dashboard.locator('.analytics-chart').locator('..').locator('.analytics-card-copy').filter({hasText:'Last 7 days.'}).count(),3);
  const oneDayCharts=await dashboard.locator('.analytics-chart svg').evaluateAll(nodes=>nodes.map(node=>node.outerHTML));
  if(process.env.ANALYTICS_SCREENSHOT_DIR){mkdirSync(process.env.ANALYTICS_SCREENSHOT_DIR,{recursive:true});await page.screenshot({animations:'disabled',path:`${process.env.ANALYTICS_SCREENSHOT_DIR}/${name.toLowerCase()}-one-day.png`});}
  await dashboard.getByLabel('Analytics date range').selectOption('7');await page.waitForFunction(()=>document.querySelector('.analytics-hero h4')?.textContent==='The last 7 days'&&!document.querySelector('[aria-label="Refresh analytics"]:disabled'));assert.deepEqual(await dashboard.locator('.analytics-chart svg').evaluateAll(nodes=>nodes.map(node=>node.outerHTML)),oneDayCharts,'one-day trend charts exactly match the seven-day view');
  await dashboard.getByLabel('Analytics date range').selectOption('30');await page.waitForFunction(()=>document.querySelector('.analytics-hero h4')?.textContent==='The last 30 days'&&!document.querySelector('[aria-label="Refresh analytics"]:disabled'));assert.equal(reads.at(-1),'30');
  const out=process.env.ANALYTICS_SCREENSHOT_DIR;if(out){mkdirSync(out,{recursive:true});await dialog.locator('#settings-main').evaluate(n=>n.scrollTop=0);await page.screenshot({animations:'disabled',path:`${out}/${name.toLowerCase()}-desktop.png`});}
  if(out){await card.scrollIntoViewIfNeeded();await page.screenshot({animations:'disabled',path:`${out}/${name.toLowerCase()}-leaderboard.png`});await dashboard.getByRole('heading',{name:'Momentum',exact:true}).locator('..').scrollIntoViewIfNeeded();await page.screenshot({animations:'disabled',path:`${out}/${name.toLowerCase()}-momentum.png`});await dialog.locator('#settings-main').evaluate(n=>n.scrollTop=0);}
  await page.emulateMedia({colorScheme:'dark'});if(out)await page.screenshot({animations:'disabled',path:`${out}/${name.toLowerCase()}-dark.png`});
  await page.setViewportSize({width:375,height:950});await dialog.locator('#settings-main').evaluate(n=>n.scrollTop=0);assert.equal(await dialog.evaluate(n=>n.scrollWidth>n.clientWidth),false);assert.equal(await dashboard.evaluate(n=>n.scrollWidth>n.clientWidth),false);if(out)await page.screenshot({animations:'disabled',path:`${out}/${name.toLowerCase()}-mobile.png`});
  const controls=await dashboard.locator('.analytics-controls').evaluate(node=>[...node.children].map(n=>{const b=n.getBoundingClientRect();return b.top+b.height/2;}));assert(Math.max(...controls)-Math.min(...controls)<5,'mobile controls stay on one row');
  assert.equal(await dashboard.getByLabel('Collect analytics',{exact:true}).count(),0);assert.equal(writes.length,0);
  const prior=await dashboard.locator('.analytics-kpi-value').allTextContents();fail=true;await dashboard.getByRole('button',{name:'Refresh analytics',exact:true}).click();await page.waitForFunction(()=>!document.querySelector('[aria-label="Refresh analytics"]:disabled'));assert.deepEqual(await dashboard.locator('.analytics-kpi-value').allTextContents(),prior);assert.equal(await dialog.locator('#settings-save-state').getAttribute('data-error'),'false');assert.equal(await page.getByText('private disk error').count(),0);
  fail=false;malformed=true;await dashboard.getByLabel('Analytics date range').selectOption('all');await dashboard.getByText('Your next turn starts the story.',{exact:false}).waitFor();await dialog.getByRole('button',{name:'Back to settings',exact:true}).click();assert(await nav.isVisible());assert.equal(await dialog.getByLabel('agentChat.analyticsEnabled',{exact:true}).count(),0);await dialog.getByRole('button',{name:'Analytics',exact:true}).click();assert.deepEqual(errors,[]);assert.equal(writes.length,0);
 }finally{await browser.close();}
});

for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: opening either analytics view fetches fresh figures without a manual refresh`,{timeout:30000},async()=>{
 const browser=await engine.launch(),page=await browser.newPage({viewport:{width:1440,height:1000}});page.setDefaultTimeout(5000);
 try{
  // No polling: opening a view itself must fetch the current snapshot.
  await page.addInitScript(()=>{window.setInterval=()=>0;});
  let version=0,usageReads=0,extrasReads=0,gate=null,fail=false;const errors=[],writes=[];
  page.on('pageerror',error=>errors.push(error.message));
  const hold=()=>{let release;const wait=new Promise(resolve=>release=resolve);gate={wait,release};return gate;};
  await routeOwner(page,'http://127.0.0.1:43358/**',async route=>{
   const req=route.request(),url=new URL(req.url()),path=url.pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   assert.equal(req.headers().authorization,'Bearer fresh-stats-token');if(req.method()!=='GET')writes.push(path);
   if(path==='/api/global')return route.fulfill({json:{version:'2.2.1',settingsAvailable:true}});
   if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
   if(path==='/api/settings')return route.fulfill({json:{document:{},defaults:{agentChat:{extras:{achievements:false,animations:false,sounds:false,weeklyRecap:false,dailyStreaks:false,personalRecords:false}}},raw:'{}',revision:'1',path:'/fixture/config.json'}});
   if(path==='/api/analytics/turn-cutoffs')return route.fulfill({json:{models:[],samples:0}});
   if(path==='/api/analytics/extras'){
    extrasReads++;const today=new Date().toISOString().slice(0,10);
    return route.fulfill({json:{today,timezone:'UTC',streak:0,bestStreak:0,activeToday:false,badges:[],notices:[],records:[{metric:'conversationTurns',value:(version+1)*10}],week:{id:'week:test',start:today,end:today,totals:{turns:0,toolCalls:0,goals:0,activeMs:0},activity:[0,0,0,0,0,0,0],ready:false}}});
   }
   if(path==='/api/analytics/usage'){
    usageReads++;const value=fixture(url.searchParams.get('days')),pending=gate,failed=fail;value.totals.turns=(version+1)*100;
    if(pending)await pending.wait;
    return failed?route.fulfill({status:503,body:'Statistics temporarily busy'}):route.fulfill({json:value});
   }
   return route.fulfill({status:404});
  });
  await page.goto('http://127.0.0.1:43358/#fresh-stats-token');
  const stats=page.getByRole('dialog',{name:'Statistics',exact:true}),settings=page.getByRole('dialog',{name:'Codexify settings',exact:true});
  const turns=dialog=>dialog.locator('.analytics-kpi[data-metric=turns] .analytics-kpi-value');
  const record=dialog=>dialog.locator('.extras-record').filter({hasText:'Turns · one chat'}).locator('strong');
  const expectFigures=async(dialog,total,best)=>{await page.waitForFunction(({id,total})=>document.querySelector(`#${id} .analytics-kpi[data-metric=turns] .analytics-kpi-value`)?.textContent===String(total),{id:await dialog.getAttribute('id'),total});await record(dialog).filter({hasText:new RegExp(`^${best}$`)}).waitFor();};
  const openStats=()=>page.getByRole('button',{name:'Statistics',exact:true}).click(),closeStats=()=>stats.getByRole('button',{name:'Close statistics',exact:true}).click();
  await openStats();await expectFigures(stats,100,10);assert.equal(usageReads,1);assert.equal(await stats.getByLabel('Analytics date range').inputValue(),'7');
  await stats.getByLabel('Analytics date range').selectOption('1');await page.waitForFunction(()=>!document.querySelector('#statistics-dialog [aria-label="Share analytics"]').disabled);await closeStats();
  version=1;const reopen=hold(),reopened=page.waitForRequest(req=>new URL(req.url()).pathname==='/api/analytics/usage');await openStats();await reopened;
  assert.equal(await turns(stats).textContent(),'100','cached figures stay visible while fresh data arrives');assert.equal(await stats.getByLabel('Analytics date range').inputValue(),'1');gate=null;reopen.release();await expectFigures(stats,200,20);await closeStats();
  version=2;await page.getByRole('button',{name:'Codexify settings',exact:true}).click();await settings.getByRole('button',{name:'Analytics',exact:true}).click();await expectFigures(settings,300,30);
  await settings.getByRole('button',{name:'Back to settings',exact:true}).click();version=3;await settings.getByRole('button',{name:'Analytics',exact:true}).click();await expectFigures(settings,400,40);
  await settings.getByRole('button',{name:'Close settings',exact:true}).click();version=4;await page.getByRole('button',{name:'Codexify settings',exact:true}).click();await expectFigures(settings,500,50);await settings.getByRole('button',{name:'Close settings',exact:true}).click();
  // An old request finishing after reopening cannot overwrite newer figures.
  version=5;const old=hold(),oldRequest=page.waitForRequest(req=>new URL(req.url()).pathname==='/api/analytics/usage');await openStats();await oldRequest;await closeStats();gate=null;
  version=6;await openStats();await expectFigures(stats,700,70);old.release();await page.waitForTimeout(75);assert.equal(await turns(stats).textContent(),'700');await closeStats();
  fail=true;await openStats();await page.waitForFunction(()=>!document.querySelector('#statistics-dialog [aria-label="Share analytics"]').disabled);assert.equal(await turns(stats).textContent(),'700','a temporary read failure preserves the last successful snapshot');await closeStats();
  fail=false;version=7;await openStats();await expectFigures(stats,800,80);
  assert.equal(usageReads,14,'one-day views refresh today and the seven-day trends without stale requests starting extra reads');assert(extrasReads>=8,'opening analytics also refreshes streaks and records');assert.deepEqual(writes,[]);assert.deepEqual(errors,[]);
 }finally{await browser.close();}
});
