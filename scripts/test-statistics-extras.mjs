import assert from 'node:assert/strict';
import test from 'node:test';
import {createRequire} from 'node:module';
import {mkdirSync,readFileSync,writeFileSync} from 'node:fs';
import {ownerHtml,routeOwner} from './owner-chat-source.mjs';
const {chromium,webkit}=createRequire(import.meta.url)('playwright');
const origin='http://127.0.0.1:43349',date='2026-10-19',id='a'.repeat(64);
const totals={turns:84,toolCalls:12480,goals:9,activeMs:36000000};
const defaults={agentChat:{extras:{achievements:true,animations:true,sounds:true,weeklyRecap:true,dailyStreaks:true,personalRecords:true}}};
const achievementFixture=[
 {"id":"first-goal","title":"First finish","detail":"Complete a goal","target":1,"progress":1,"earned":true,"color":"mint","symbol":"flag"},
 {"id":"ten-goals","title":"Finishing streak","detail":"Complete 10 goals","target":10,"progress":9,"earned":false,"color":"violet","symbol":"flag"},
 {"id":"twenty-five-goals","title":"Follow through","detail":"Complete 25 goals","target":25,"progress":9,"earned":false,"color":"sky","symbol":"flag"},
 {"id":"hundred-goals","title":"Century of finishes","detail":"Complete 100 goals","target":100,"progress":9,"earned":false,"color":"pink","symbol":"flag"},
 {"id":"five-hundred-goals","title":"Finishing machine","detail":"Complete 500 goals","target":500,"progress":9,"earned":false,"color":"amber","symbol":"flag"},
 {"id":"thousand-goals","title":"A thousand victories","detail":"Complete 1,000 goals","target":1000,"progress":9,"earned":false,"color":"violet","symbol":"flag"},
 {"id":"ten-turns","title":"Getting going","detail":"Send 10 turns","target":10,"progress":10,"earned":true,"color":"sky","symbol":"spark"},
 {"id":"hundred-turns","title":"In the groove","detail":"Send 100 turns","target":100,"progress":84,"earned":false,"color":"pink","symbol":"spark"},
 {"id":"five-hundred-turns","title":"Finding a rhythm","detail":"Send 500 turns","target":500,"progress":84,"earned":false,"color":"mint","symbol":"spark"},
 {"id":"thousand-turns","title":"A thousand ideas","detail":"Send 1,000 turns","target":1000,"progress":84,"earned":false,"color":"amber","symbol":"spark"},
 {"id":"ten-thousand-turns","title":"Long game","detail":"Send 10,000 turns","target":10000,"progress":84,"earned":false,"color":"violet","symbol":"spark"},
 {"id":"hundred-thousand-turns","title":"Beyond the horizon","detail":"Send 100,000 turns","target":100000,"progress":84,"earned":false,"color":"pink","symbol":"spark"},
 {"id":"thousand-tools","title":"Toolsmith","detail":"Run 1,000 tools","target":1000,"progress":1000,"earned":true,"color":"amber","symbol":"bolt"},
 {"id":"ten-thousand-tools","title":"Power builder","detail":"Run 10,000 tools","target":10000,"progress":10000,"earned":true,"color":"violet","symbol":"bolt"},
 {"id":"hundred-thousand-tools","title":"Master builder","detail":"Run 100,000 tools","target":100000,"progress":12480,"earned":false,"color":"sky","symbol":"bolt"},
 {"id":"million-tools","title":"Million-tool club","detail":"Run 1,000,000 tools","target":1000000,"progress":12480,"earned":false,"color":"mint","symbol":"bolt"},
 {"id":"five-million-tools","title":"Industrial scale","detail":"Run 5,000,000 tools","target":5000000,"progress":12480,"earned":false,"color":"pink","symbol":"bolt"},
 {"id":"ten-million-tools","title":"Ten-million league","detail":"Run 10,000,000 tools","target":10000000,"progress":12480,"earned":false,"color":"violet","symbol":"bolt"},
 {"id":"three-days","title":"Three's a streak","detail":"Be active on 3 consecutive days","target":3,"progress":3,"earned":true,"color":"amber","symbol":"flame"},
 {"id":"seven-days","title":"A week of momentum","detail":"Be active on 7 consecutive days","target":7,"progress":7,"earned":true,"color":"pink","symbol":"flame"},
 {"id":"thirty-days","title":"A month of momentum","detail":"Be active on 30 consecutive days","target":30,"progress":12,"earned":false,"color":"mint","symbol":"flame"},
 {"id":"hundred-days","title":"Hundred-day fire","detail":"Be active on 100 consecutive days","target":100,"progress":12,"earned":false,"color":"sky","symbol":"flame"},
 {"id":"one-eighty-days","title":"Staying power","detail":"Be active on 180 consecutive days","target":180,"progress":12,"earned":false,"color":"violet","symbol":"flame"},
 {"id":"year-streak","title":"A year of momentum","detail":"Be active on 365 consecutive days","target":365,"progress":12,"earned":false,"color":"amber","symbol":"flame"},
 {"id":"six-hour-goal","title":"Deep focus","detail":"Reach 6 active hours on one goal","target":6,"progress":6,"earned":true,"color":"sky","symbol":"trophy"},
 {"id":"day-goal","title":"Day by day","detail":"Reach 24 active hours on one goal","target":24,"progress":20,"earned":false,"color":"mint","symbol":"trophy"},
 {"id":"three-day-goal","title":"Endurance","detail":"Reach 72 active hours on one goal","target":72,"progress":20,"earned":false,"color":"pink","symbol":"trophy"},
 {"id":"week-goal","title":"Marathon","detail":"Reach 168 active hours on one goal","target":168,"progress":20,"earned":false,"color":"violet","symbol":"trophy"},
 {"id":"hundred-turn-chat","title":"Keep the thread","detail":"Send 100 turns in one conversation","target":100,"progress":100,"earned":true,"color":"mint","symbol":"spark"},
 {"id":"five-hundred-turn-chat","title":"An ongoing story","detail":"Send 500 turns in one conversation","target":500,"progress":123,"earned":false,"color":"sky","symbol":"spark"},
 {"id":"thousand-turn-chat","title":"Epic conversation","detail":"Send 1,000 turns in one conversation","target":1000,"progress":123,"earned":false,"color":"pink","symbol":"spark"},
 {"id":"five-thousand-turn-chat","title":"The long haul","detail":"Send 5,000 turns in one conversation","target":5000,"progress":123,"earned":false,"color":"violet","symbol":"spark"},
];
function extra(){return {preferences:structuredClone(defaults.agentChat.extras),today:date,timezone:'UTC',streak:7,bestStreak:12,activeToday:true,records:[{metric:'conversationTurns',value:123},{metric:'conversationTools',value:12480},{metric:'goalActiveMs',value:72000000},{metric:'goalTurns',value:34}],badges:structuredClone(achievementFixture),week:{id:'week:2026-10-18',start:'2026-10-12',end:'2026-10-18',totals,models:[{id:'gpt-6-astra\x1fpro',model:'gpt-6-astra',effort:'pro',...totals}],activity:[4,8,16,12,20,15,9],activeDays:7,ready:false},notices:[]};}
async function fixture(engine){
 const browser=await engine.launch(),page=await browser.newPage({viewport:{width:1440,height:1000}});page.setDefaultTimeout(6000);await page.clock.install();
 await page.addInitScript(()=>{
  const calls=window.__celebrationAudio={contexts:0,resumes:0,suspends:0,starts:[],stops:0};
  class Param{value=0;setValueAtTime(v){this.value=v;}linearRampToValueAtTime(v){this.value=v;}exponentialRampToValueAtTime(v){this.value=v;}}
  class Node{constructor(type){this.type=type;this.gain=new Param();this.frequency=new Param();}connect(node){return node;}disconnect(){}start(at){calls.starts.push({at,type:this.type});}stop(){calls.stops++;}}
  class Audio{constructor(){calls.contexts++;this.state='suspended';this.sampleRate=48000;this.currentTime=0;this.destination=new Node('destination');}resume(){calls.resumes++;this.state='running';return Promise.resolve();}suspend(){calls.suspends++;this.state='suspended';return Promise.resolve();}close(){this.state='closed';return Promise.resolve();}createGain(){return new Node('gain');}createOscillator(){return new Node('oscillator');}createBiquadFilter(){return new Node('filter');}createBufferSource(){return new Node('buffer');}createBuffer(_channels,length){const samples=new Float32Array(length);return {getChannelData:()=>samples};}}
  window.AudioContext=Audio;window.webkitAudioContext=Audio;
 });
 const errors=[],posts=[],claims=new Set();let extras=extra(),document={agentChat:{extras:{}}},revision=1;
 page.on('pageerror',e=>errors.push(e.message));
 await routeOwner(page,origin+'/**',async route=>{const req=route.request(),url=new URL(req.url()),path=url.pathname;
  if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
  assert.equal(req.headers().authorization,'Bearer fixture-token');
  if(req.method()!=='GET')posts.push(path);
  if(path==='/api/global')return route.fulfill({json:{version:'2.1.0',settingsAvailable:true,restartAvailable:true,plan:{label:'Pro 20x'}}});
  if(path==='/api/chats')return route.fulfill({json:{chats:[{id,title:'Private chat title',workspace:'private-project',lastEntryAtMs:Date.now(),totalToolCalls:0}],serverTimeMs:Date.now()}});
  if(path==='/api/analytics/extras')return route.fulfill({json:{...extras,preferences:{...extras.preferences,...document.agentChat.extras}}});
  if(path==='/api/analytics/extras/claim'){const key=req.postDataJSON().id,claimed=!claims.has(key);claims.add(key);return route.fulfill({json:{claimed}});}
  if(path==='/api/analytics/usage')return route.fulfill({json:{since:Date.parse('2026-10-01'),today:date,timezone:'UTC',totals,lifetime:totals,streak:7,activeDays:7,models:[{id:'gpt-5.6-sol\x1fmax',model:'gpt-5.6-sol',effort:'max',...totals}],daily:extras.week.activity.map((turns,i)=>({date:`2026-10-${13+i}`,models:{['gpt-5.6-sol\x1fmax']:{turns,toolCalls:1000,activeMs:3600000,goals:1}}})),conversations:[{id,...totals}]}});
  if(path==='/api/analytics/turn-cutoffs')return route.fulfill({json:{models:[],samples:0}});
  if(path==='/api/settings'){
   if(req.method()==='POST'){const body=req.postDataJSON();for(const [key,value]of Object.entries(body.changes||{}))document.agentChat.extras[key.split('.').at(-1)]=value;revision++;}
   return route.fulfill({json:{defaults,fieldSchema:defaults,document,revision:String(revision),raw:JSON.stringify(document),path:'/fixture/config.json',restartRequired:false}});
  }
  return route.fulfill({json:{enabled:false,pairing:{status:'unpaired'},action:{status:'idle'},catalog:{models:[]},selection:{},presence:{},serverTimeMs:Date.now(),_meta:{['io.github.devnoname120/codexify/markdown-chat']:{messages:[],revision:'0',chat_file:'/fixture/CHAT.md',last_agent_call_at_ms:null,server_time_ms:Date.now(),total_tool_calls:0,has_more:false}}}});
 });
 await page.goto(origin+'/#fixture-token');await page.getByText('Pro 20x',{exact:true}).waitFor();
 return {browser,page,errors,posts,claims,get extras(){return extras;},set extras(v){extras=v;}};
}
for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: standalone statistics, weekly sharing and Extras are responsive and independent`,{timeout:60000},async()=>{
 const f=await fixture(engine),{page,browser}=f;const out=process.env.EXTRAS_SCREENSHOTS||'/tmp/statistics-extras-screenshots';mkdirSync(out,{recursive:true});
 try{
  const button=page.getByRole('button',{name:'Statistics',exact:true});const icons=await page.locator('#global-footer-actions button').evaluateAll(nodes=>nodes.slice(0,2).map(n=>{const b=n.getBoundingClientRect();return {x:b.x,y:b.y,w:b.width,h:b.height};}));assert.equal(icons[0].y,icons[1].y);assert.equal(icons[0].w,icons[1].w);assert(icons[0].x<icons[1].x);
  await button.click();const stats=page.getByRole('dialog',{name:'Statistics',exact:true});await stats.getByText('A collection worth building',{exact:true}).waitFor();assert.equal(await page.locator('#settings-dialog').evaluate(n=>n.open),false);assert.equal(await stats.locator('.analytics-kpi').count(),4);assert.equal(await stats.locator('.extras-badge').count(),32);assert.equal(await stats.getByText('123',{exact:true}).count(),1);
  await page.screenshot({path:out+'/'+name+'-desktop.png',animations:'disabled'});await stats.locator('.extras-badges').scrollIntoViewIfNeeded();await page.screenshot({path:out+'/'+name+'-badges.png',animations:'disabled'});await page.emulateMedia({colorScheme:'dark'});await stats.locator('.statistics-content').evaluate(n=>n.scrollTop=0);await page.screenshot({path:out+'/'+name+'-dark.png',animations:'disabled'});await page.emulateMedia({colorScheme:'light'});
  await stats.getByRole('button',{name:'Your week in review',exact:false}).click();const recap=page.getByRole('dialog',{name:'Your week in review',exact:true});assert(await recap.isVisible());assert.match(await recap.locator('.recap-dates').textContent(),/Oct 12.*Oct 18/);assert.equal(await recap.locator('.recap-tile').count(),4);assert.equal(await recap.locator('[data-metric=activeMs] strong').textContent(),await stats.locator('.analytics-kpi[data-metric=activeMs] .analytics-kpi-value').textContent());assert.equal(await recap.locator('[data-metric=activeMs] span').textContent(),'Active time · estimated');await page.screenshot({path:out+'/'+name+'-recap.png',animations:'disabled'});
  await recap.getByRole('button',{name:'Share your week',exact:true}).click();const share=page.getByRole('dialog',{name:'Share your stats',exact:true});await share.locator('#stats-share-download[aria-disabled=false]').waitFor();assert.match(await share.locator('#stats-share-description').textContent(),/WEEK IN REVIEW/);assert.match(await share.locator('#stats-share-download').getAttribute('download'),/week-in-review/);
  await share.getByRole('button',{name:'Animated SVG',exact:true}).click();await share.locator('#stats-share-download[aria-disabled=false]').waitFor();const downloadPromise=page.waitForEvent('download');await share.getByRole('link',{name:'Download SVG',exact:true}).click();const download=await downloadPromise;const path=out+'/'+name+'-recap.svg';await download.saveAs(path);const svg=readFileSync(path,'utf8');assert(svg.includes('My week in Codexify'));assert(svg.includes('WEEK IN REVIEW'));assert(svg.includes('Thinking hours'));assert(svg.includes('10h 0m thinking time'));assert(!svg.includes('estimated'));assert(svg.includes('GPT-6 Astra Pro'));assert(!svg.includes('GPT-5.6 Sol'),'weekly model must come from the completed week, not the rolling dashboard');assert(!svg.includes('Private chat title'));assert(!svg.includes('fixture-token'));
  await share.getByRole('button',{name:'Close share preview',exact:true}).click();await recap.getByRole('button',{name:'Close weekly recap',exact:true}).click();await stats.getByRole('button',{name:'Share analytics',exact:true}).click();await share.locator('#stats-share-download[aria-disabled=false]').waitFor();assert.equal(await page.locator('#analytics-share-dialog').count(),1,'all statistics windows reuse the export dialog');assert.match(await share.locator('#stats-share-description').textContent(),/Last 7 days/);await share.getByRole('button',{name:'Close share preview',exact:true}).click();
  await page.setViewportSize({width:390,height:844});await stats.locator('.statistics-content').evaluate(n=>n.scrollTop=0);await page.screenshot({path:out+'/'+name+'-mobile.png',animations:'disabled'});assert.equal(await stats.evaluate(n=>n.scrollWidth>n.clientWidth),false);assert.equal(await stats.locator('.analytics').evaluate(n=>n.scrollWidth>n.clientWidth),false);await stats.locator('.extras-badges').scrollIntoViewIfNeeded();await page.screenshot({path:out+'/'+name+'-mobile-badges.png',animations:'disabled'});
  f.extras.week.totals={...totals,activeMs:(1705*60+18)*60000};await page.clock.fastForward(30001);await stats.getByRole('button',{name:'Your week in review',exact:false}).click();assert.equal(await recap.locator('[data-metric=activeMs] strong').textContent(),'1705h');assert.equal(await recap.locator('.recap-tiles').evaluate(n=>getComputedStyle(n).gridTemplateColumns.split(' ').length),2);assert(await recap.locator('[data-metric=activeMs] strong').evaluate(n=>n.scrollWidth<=n.clientWidth),'long hour totals fit the mobile tile');await page.screenshot({path:out+'/'+name+'-mobile-recap.png',animations:'disabled'});assert.equal(await recap.evaluate(n=>n.scrollWidth>n.clientWidth),false);assert(await recap.evaluate(n=>n.getBoundingClientRect().bottom-n.querySelector('.recap-footer').getBoundingClientRect().bottom<50),'recap fits its content without a large blank tail');await recap.getByRole('button',{name:'Close weekly recap',exact:true}).click();await stats.getByRole('button',{name:'Close statistics',exact:true}).click();
  await page.getByRole('button',{name:'Codexify settings',exact:true}).click();const settings=page.getByRole('dialog',{name:'Codexify settings',exact:true});await settings.getByRole('button',{name:'Extras',exact:true}).click();assert.equal(await settings.locator('[data-setting-row^="agentChat.extras."]').count(),6);await page.screenshot({path:out+'/'+name+'-extras-mobile.png',animations:'disabled'});
  for(const row of await settings.locator('[data-setting-row^="agentChat.extras."]').all())await row.getByRole('button',{name:'Off',exact:true}).click();await page.clock.fastForward(650);await settings.getByText('All changes saved.',{exact:true}).waitFor();await settings.getByRole('button',{name:'Close settings',exact:true}).click();
  f.extras.notices=[{id:'f'.repeat(64),kind:'record',title:'Personal best!',detail:'125 turns in one conversation'}];f.extras.week.ready=true;await page.clock.fastForward(30001);assert.equal(await page.locator('#extras-celebration').count(),0);assert.equal(await page.locator('#weekly-recap-dialog').evaluate(n=>n.open),false);
  await button.click();await stats.getByText('A collection worth building',{exact:true}).waitFor();assert.equal(await stats.locator('.extras-badge').count(),32);assert(await stats.getByRole('button',{name:'Share analytics',exact:true}).isVisible());await stats.getByRole('button',{name:'Your week in review',exact:false}).click();assert(await recap.isVisible(),'disabled automatic recaps remain available manually');assert.equal(await recap.getAttribute('data-motion'),'false');await page.clock.fastForward(30001);assert(await recap.isVisible());await recap.getByRole('button',{name:'Close weekly recap',exact:true}).click();assert.deepEqual(f.errors,[]);
 }finally{await browser.close();}
});
for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: automatic recap is once-only and celebrations do not block or replay`,{timeout:45000},async()=>{
 const f=await fixture(engine),{page,browser}=f;
 try{
  f.extras.week.ready=true;await page.clock.fastForward(3600);const recap=page.getByRole('dialog',{name:'Your week in review',exact:true});await recap.waitFor();assert.equal(await page.evaluate(()=>window.__celebrationAudio.contexts),0,'automatic recaps must not unlock audio');assert(f.claims.has(f.extras.week.id));await recap.getByRole('button',{name:'Close weekly recap',exact:true}).click();await page.clock.fastForward(30001);assert.equal(await page.locator('#weekly-recap-dialog').evaluate(n=>n.open),false);
  f.extras.week.ready=false;f.extras.notices=[{id:'c'.repeat(64),kind:'record',title:'Personal best!',detail:'125 turns in one conversation'}];await page.clock.fastForward(30001);const celebration=page.locator('#extras-celebration');await celebration.waitFor();assert.equal(await celebration.evaluate(n=>getComputedStyle(n).pointerEvents),'none');assert.equal(await page.locator('dialog[open]').count(),0);assert.equal(await celebration.locator('.extras-particle').count(),220);const soundStarts=await page.evaluate(()=>window.__celebrationAudio.starts.length);assert(soundStarts>0);if(process.env.EXTRAS_SCREENSHOTS){await celebration.evaluate(layer=>{for(const animation of document.getAnimations())if(layer.contains(animation.effect?.target)){animation.pause();animation.currentTime=1500;}});await page.screenshot({path:process.env.EXTRAS_SCREENSHOTS+'/'+name+'-celebration.png'});}await page.clock.fastForward(3700);assert.equal(await celebration.count(),0);await page.clock.fastForward(30001);assert.equal(await celebration.count(),0,'stale polling response is claimed only once');assert.equal(await page.evaluate(()=>window.__celebrationAudio.starts.length),soundStarts,'sound must not replay a claimed notice');
  f.extras.notices=[{id:'d'.repeat(64),kind:'achievement',title:'Unlocked: Toolsmith',detail:'Run 1,000 tools'}];await page.emulateMedia({reducedMotion:'reduce'});await page.waitForFunction(()=>matchMedia('(prefers-reduced-motion: reduce)').matches);await page.clock.fastForward(30001);await celebration.waitFor();await page.waitForFunction(()=>document.querySelector('#extras-celebration .extras-toast')?.dataset.motion==='false'&&!document.querySelector('#extras-celebration .extras-particle'));assert.equal(await celebration.locator('.extras-particle').count(),0);assert.equal(await celebration.locator('.extras-toast').getAttribute('data-motion'),'false');assert(await page.evaluate(()=>window.__celebrationAudio.starts.length)>soundStarts,'sound and reduced motion are independent');const reducedStarts=await page.evaluate(()=>window.__celebrationAudio.starts.length);await page.evaluate(()=>{Object.defineProperty(document,'hidden',{configurable:true,value:true});document.dispatchEvent(new Event('visibilitychange'));});assert(await page.evaluate(()=>window.__celebrationAudio.suspends)>0,'hiding the page stops audio');await page.evaluate(()=>{Object.defineProperty(document,'hidden',{configurable:true,value:false});document.dispatchEvent(new Event('visibilitychange'));});await page.clock.fastForward(7300);assert.equal(await page.evaluate(()=>window.__celebrationAudio.starts.length),reducedStarts,'returning to the page does not replay audio');assert.equal(await celebration.count(),0);assert.deepEqual(f.errors,[]);
 }finally{await browser.close();}
});

