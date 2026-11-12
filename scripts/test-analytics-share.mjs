import assert from 'node:assert/strict';
import test from 'node:test';
import {runInNewContext} from 'node:vm';
import {createRequire} from 'node:module';
import {mkdtempSync,mkdirSync,readFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {createHash} from 'node:crypto';
import {ownerHtml,routeOwner} from './owner-chat-source.mjs';
const {chromium,webkit}=createRequire(import.meta.url)('playwright');
const origin='http://127.0.0.1:43329',today='2026-10-17';
const numbers={1:{turns:12,toolCalls:690,goals:1},7:{turns:84,toolCalls:12480,goals:9},30:{turns:328,toolCalls:50321,goals:27},90:{turns:820,toolCalls:150000,goals:84},all:{turns:1240,toolCalls:284000,goals:119}};
const modelCases={1:['gpt-5.6-sol','max','GPT-5.6 Sol Pro'],7:['gpt-6-astra','pro','GPT-6 Astra Pro'],30:['gpt-5.6-sol','max','GPT-5.6 Sol Pro'],90:['gpt-5.6-sol','xhigh','GPT-5.6 Sol (Extra High)'],all:['gpt-6-astra','pro','GPT-6 Astra Pro']};
const activeMs={1:365400000,7:12345678,30:36000000,90:720000000,all:72000000000};
const timeText={1:'101h',7:'3h 25m',30:'10h 0m',90:'200h',all:'20000h'};
test('time values show hours and minutes below 100 hours and whole hours from 100 onward',()=>{
 const source=readFileSync(new URL('../src/analytics_ui.js',import.meta.url),'utf8');
 const {duration,share}=runInNewContext(source+'\n({duration:analyticsDuration,share:analyticsShareNumber})');
 for(const [ms,expected]of [[0,'0h 0m'],[60000,'0h 1m'],[(45*60+1)*60000,'45h 1m'],[(27*60+23)*60000,'27h 23m'],[100*3600000-1,'99h 59m'],[100*3600000,'100h'],[(134*60+59)*60000,'134h'],[(1705*60+18)*60000,'1705h'],[20000*3600000,'20000h']]){
  assert.equal(duration(ms),expected);assert.equal(share('activeMs',ms),expected);
 }
});
async function fixture(engine,{clipboard=true,usage={}}={}){
 const browser=await engine.launch(),page=await browser.newPage({viewport:{width:1440,height:1000}});page.setDefaultTimeout(6000);const errors=[],requests=[],posts=[];page.on('pageerror',e=>errors.push(e.message));page.on('request',r=>{if(r.url().startsWith(origin))requests.push(new URL(r.url()).pathname);if(r.method()!=='GET')posts.push(new URL(r.url()).pathname);});
 await page.addInitScript(({clipboard})=>{
  // Stop unrelated sidebar polling so this fixture audits only export requests.
  window.setInterval=()=>0;
  window.__drawn=[];window.__copyFailed=false;window.__copyActivation=null;
  const fill=CanvasRenderingContext2D.prototype.fillText;CanvasRenderingContext2D.prototype.fillText=function(text,...args){window.__drawn.push(String(text));return fill.call(this,text,...args);};
  if(clipboard){
   window.ClipboardItem??=class {constructor(data){this.data=data;this.types=Object.keys(data);}getType(type){return Promise.resolve(this.data[type]);}};
   Object.defineProperty(navigator,'clipboard',{configurable:true,value:{write(items){window.__copyActivation=navigator.userActivation?.isActive??null;window.__copied=items[0];return window.__copyFailed?Promise.reject(new DOMException('Denied','NotAllowedError')):Promise.resolve();}}});
  }else Object.defineProperty(navigator,'clipboard',{configurable:true,value:undefined});
 },{clipboard});
 await routeOwner(page,origin+'/**',async route=>{
  const request=route.request(),url=new URL(request.url()),path=url.pathname;
  if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
  assert.equal(request.headers().authorization,'Bearer share-test-token');
  if(path==='/api/global')return route.fulfill({json:{version:'2.0.0',settingsAvailable:true}});
  if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
  if(path==='/api/settings')return route.fulfill({json:{document:{},defaults:{agentChat:{analyticsEnabled:true}},revision:'1',path:'/private/secret-project/config.json',raw:'{}'}});
  if(path==='/api/analytics/turn-cutoffs')return route.fulfill({json:{models:[],samples:0}});
  if(path==='/api/analytics/usage'){
   const days=url.searchParams.get('days'),totals={...numbers[days],activeMs:activeMs[days]},[model,effort]=modelCases[days];
   return route.fulfill({json:{today,timezone:'CEST',since:1700000000000,totals,lifetime:totals,activeDays:7,streak:4,models:[{id:'private-model\x1fhigh',model,effort,...totals,turns:Math.ceil(totals.turns*.6),activeMs:1000},{id:'private-runner\x1fhigh',model:'gpt-runner-up',effort:'high',...totals,turns:Math.floor(totals.turns*.4)}],daily:[{date:'2026-10-11',models:{model:{turns:8}}},{date:today,models:{model:{turns:21}}}],conversations:[{id:'a'.repeat(64),title:'Private project secret',path:'/Users/private/secret-project',...totals}],...usage}});
  }
  return route.fulfill({status:404});
 });
 await page.goto(origin+'/#share-test-token');await page.getByRole('button',{name:'Codexify settings',exact:true}).click();const settings=page.getByRole('dialog',{name:'Codexify settings'});await settings.getByRole('button',{name:'Analytics',exact:true}).click();await settings.getByRole('button',{name:'Share analytics',exact:true}).waitFor();await page.waitForFunction(()=>!document.querySelector('[aria-label="Share analytics"]').disabled);
 return {browser,page,settings,errors,requests,posts};
}
async function ready(page){
 const dialog=page.getByRole('dialog',{name:'Share your stats'}),timeout=20000;
 // Canvas encoding on the shared Linux runner can outlast the control timeout.
 await dialog.locator('#stats-share-download[aria-disabled=false]').waitFor({timeout});
 await page.waitForFunction(()=>{const image=document.getElementById('stats-share-image');return image?.complete&&image.naturalWidth===1200;},null,{timeout});
 return dialog;
}
function png(bytes){assert.equal(bytes.subarray(0,8).toString('hex'),'89504e470d0a1a0a');assert.equal(bytes.readUInt32BE(16),1200);assert.equal(bytes.readUInt32BE(20),675);assert(bytes.length<400000,'compact PNG should stay small');}
for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: share cards export the selected period as a private PNG and copy the same image`,{timeout:45000},async()=>{
 const f=await fixture(engine),{browser,page,settings,errors,requests,posts}=f;const output=process.env.ANALYTICS_SHARE_SCREENSHOTS||mkdtempSync(join(tmpdir(),'analytics-share-'));mkdirSync(output,{recursive:true});
 try{
  const range=settings.getByLabel('Analytics date range');
  for(const period of ['1','7','30','90','all']){
   await range.selectOption(period);await page.waitForFunction(()=>!document.querySelector('[aria-label="Share analytics"]').disabled);
   await page.evaluate(()=>{window.__drawn=[];});const before=requests.length;await settings.getByRole('button',{name:'Share analytics',exact:true}).click();const dialog=await ready(page);
   assert.match(await dialog.locator('#stats-share-description').textContent(),period==='all'?/Lifetime/:period==='1'?/Today/:new RegExp(`Last ${period} days`));
   assert.equal(await dialog.locator('img').getAttribute('src').then(v=>v.startsWith('blob:')),true);
   const text=await page.evaluate(()=>window.__drawn);assert(text.includes('Codexify stats'));assert(text.includes('codexify.dev'));assert(text.includes('Turns'));assert(text.includes('Tool calls'));assert(text.includes('Goals completed'));assert(text.some(t=>t===String(numbers[period].turns)||t===new Intl.NumberFormat('en-US').format(numbers[period].turns)));
   assert(text.includes('Thinking hours'));assert(text.includes(timeText[period]));assert(text.includes(modelCases[period][2]));assert(text.includes(`Top model · ${Math.ceil(numbers[period].turns*.6)} turns`));assert(!text.some(t=>t.includes('estimated')));
   assert(text.includes(period==='all'?'LIFETIME':period==='1'?'TODAY':`LAST ${period} DAYS`));
   assert(!text.some(t=>/secret|private|runner-up|share-test-token|127\.0\.0\.1|chatgpt\.com/i.test(t)),'only aggregate usage, the winning model and public branding belong on the card');
   const pending=page.waitForEvent('download');await dialog.getByRole('link',{name:'Download PNG',exact:true}).click();const download=await pending;
   assert.equal(download.suggestedFilename(),`codexify-stats-${period==='all'?'lifetime':period+(period==='1'?'-day':'-days')}-${today}.png`);const path=join(output,`${name.toLowerCase()}-${period}.png`);await download.saveAs(path);const bytes=readFileSync(path);png(bytes);
   await dialog.getByRole('button',{name:'Copy image',exact:true}).click();await dialog.getByText('Image copied.',{exact:true}).waitFor();
   const copied=await page.evaluate(async()=>Array.from(new Uint8Array(await(await window.__copied.getType('image/png')).arrayBuffer())));assert.equal(createHash('sha256').update(Buffer.from(copied)).digest('hex'),createHash('sha256').update(bytes).digest('hex'));assert.notEqual(await page.evaluate(()=>window.__copyActivation),false,'clipboard write must keep the click gesture');
   assert.equal(requests.length,before,'generation, copy and download must not make server requests');assert.deepEqual(posts,[]);
   if(period==='7'&&process.env.ANALYTICS_SHARE_SCREENSHOTS){await page.screenshot({animations:'disabled',path:join(output,`${name.toLowerCase()}-preview.png`)});await page.setViewportSize({width:375,height:850});const box=await dialog.boundingBox();assert(box&&box.x>=0&&box.x+box.width<=375);assert.equal(await dialog.evaluate(n=>n.scrollWidth>n.clientWidth),false);await page.screenshot({animations:'disabled',path:join(output,`${name.toLowerCase()}-mobile.png`)});await page.setViewportSize({width:1440,height:1000});}
   await dialog.getByRole('button',{name:'Close share preview',exact:true}).click();assert.equal(await settings.isVisible(),true);
  }
  await settings.getByRole('button',{name:'Share analytics',exact:true}).click();await ready(page);await page.mouse.click(3,3);assert.equal(await page.getByRole('dialog',{name:'Share your stats'}).isVisible(),false);assert.equal(await settings.isVisible(),true);
  assert.deepEqual(errors,[]);
 }finally{await browser.close();}
});
for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: clipboard denial and unsupported copy keep PNG download available`,{timeout:30000},async()=>{
 for(const clipboard of [true,false]){
  const {browser,page,settings,errors,posts}=await fixture(engine,{clipboard});try{
   await settings.getByRole('button',{name:'Share analytics',exact:true}).click();const dialog=await ready(page);
   if(clipboard){await page.evaluate(()=>{window.__copyFailed=true;});await dialog.getByRole('button',{name:'Copy image',exact:true}).click();await dialog.getByText('Couldn’t copy the image. Use Download PNG, or try again.',{exact:true}).waitFor();assert.equal(await dialog.getByRole('button',{name:'Copy image',exact:true}).isEnabled(),true);}
   else{assert.equal(await dialog.getByRole('button',{name:'Copy image',exact:true}).isDisabled(),true);await dialog.getByText('Copy isn’t available in this browser. Download the PNG instead.',{exact:true}).waitFor();}
   const pending=page.waitForEvent('download');await dialog.getByRole('link',{name:'Download PNG',exact:true}).click();await pending;
   await page.keyboard.press('Escape');assert.equal(await settings.isVisible(),true);assert.equal(await settings.locator('#settings-save-state').getAttribute('data-error'),'false');assert.deepEqual(errors,[]);assert.deepEqual(posts,[]);
  }finally{await browser.close();}
 }
});