for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: celebration audio waits for interaction and mutes immediately without hiding fireworks`,{timeout:30000},async()=>{
 const f=await fixture(engine),{page,browser}=f;
 try{
  f.extras.notices=[{id:'1'.repeat(64),kind:'record',title:'Before interaction',detail:'A new record'}];
  await page.clock.fastForward(3600);const celebration=page.locator('#extras-celebration');await celebration.waitFor();
  assert.equal(await page.evaluate(()=>window.__celebrationAudio.contexts),0);
  await page.clock.fastForward(3700);await page.mouse.click(8,8);
  f.extras.notices=[{id:'2'.repeat(64),kind:'record',title:'Sound is ready',detail:'Another new record'}];
  await page.clock.fastForward(30001);await celebration.getByText('Sound is ready',{exact:true}).waitFor();
  const playing=await page.evaluate(()=>structuredClone(window.__celebrationAudio));assert(playing.starts.length>0);assert.equal(playing.contexts,1);
  // The settings button remains usable through the full-screen effects.
  await page.getByRole('button',{name:'Codexify settings',exact:true}).click();const settings=page.getByRole('dialog',{name:'Codexify settings',exact:true});
  await settings.getByRole('button',{name:'Extras',exact:true}).click();await settings.locator('[data-setting-row="agentChat.extras.sounds"]').getByRole('button',{name:'Off',exact:true}).click();
  await page.clock.fastForward(650);await settings.getByText('All changes saved.',{exact:true}).waitFor();
  const muted=await page.evaluate(()=>structuredClone(window.__celebrationAudio));assert(muted.stops>playing.stops);assert(muted.suspends>0);
  await settings.getByRole('button',{name:'Close settings',exact:true}).click();
  await page.clock.fastForward(3700);await celebration.waitFor({state:'detached'});
  await page.setViewportSize({width:390,height:844});f.extras.notices=[{id:'3'.repeat(64),kind:'achievement',title:'Quiet celebration',detail:'Sound is disabled'}];
  await page.clock.fastForward(30001);await celebration.getByText('Quiet celebration',{exact:true}).waitFor();
  assert.equal(await celebration.locator('.extras-particle').count(),220);if(process.env.EXTRAS_SCREENSHOTS){await celebration.evaluate(layer=>{for(const animation of document.getAnimations())if(layer.contains(animation.effect?.target)){animation.pause();animation.currentTime=1500;}});await page.screenshot({path:process.env.EXTRAS_SCREENSHOTS+'/'+name+'-celebration-mobile.png'});}assert.equal(await page.evaluate(()=>window.__celebrationAudio.starts.length),playing.starts.length);assert.deepEqual(f.errors,[]);
 }finally{await browser.close();}
});

for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: blocked audio cannot prevent a celebration`,{timeout:15000},async()=>{
 const f=await fixture(engine),{page,browser}=f;
 try{
  await page.evaluate(()=>{window.AudioContext=class{constructor(){throw new Error('Audio unavailable');}};window.webkitAudioContext=window.AudioContext;});
  await page.mouse.click(8,8);f.extras.notices=[{id:'4'.repeat(64),kind:'record',title:'Still celebrating',detail:'The sound device is unavailable'}];
  await page.clock.fastForward(3600);await page.getByText('Still celebrating',{exact:true}).waitFor();
  assert.equal(await page.locator('#extras-celebration .extras-particle').count(),220);assert(f.claims.has('4'.repeat(64)));assert.deepEqual(f.errors,[]);
 }finally{await browser.close();}
});