for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: unavailable canvas stays inside the share preview`,{timeout:30000},async()=>{
 const {browser,page,settings,errors,posts}=await fixture(engine);try{
  await page.evaluate(()=>{HTMLCanvasElement.prototype.getContext=()=>null;});await settings.getByRole('button',{name:'Share analytics',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Share your stats'});await dialog.getByText('The image could not be created.',{exact:true}).waitFor();assert.equal(await dialog.getByRole('button',{name:'Copy image',exact:true}).isDisabled(),true);assert.equal(await dialog.getByRole('link',{name:'Download PNG',exact:true}).count(),0);
  await dialog.getByRole('button',{name:'Close share preview',exact:true}).click();assert.equal(await settings.getByRole('button',{name:'Share analytics',exact:true}).isEnabled(),true);assert.equal(await settings.locator('#settings-save-state').getAttribute('data-error'),'false');assert.deepEqual(errors,[]);assert.deepEqual(posts,[]);
 }finally{await browser.close();}
});

for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: animated SVG downloads locally, counts up in an image, and retains PNG copying`,{timeout:45000},async()=>{
 const {browser,page,settings,errors,requests,posts}=await fixture(engine),output=process.env.ANALYTICS_SHARE_SCREENSHOTS||mkdtempSync(join(tmpdir(),'analytics-motion-'));mkdirSync(output,{recursive:true});
 try{
  for(const period of ['1','7','30','90','all']){
   await settings.getByLabel('Analytics date range').selectOption(period);await page.waitForFunction(()=>!document.querySelector('[aria-label="Share analytics"]').disabled);
   const before=requests.length;await settings.getByRole('button',{name:'Share analytics',exact:true}).click();const dialog=await ready(page);
   await dialog.getByRole('button',{name:'Animated SVG',exact:true}).click();await dialog.getByRole('link',{name:'Download SVG',exact:true}).waitFor();
   const source=await dialog.locator('img').getAttribute('src');assert(source.startsWith('blob:'));
   const pending=page.waitForEvent('download');await dialog.getByRole('link',{name:'Download SVG',exact:true}).click();const download=await pending;assert.equal(download.suggestedFilename(),`codexify-stats-${period==='all'?'lifetime':period+(period==='1'?'-day':'-days')}-${today}.svg`);
   const path=join(output,`${name.toLowerCase()}-${period}.svg`);await download.saveAs(path);const svg=readFileSync(path,'utf8');assert.equal(await page.evaluate(async source=>(await fetch(source)).text(),source),svg,'download and live preview use identical SVG bytes');
   assert(svg.length<60000);assert(svg.includes('prefers-reduced-motion'));assert(!/secret|private|share-test-token|127\.0\.0\.1|chatgpt\.com|foreignObject|<script|<image|href=|@import|onload=/i.test(svg));
   const xml=await page.evaluate(svg=>{const d=new DOMParser().parseFromString(svg,'image/svg+xml');return {error:!!d.querySelector('parsererror'),dimensions:[d.documentElement.getAttribute('width'),d.documentElement.getAttribute('height')],final:[...d.querySelectorAll('[data-final]')].map(n=>n.textContent),animated:d.querySelectorAll('animateTransform').length,desc:d.querySelector('desc')?.textContent};},svg);
   assert.equal(xml.error,false);assert.deepEqual(xml.dimensions,['1200','675']);assert(xml.animated>=4);assert(xml.desc.includes(`${numbers[period].toolCalls} tool calls`));assert(xml.desc.includes(`${timeText[period]} thinking time`));assert(svg.includes('Thinking hours'));assert(!svg.includes('estimated'));assert.equal(xml.final.length,5);assert.equal(xml.final[4],modelCases[period][2]);assert(svg.includes(`Top model · ${Math.ceil(numbers[period].turns*.6)} turns`));assert.equal(xml.final[3],timeText[period]);
   assert(svg.includes('Codexify stats · '+(period==='all'?'LIFETIME':period==='1'?'TODAY':`LAST ${period} DAYS`)));
   assert.equal(xml.final[0],new Intl.NumberFormat('en-US',{notation:numbers[period].turns>=10000?'compact':'standard'}).format(numbers[period].turns));
   await dialog.getByRole('button',{name:'Pause preview',exact:true}).click();assert.notEqual(await dialog.locator('img').getAttribute('src'),source);
   await dialog.getByRole('button',{name:'Copy PNG',exact:true}).click();await dialog.getByText('Image copied.',{exact:true}).waitFor();const copied=await page.evaluate(async()=>Array.from(new Uint8Array(await(await window.__copied.getType('image/png')).arrayBuffer())));png(Buffer.from(copied));
   await dialog.getByRole('button',{name:'Play preview',exact:true}).click();assert.equal(await dialog.locator('img').getAttribute('src'),source);
   if(period==='7'){
    await page.setViewportSize({width:375,height:850});assert.equal(await dialog.evaluate(n=>n.scrollWidth>n.clientWidth),false);await page.screenshot({path:join(output,`${name.toLowerCase()}-animated-mobile.png`)});await page.setViewportSize({width:1440,height:1000});
    // Test the exported file in the same restricted image context used by a
    // README: no SVG scripts execute, and no external resources are requested.
    const context=await browser.newContext({javaScriptEnabled:false,viewport:{width:1200,height:675},deviceScaleFactor:1});const frame=await context.newPage();let imageRequests=0;
    await frame.route('http://127.0.0.1:43330/**',route=>{imageRequests++;return route.fulfill({contentType:route.request().url().endsWith('.svg')?'image/svg+xml':'text/html',body:route.request().url().endsWith('.svg')?svg:'<style>body{margin:0}img{display:block;width:1200px;height:675px}</style><img src="/stats.svg" alt="Stats">'});});
    await frame.goto('http://127.0.0.1:43330/');await frame.locator('img').waitFor();await frame.waitForTimeout(150);const early=await frame.screenshot({path:join(output,`${name.toLowerCase()}-motion-start.png`)});await frame.waitForTimeout(700);const middle=await frame.screenshot({path:join(output,`${name.toLowerCase()}-motion-count.png`)});assert.notDeepEqual(early,middle,'SVG must animate as an image without JavaScript');
    assert.equal(imageRequests,2,'the image makes no external resource requests');
    await context.close();
    // Slow screenshots must not move a sample across the six-second loop.
    // Use the native SVG clock for exact hold/restart frames; the image above
    // independently proves the same export animates without page scripts.
    const timedContext=await browser.newContext({viewport:{width:1200,height:675},deviceScaleFactor:1}),timed=await timedContext.newPage();
    await timed.route('http://127.0.0.1:43330/stats.svg',route=>route.fulfill({contentType:'image/svg+xml',body:svg}));await timed.goto('http://127.0.0.1:43330/stats.svg');
    const seek=seconds=>timed.evaluate(async seconds=>{document.documentElement.pauseAnimations();document.documentElement.setCurrentTime(seconds);await document.fonts.ready;await new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));},seconds);
    await seek(2.1);await timed.screenshot({path:join(output,`${name.toLowerCase()}-motion-fire.png`)});
    // Check the statistic tiles, excluding fractional rasterization in the
    // decorative activity strip below them.
    const tiles={x:45,y:160,width:1100,height:405};
    await seek(4);await timed.screenshot({path:join(output,`${name.toLowerCase()}-motion-final.png`)});const final=await timed.screenshot({clip:tiles});
    await seek(4.55);assert.equal(createHash('sha256').update(await timed.screenshot({clip:tiles})).digest('hex'),createHash('sha256').update(final).digest('hex'),'final totals must hold still before the loop restarts');
    await seek(6.2);assert.notEqual(createHash('sha256').update(await timed.screenshot({clip:tiles})).digest('hex'),createHash('sha256').update(final).digest('hex'),'the next loop restarts the animation');await timedContext.close();await page.bringToFront();
   }
   assert.equal(requests.length,before,'export makes no requests');await dialog.getByRole('button',{name:'Close share preview',exact:true}).click();
  }
  assert.deepEqual(errors,[]);assert.deepEqual(posts,[]);
 }finally{await browser.close();}
});