for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: the real celebration sound renders locally with bounded volume and a silent tail`,{timeout:20000},async()=>{
 const f=await fixture(engine),{page,browser}=f;
 try{
  const rendered=await page.evaluate(async exportSound=>{
   const rate=48000,context=new OfflineAudioContext(1,rate*3,rate);scheduleCelebrationSound(context);const buffer=await context.startRendering(),samples=buffer.getChannelData(0);
   let peak=0,energy=0,tail=0;for(let i=0;i<samples.length;i++){peak=Math.max(peak,Math.abs(samples[i]));energy+=samples[i]*samples[i];if(i>rate*2.5)tail=Math.max(tail,Math.abs(samples[i]));}
   let wav;
   if(exportSound){const bytes=new ArrayBuffer(44+samples.length*2),view=new DataView(bytes),text=(offset,value)=>{for(let i=0;i<value.length;i++)view.setUint8(offset+i,value.charCodeAt(i));};text(0,'RIFF');view.setUint32(4,36+samples.length*2,true);text(8,'WAVE');text(12,'fmt ');view.setUint32(16,16,true);view.setUint16(20,1,true);view.setUint16(22,1,true);view.setUint32(24,rate,true);view.setUint32(28,rate*2,true);view.setUint16(32,2,true);view.setUint16(34,16,true);text(36,'data');view.setUint32(40,samples.length*2,true);for(let i=0;i<samples.length;i++)view.setInt16(44+i*2,Math.round(Math.max(-1,Math.min(1,samples[i]))*32767),true);wav=Array.from(new Uint8Array(bytes));}
   return {peak,rms:Math.sqrt(energy/samples.length),tail,wav};
  },!!process.env.EXTRAS_SOUND_PREVIEW_DIR);
  assert(rendered.peak>.03&&rendered.peak<.9,`peak ${rendered.peak}`);assert(rendered.rms>.005);assert(rendered.tail<.0001);
  if(rendered.wav){mkdirSync(process.env.EXTRAS_SOUND_PREVIEW_DIR,{recursive:true});writeFileSync(process.env.EXTRAS_SOUND_PREVIEW_DIR+'/'+name+'-celebration.wav',Buffer.from(rendered.wav));}
  assert.deepEqual(f.errors,[]);
 }finally{await browser.close();}
});