for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: reduced-motion SVG is still and SVG export survives unavailable canvas`,{timeout:30000},async()=>{
 const {browser,page,settings,errors,posts}=await fixture(engine);try{
  await page.emulateMedia({reducedMotion:'reduce'});await settings.getByRole('button',{name:'Share analytics',exact:true}).click();const dialog=await ready(page);await dialog.getByRole('button',{name:'Animated SVG',exact:true}).click();
  await dialog.getByRole('button',{name:'Motion reduced',exact:true}).waitFor();assert.equal(await dialog.getByRole('button',{name:'Motion reduced',exact:true}).isDisabled(),true);
  await page.waitForFunction(()=>document.getElementById('stats-share-image').complete);const first=await dialog.locator('img').screenshot();await page.waitForTimeout(700);assert.equal(createHash('sha256').update(await dialog.locator('img').screenshot()).digest('hex'),createHash('sha256').update(first).digest('hex'),'reduced motion must keep exact final totals visible');
  // Check the exported file's own media rule separately: page-level browser
  // emulation is not inherited by SVG image subdocuments on every engine.
  const svg=await page.evaluate(async()=>{const link=document.getElementById('stats-share-download');return(await fetch(link.href)).text();});
  const context=await browser.newContext({reducedMotion:'reduce',viewport:{width:1200,height:675}}),standalone=await context.newPage();
  await standalone.route('http://127.0.0.1:43331/stats.svg',r=>r.fulfill({contentType:'image/svg+xml',body:svg}));await standalone.goto('http://127.0.0.1:43331/stats.svg');
  assert.equal(await standalone.evaluate(()=>[...document.querySelectorAll('.motion')].every(n=>getComputedStyle(n).display==='none')),true);
  await standalone.evaluate(()=>document.documentElement.setCurrentTime(.5));const still=await standalone.screenshot();await standalone.evaluate(()=>document.documentElement.setCurrentTime(2.5));assert.equal(createHash('sha256').update(await standalone.screenshot()).digest('hex'),createHash('sha256').update(still).digest('hex'));await context.close();
  await dialog.getByRole('button',{name:'Close share preview',exact:true}).click();
  await page.evaluate(()=>{HTMLCanvasElement.prototype.getContext=()=>null;});await settings.getByRole('button',{name:'Share analytics',exact:true}).click();await dialog.getByText('The image could not be created.',{exact:true}).waitFor();
  await dialog.getByRole('button',{name:'Animated SVG',exact:true}).click();await dialog.getByRole('link',{name:'Download SVG',exact:true}).waitFor();await page.waitForFunction(()=>document.getElementById('stats-share-image').naturalWidth===1200);assert.equal(await dialog.getByRole('button',{name:'Copy PNG',exact:true}).isDisabled(),true);
  const pending=page.waitForEvent('download');await dialog.getByRole('link',{name:'Download SVG',exact:true}).click();await pending;assert.deepEqual(errors,[]);assert.deepEqual(posts,[]);
 }finally{await browser.close();}
});

for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: share cards handle zero hours and missing model attribution without inventing a winner`,{timeout:30000},async()=>{
 const usage={models:[],totals:{turns:0,toolCalls:0,goals:0,activeMs:0}},f=await fixture(engine,{usage}),{browser,page,settings,errors,posts}=f;
 try{
  for(const [model,hours]of [['No turns yet','0h 0m'],['Not recorded','0h 1m']]){
   if(model==='Not recorded'){
    usage.totals={turns:1,toolCalls:0,goals:0,activeMs:60000};usage.models=[{id:'private-unknown',model:'unattributed',effort:'unknown',...usage.totals}];
    await settings.getByRole('button',{name:'Refresh analytics',exact:true}).click();await page.waitForFunction(()=>!document.querySelector('[aria-label="Share analytics"]').disabled);
   }
   await page.evaluate(()=>window.__drawn=[]);await settings.getByRole('button',{name:'Share analytics',exact:true}).click();const dialog=await ready(page),drawn=await page.evaluate(()=>window.__drawn);
   assert(drawn.includes(model));assert(drawn.includes(hours));assert(!drawn.some(t=>/NaN|undefined|private/.test(t)));
   await dialog.getByRole('button',{name:'Animated SVG',exact:true}).click();await dialog.getByRole('link',{name:'Download SVG',exact:true}).waitFor();
   const values=await page.evaluate(async()=>{const svg=await(await fetch(document.getElementById('stats-share-download').href)).text(),xml=new DOMParser().parseFromString(svg,'image/svg+xml');return {error:!!xml.querySelector('parsererror'),hours:xml.querySelector('[data-final="activeMs"]').textContent,model:xml.querySelector('[data-final="topModel"]').textContent};});
   assert.deepEqual(values,{error:false,hours,model});await dialog.getByRole('button',{name:'Close share preview',exact:true}).click();
  }
  assert.deepEqual(errors,[]);assert.deepEqual(posts,[]);
 }finally{await browser.close();}
});
