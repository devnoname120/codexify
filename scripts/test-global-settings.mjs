import assert from 'node:assert/strict';
import {mkdirSync,readFileSync} from 'node:fs';
import test from 'node:test';
import {createRequire} from 'node:module';
import {ownerHtml,routeOwner} from './owner-chat-source.mjs';
const {chromium,webkit}=createRequire(import.meta.url)('playwright');

for(const [engineName,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${engineName}: optional installation titles remain unset until edited and can be cleared`,{timeout:20000},async()=>{
 const browser=await engine.launch();
 try {
  const page=await browser.newPage();page.setDefaultTimeout(5000);await page.clock.install();
  let document={},revision=1;const writes=[];
  const snapshot=()=>({document,defaults:{instanceName:null},fieldSchema:{instanceName:null},raw:JSON.stringify(document),revision:String(revision),restartRequired:revision>1});
  await routeOwner(page,'http://127.0.0.1:43267/**',route=>{
   const request=route.request(),path=new URL(request.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return route.fulfill({json:{version:'2.3.0',settingsAvailable:true}});
   if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
   if(path==='/api/settings'&&request.method()==='GET')return route.fulfill({json:snapshot()});
   if(path==='/api/settings'){
    const body=request.postDataJSON();writes.push(body);
    if(body.raw!=null)document=JSON.parse(body.raw);else Object.assign(document,body.changes);
    revision++;return route.fulfill({json:snapshot()});
   }
   return route.fulfill({status:404});
  });
  await page.goto('http://127.0.0.1:43267/#test-token');
  await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Codexify settings'}),input=dialog.getByLabel('instanceName',{exact:true});
  assert.equal(await input.inputValue(),'');assert.equal(await input.getAttribute('placeholder'),'None (default)');
  assert.deepEqual(writes,[]);assert.deepEqual(document,{});
  await input.fill('Workstation');await page.clock.fastForward(700);
  await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
  assert.equal(document.instanceName,'Workstation');
  await input.fill('');await page.clock.fastForward(700);
  await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
  assert.equal(document.instanceName,undefined);
 }finally{await browser.close();}
});

for(const [engineName,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${engineName}: audit log file shows its default and is optional while logging is off`,{timeout:20000},async()=>{
 const browser=await engine.launch();
 try {
  const page=await browser.newPage();page.setDefaultTimeout(5000);await page.clock.install();
  const logFile='/private/.codexify/projects/fixture/audit.jsonl',writes=[];
  let document={audit:{enabled:false,logFile:''}},revision=1;
  const snapshot=()=>({document,defaults:{audit:{enabled:true,logFile}},fieldSchema:{audit:{enabled:true,logFile:null}},raw:JSON.stringify(document),revision:String(revision),restartRequired:false});
  await routeOwner(page,'http://127.0.0.1:43266/**',route=>{
   const request=route.request(),path=new URL(request.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return route.fulfill({json:{version:'2.3.0',settingsAvailable:true}});
   if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
   if(path==='/api/settings'&&request.method()==='GET')return route.fulfill({json:snapshot()});
   if(path==='/api/settings'){
    const body=request.postDataJSON();writes.push(body);
    if(body.raw!=null)document=JSON.parse(body.raw);
    else for(const [key,value] of Object.entries(body.changes))document.audit[key.split('.').at(-1)]=value;
    revision++;return route.fulfill({json:snapshot()});
   }
   return route.fulfill({status:404});
  });
  await page.goto('http://127.0.0.1:43266/#test-token');
  await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Codexify settings'});
  await dialog.getByRole('button',{name:'Advanced',exact:true}).click();
  const input=dialog.getByLabel('audit.logFile',{exact:true}),row=dialog.locator('[data-setting-row="audit.logFile"]');
  assert.equal(await input.getAttribute('placeholder'),`${logFile} (default)`);
  assert.equal(await input.evaluate(node=>node.required),false);
  assert.match(await row.textContent(),/Optional.*Ignored when Audit logging is off/);
  assert.deepEqual(writes,[]);
  await dialog.getByRole('group',{name:'audit.enabled',exact:true}).getByRole('button',{name:'On',exact:true}).click();await page.clock.fastForward(700);
  await dialog.getByText('All changes saved.',{exact:true}).waitFor();
  assert.equal(document.audit.enabled,true);
  await dialog.getByLabel('audit.logFile',{exact:true}).fill('/custom/audit.jsonl');await page.clock.fastForward(700);
  await dialog.getByText('All changes saved.',{exact:true}).waitFor();
  assert.equal(document.audit.logFile,'/custom/audit.jsonl');
  await dialog.getByLabel('audit.logFile',{exact:true}).fill('');await page.clock.fastForward(700);
  await dialog.getByText('All changes saved.',{exact:true}).waitFor();
  assert.equal(document.audit.logFile,undefined);
  await dialog.getByRole('group',{name:'audit.enabled',exact:true}).getByRole('button',{name:'Off',exact:true}).click();await page.clock.fastForward(700);
  await dialog.getByText('All changes saved.',{exact:true}).waitFor();
  assert.deepEqual(document.audit,{enabled:false});
 }finally{await browser.close();}
});

for(const [engineName,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${engineName}: file materialization defaults off and can be enabled, disabled, and reset`,{timeout:20000},async()=>{
 const browser=await engine.launch();
 try {
  const page=await browser.newPage();page.setDefaultTimeout(5000);await page.clock.install();
  const example=JSON.parse(readFileSync(new URL('../codexify.config.example.json',import.meta.url)));
  assert.equal(example.artifactEgress.enabled,false);
  const defaults={artifactIngress:{enabled:true},artifactEgress:{enabled:example.artifactEgress.enabled}};
  const writes=[];let document={},revision=1;
  const snapshot=()=>({document,defaults,raw:JSON.stringify(document),revision:String(revision),restartRequired:revision>1});
  await routeOwner(page,'http://127.0.0.1:43265/**',route=>{
   const request=route.request(),path=new URL(request.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return route.fulfill({json:{version:'2.3.0',settingsAvailable:true}});
   if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
   if(path==='/api/settings'&&request.method()==='GET')return route.fulfill({json:snapshot()});
   if(path==='/api/settings'){
    const body=request.postDataJSON();writes.push(body);assert.equal(body.revision,String(revision));
    if(body.raw!=null)document=JSON.parse(body.raw);
    else {document.artifactEgress??={};document.artifactEgress.enabled=body.changes['artifactEgress.enabled'];}
    revision++;return route.fulfill({json:snapshot()});
   }
   return route.fulfill({status:404});
  });
  await page.goto('http://127.0.0.1:43265/#test-token');
  await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Codexify settings'});
  await dialog.getByRole('button',{name:'Tools & files',exact:true}).click();
  const row=dialog.locator('[data-setting-row="artifactEgress.enabled"]'),group=row.getByRole('group');
  await row.getByText('File materialization in ChatGPT',{exact:true}).waitFor();
  assert.match(await row.textContent(),/inline attachment text and image previews remain available/);
  assert.equal(await group.getByRole('button',{name:'Off (default)',exact:true}).getAttribute('aria-pressed'),'true');
  assert.deepEqual(writes,[]);
  for(const [label,value] of [['On',true],['Off',false],['Off (default)',null]]){
   await group.getByRole('button',{name:label,exact:true}).click();await page.clock.fastForward(700);
   await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
   if(value===null)assert.equal(JSON.parse(writes.at(-1).raw).artifactEgress?.enabled,undefined);
   else assert.deepEqual(writes.at(-1).changes,{'artifactEgress.enabled':value});
   assert.equal(document.artifactEgress?.enabled,value===null?undefined:value);
   assert.equal(await group.getByRole('button',{name:label,exact:true}).getAttribute('aria-pressed'),'true');
  }
  assert.equal(document.artifactIngress,undefined,'the independent import preference must not be written');
 }finally{await browser.close();}
});

for(const [engineName,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${engineName}: Pro labels repair old generic catalogs and preserve Astra/Pro selection`,{timeout:30000},async()=>{
 const browser=await engine.launch();
 try {
  const page=await browser.newPage();page.setDefaultTimeout(5000);
  const id='e'.repeat(64),now=Date.now(),selection={model:'gpt-6-astra',effort:'max'};
  const models=[
   {id:'gpt-6-astra',label:'GPT-6 (Pro)',efforts:[{id:'max',label:'Pro',available:true}]},
   {id:'gpt-6-sol',label:'GPT-6',efforts:[{id:'high',label:'High',available:true}]},
   {id:'gpt-5.6-sol',label:'GPT-5.6 Sol',efforts:[{id:'high',label:'High',available:true}]},
  ];
  const mutations=[];
  await routeOwner(page,'http://127.0.0.1:43228/**',async route=>{
   const request=route.request(),path=new URL(request.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(request.method()!=='GET')mutations.push(path);
   if(path==='/api/global')return route.fulfill({json:{version:'1.7.0',plan:{label:'Pro 20x',kind:'pro'},latest:{updateAvailable:false}}});
   if(path==='/api/chats')return route.fulfill({json:{chats:[{id,title:'Saved Astra chat',workspace:'fixture',lastEntryEnd:0,lastEntryAtMs:now,lastAgentCallAtMs:null,totalToolCalls:0}],serverTimeMs:Date.now()}});
   if(path.endsWith('/automation'))return route.fulfill({json:{enabled:true,pairing:{status:'verified'},goal:null,action:{status:'idle'},selection,catalog:{models},companion:{authenticated:true,status:'idle'},presence:{lastAgentCallAtMs:null},serverTimeMs:Date.now()}});
   if(path==='/api/settings/connector')return route.fulfill({json:{configured:true,connectorName:'Fixture connector'}});
   if(path==='/api/new-chat'||path.endsWith('/automation/capabilities'))return route.fulfill({json:{models,catalogRevision:'old-generic-catalog'}});
   if(path==='/api/projects')return route.fulfill({json:{catalog:{projects:[],total:0,warnings:[]},createWorktree:true}});
   return route.fulfill({json:{_meta:{['io.github.devnoname120/codexify/markdown-chat']:{chat_file:'/fixture/CHAT.md',revision:'0',messages:[],delivered_through:0,read_through:0,total_tool_calls:0,last_agent_call_at_ms:null,server_time_ms:Date.now(),has_more:false,unchanged:false}}}});
  });
  await page.goto('http://127.0.0.1:43228/#test-token');await page.getByText('Pro 20x',{exact:true}).waitFor();
  await page.locator('.conversation').filter({hasText:'Saved Astra chat'}).click();
  const model=page.locator('#chat-host #automation-model'),effort=page.locator('#chat-host #automation-effort');
  await model.locator('option[value="gpt-6-sol"]').waitFor({state:'attached'});
  assert.deepEqual(await model.locator('option').allTextContents(),['GPT-6 Astra','GPT-6 Sol','GPT-5.6 Sol']);
  assert.equal(await model.inputValue(),'gpt-6-astra');assert.equal(await effort.inputValue(),'max');assert.equal(await effort.locator('option:checked').textContent(),'Pro');
  await page.getByRole('button',{name:'New chat',exact:true}).click();const dialog=page.getByRole('dialog',{name:'New chat'});
  await dialog.locator('#new-chat-model:enabled').waitFor();
  assert.deepEqual(await dialog.getByLabel('ChatGPT model').locator('option').allTextContents(),['GPT-6 Astra','GPT-6 Sol','GPT-5.6 Sol']);
  assert.equal(await dialog.getByLabel('Reasoning effort').inputValue(),'max');
  assert.deepEqual(mutations,['/api/new-chat'],'displaying names must not save a different model or effort');
 }finally{await browser.close();}
});

for(const [engineName,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${engineName}: global settings auto-save, validate raw JSON, and wait for restarted health`,{timeout:30000},async()=>{
 const browser=await engine.launch();
 try {
  const page=await browser.newPage();page.setDefaultTimeout(5000);await page.clock.install();
  let version='1.7.0',instance='old',revision=1,healthDown=0,refreshes=0;
  let document={debug:false,workDir:'/workspace',worktrees:{mode:'auto'},agentChat:{enabled:true,automation:{enabled:true}},apiKey:'private-fixture-key'};
  const saves=[],operations=[],errors=[];page.on('pageerror',error=>errors.push(error.message));
  const snapshot=()=>({path:'/private/codexify.config.json',document,raw:JSON.stringify(document,null,2),revision:String(revision),defaults:document,restartRequired:false});
  await routeOwner(page,'http://127.0.0.1:43227/**',async route=>{
   const request=route.request(),path=new URL(request.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   assert.equal(request.headers().authorization,'Bearer test-token');
   if(path==='/api/global')return route.fulfill({json:{version,instanceId:instance,plan:{label:'Pro 20x',sourceLabel:'ChatGPT Pro 200',kind:'pro'},latest:{version:'1.8.0',updateAvailable:version==='1.7.0'},settingsAvailable:true,restartAvailable:true}});
   if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
   if(path==='/api/settings'&&request.method()==='GET')return route.fulfill({json:snapshot()});
   if(path==='/api/settings') {
    const body=request.postDataJSON();saves.push(body);
    if(body.revision!==String(revision))return route.fulfill({status:409,body:'Configuration changed on disk'});
    if(body.raw!=null){try{const parsed=JSON.parse(body.raw);if(typeof parsed.debug!=='boolean')throw Error('debug must be boolean');document=parsed;}catch(error){return route.fulfill({status:400,body:error.message});}}
    else for(const [key,value] of Object.entries(body.changes)){const parts=key.split('.');let target=document;for(const part of parts.slice(0,-1))target=target[part]??={};target[parts.at(-1)]=value;}
    revision++;return route.fulfill({json:{...snapshot(),restartRequired:true}});
   }
   if(path==='/api/settings/restart'||path==='/api/settings/update'){operations.push(path);healthDown=2;if(path.endsWith('/update'))version='1.8.0';return route.fulfill({json:path.endsWith('/update')?{status:'scheduled',targetVersion:'1.8.0',updateId:'update-fixture'}:{scheduled:true}});}
   if(path==='/api/health'){if(healthDown-->0)return route.fulfill({status:503});instance='new';return route.fulfill({json:{status:'ok',tunnelsConfigured:2,tunnelsReady:2}});}
   if(path==='/api/settings/refresh-connector'){refreshes++;return route.fulfill({json:{phase:'completed',source:'manual',refreshId:'refresh-test',result:{refreshObserved:true,permissionChanged:true,schemaVerified:false}}});}
   return route.fulfill({status:404});
  });
  await page.goto('http://127.0.0.1:43227/#test-token');
  await page.getByText('Pro 20x',{exact:true}).waitFor();
  assert.equal(await page.getByText('Search names and conversation content. Use Rename chat to save a name.').count(),0);
  assert.equal(await page.locator('#global-version').textContent(),'v1.7.0');await page.locator('#global-update').waitFor();
  await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Codexify settings'});
  await dialog.getByRole('button',{name:'Advanced',exact:true}).click();await dialog.getByRole('group',{name:'debug',exact:true}).getByRole('button',{name:'On',exact:true}).click();await page.clock.fastForward(700);
  await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
  assert.equal(document.debug,true);assert.deepEqual(saves[0].changes,{debug:true});assert.equal(document.apiKey,'private-fixture-key');
  await dialog.getByRole('button',{name:'Connections',exact:true}).click();
  assert.equal(await dialog.getByLabel('apiKey',{exact:true}).getAttribute('type'),'password');
  await dialog.getByRole('button',{name:'View raw config',exact:true}).click();
  const raw=dialog.getByLabel('Raw configuration JSON');await raw.fill('{');
  await dialog.getByRole('button',{name:'Save raw config',exact:true}).click();await dialog.locator('#settings-save-state[data-error=true]').waitFor();assert.equal(document.debug,true);
  await raw.fill(JSON.stringify({...document,debug:false}));await dialog.getByText('Raw configuration changes are not saved yet.',{exact:true}).waitFor();
  await dialog.getByRole('button',{name:'Restart Codexify',exact:true}).click();await dialog.getByText('Save raw configuration changes before continuing.',{exact:true}).waitFor();assert.equal(operations.length,0);
  await dialog.getByRole('button',{name:'Save raw config',exact:true}).click();
  await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();assert.equal(document.debug,false);
  await dialog.getByRole('button',{name:'Restart Codexify',exact:true}).click();
  await page.clock.fastForward(6000);await dialog.getByText('Codexify restarted.',{exact:true}).waitFor();assert.equal(operations[0],'/api/settings/restart');
  await dialog.getByRole('button',{name:'Update Codexify',exact:true}).click();await page.clock.fastForward(6000);
  await page.getByText('v1.8.0',{exact:true}).waitFor();await dialog.getByText('Codexify updated.',{exact:true}).waitFor();await page.locator('#global-update').waitFor({state:'hidden'});
  await dialog.getByRole('button',{name:'Refresh connector',exact:true}).click();await dialog.getByText('Connector refresh completed; Allow all tools is selected.',{exact:true}).waitFor();assert.equal(refreshes,1);
  await page.setViewportSize({width:375,height:850});const box=await dialog.boundingBox();assert(box&&box.x>=0&&box.x+box.width<=375);
  assert.deepEqual(errors,[]);
 }finally{await browser.close();}
});

for(const [engineName,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${engineName}: settings categories, search, inherited values, and complex editors stay usable`,{timeout:30000},async()=>{
 const browser=await engine.launch();
 try {
  const page=await browser.newPage({viewport:{width:1280,height:900}});page.setDefaultTimeout(5000);await page.clock.install();
  const defaults={workDir:'/example',multiProject:true,port:3000,debug:false,worktrees:{mode:'auto',keepCount:15},agentChat:{automation:{enabled:false,requestBlockList:[]},notifications:{events:{completed:true,inputRequired:true,errors:true,offline:true}}},mcpServers:{example:{command:'example'}},openaiTunnels:[{tunnelId:'example'}]};
  let document={workDir:'/projects',multiProject:true,port:3000,worktrees:{mode:'always',keepCount:5},agentChat:{automation:{enabled:true,requestBlockList:[]}},mcpServers:{local:{command:'fixture-tools'}},openaiTunnels:[]},revision=1;
  const writes=[],errors=[];let failSave=false,heldSave=null;
  page.on('pageerror',error=>errors.push(error.message));
  const snapshot=()=>({path:'/fixture/config.json',document,defaults,revision:String(revision),raw:JSON.stringify(document,null,2)});
  await routeOwner(page,'http://127.0.0.1:43229/**',async route=>{
   const request=route.request(),path=new URL(request.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return route.fulfill({json:{version:'1.7.0',plan:{label:'Pro 20x'},settingsAvailable:true,restartAvailable:true,latest:{updateAvailable:false}}});
   if(path==='/api/chats')return route.fulfill({json:{chats:[]}});
   if(path==='/api/settings'&&request.method()==='GET')return route.fulfill({json:snapshot()});
   if(path==='/api/settings'){
    const body=request.postDataJSON();writes.push(body);
    if(failSave)return route.fulfill({status:409,body:'Configuration changed on disk; reload it before saving'});
    if(heldSave)await heldSave;
    assert.equal(body.revision,String(revision));
    if(body.raw!=null)document=JSON.parse(body.raw);
    else for(const [key,value] of Object.entries(body.changes)){const parts=key.split('.');let target=document;for(const part of parts.slice(0,-1))target=target[part]??={};target[parts.at(-1)]=value;}
    revision++;return route.fulfill({json:{...snapshot(),restartRequired:true}});
   }
   if(path==='/api/analytics/turn-cutoffs')return route.fulfill({json:{models:[],samples:0}});
   if(path==='/api/analytics/extras')return route.fulfill({status:503,body:'Optional extras unavailable'});
   throw Error(`Unexpected request: ${path}`);
  });
  await page.goto('http://127.0.0.1:43229/#test-token');await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Codexify settings'});
  await dialog.getByLabel('workDir',{exact:true}).waitFor();
  assert.equal(await dialog.locator('.settings-card').count(),1);
  assert.equal(await dialog.locator('#settings-nav button[aria-current=true]').textContent(),'General');
  const inputStyle=await dialog.getByLabel('workDir',{exact:true}).evaluate(input=>({border:getComputedStyle(input).borderTopWidth,background:getComputedStyle(input).backgroundColor}));
  assert.equal(inputStyle.border,'1px');assert.notEqual(inputStyle.background,'rgba(0, 0, 0, 0)');
  await dialog.getByRole('button',{name:'Workspaces',exact:true}).click();
  await dialog.getByLabel('worktrees.mode',{exact:true}).selectOption('');await page.clock.fastForward(700);
  await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
  assert.equal(Object.hasOwn(document.worktrees,'mode'),false,'Default removes the override rather than writing invalid null');
  assert.equal(document.worktrees.keepCount,5);assert.equal(document.mcpServers.local.command,'fixture-tools');assert.equal(typeof writes[0].raw,'string');
  await dialog.getByRole('button',{name:'Notifications',exact:true}).click();
  assert.equal(await dialog.getByRole('group',{name:'agentChat.notifications.events.completed',exact:true}).getByRole('button',{name:'On (default)',exact:true}).getAttribute('aria-pressed'),'true');
  await dialog.getByRole('group',{name:'agentChat.notifications.events.completed',exact:true}).getByRole('button',{name:'Off',exact:true}).click();
  await page.clock.fastForward(700);await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();assert.equal(document.agentChat.notifications.events.completed,false);
  await dialog.getByLabel('Search settings',{exact:true}).fill('blocked requests');
  assert.equal(await dialog.locator('[data-setting-row]').count(),1);
  const blocked=dialog.getByLabel('agentChat.automation.requestBlockList',{exact:true});
  const patterns=['^https?://(www\\.)?chatgpt\\.com/.*/rgstr','^https?://(www\\.)?chatgpt\\.com/backend-api/conversations'];
  await blocked.fill(patterns.join('\n'));await dialog.getByRole('button',{name:'General',exact:true}).click();await page.clock.fastForward(700);
  await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();assert.deepEqual(document.agentChat.automation.requestBlockList,patterns);
  await dialog.getByRole('button',{name:'Connections',exact:true}).click();
  const mcp=dialog.locator('[data-setting-row="mcpServers"]');assert.equal(await mcp.getByLabel('mcpServers',{exact:true}).isVisible(),false);
  const tunnels=dialog.locator('[data-setting-row="openaiTunnels"]');assert.equal(await tunnels.getByLabel('openaiTunnels',{exact:true}).evaluate(input=>input.tagName),'TEXTAREA');assert.equal(await tunnels.getByLabel('openaiTunnels',{exact:true}).isVisible(),false);
  await mcp.getByRole('button',{name:'Edit configuration'}).click();await mcp.getByLabel('mcpServers',{exact:true}).fill('{broken');
  await dialog.getByRole('button',{name:'Restart Codexify',exact:true}).click();await dialog.getByText('Fix the highlighted settings before restarting.',{exact:true}).waitFor();
  await dialog.getByRole('button',{name:'General',exact:true}).click();await dialog.getByRole('button',{name:'Connections',exact:true}).click();
  await mcp.getByRole('button',{name:'Edit configuration'}).click();assert.equal(await mcp.getByLabel('mcpServers',{exact:true}).inputValue(),'{broken','Invalid edits survive navigation');
  await mcp.getByLabel('mcpServers',{exact:true}).fill('{"local":{"command":"fixture-tools"}}');await page.clock.fastForward(700);
  await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
  // Navigation while a save is still pending must retain the visible draft.
  await dialog.getByRole('button',{name:'General',exact:true}).click();let release;heldSave=new Promise(resolve=>{release=resolve;});
  await dialog.getByLabel('workDir',{exact:true}).fill('/new-projects');await page.clock.fastForward(700);
  await dialog.getByRole('button',{name:'Workspaces',exact:true}).click();await dialog.getByRole('button',{name:'General',exact:true}).click();
  assert.equal(await dialog.getByLabel('workDir',{exact:true}).inputValue(),'/new-projects');release();heldSave=null;
  await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();assert.equal(document.workDir,'/new-projects');
  // Header/footer remain fixed when the content scrolls. The form fits phones.
  await dialog.getByLabel('Search settings',{exact:true}).fill('');await dialog.getByRole('button',{name:'Notifications',exact:true}).click();
  const before=await dialog.locator('#settings-toolbar').boundingBox();await dialog.locator('#settings-main').evaluate(element=>element.scrollTop=1000);
  assert.deepEqual(await dialog.locator('#settings-toolbar').boundingBox(),before);
  for(const colorScheme of ['light','dark']){await page.emulateMedia({colorScheme});await page.setViewportSize({width:375,height:650});
   const overflow=await dialog.evaluate(element=>({dialog:element.scrollWidth>element.clientWidth,content:element.querySelector('#settings-main').scrollWidth>element.querySelector('#settings-main').clientWidth}));assert.deepEqual(overflow,{dialog:false,content:false});
   const footer=await dialog.locator('.settings-footer').boundingBox();assert(footer&&footer.y>=0&&footer.y+footer.height<=650);
  }
  await page.setViewportSize({width:1280,height:900});await dialog.getByRole('button',{name:'Advanced',exact:true}).click();failSave=true;
  await dialog.getByRole('group',{name:'debug',exact:true}).getByRole('button',{name:'On',exact:true}).click();await page.clock.fastForward(700);
  await dialog.getByText('Configuration changed on disk; reload it before saving',{exact:true}).waitFor();
  await dialog.getByRole('button',{name:'General',exact:true}).click();await dialog.getByRole('button',{name:'Advanced',exact:true}).click();
  assert.equal(await dialog.getByRole('group',{name:'debug',exact:true}).getByRole('button',{name:'On',exact:true}).getAttribute('aria-pressed'),'true','Failed saves retain the edit');
  failSave=false;await dialog.getByRole('button',{name:'Reload settings',exact:true}).click();await dialog.getByText('All changes saved.',{exact:true}).waitFor();assert.equal(await dialog.getByRole('group',{name:'debug',exact:true}).getByRole('button',{name:'Off (default)',exact:true}).getAttribute('aria-pressed'),'true');
  assert.deepEqual(errors,[]);
 }finally{await browser.close();}
});

for(const [engineName,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${engineName}: real defaults are muted hints and only edited overrides are saved`,{timeout:30000},async()=>{
 const browser=await engine.launch();
 try {
  const page=await browser.newPage({viewport:{width:1280,height:900}});page.setDefaultTimeout(5000);await page.clock.install();
  const defaults={workDir:null,debug:false,uiWidgets:true,multiProject:false,exec:{defaultShell:'/bin/zsh',idleTimeoutMs:300000,maxSessions:8},filesystem:{maxConcurrentWorkers:4,maxLineBytes:1048576,maxPendingRequests:16},worktrees:{mode:'auto'},tree:{ignore:['node_modules','.git']},mcpServers:{},openaiTunnels:[]};
  const fieldSchema={...defaults,multiProject:true,exec:{defaultShell:null,idleTimeoutMs:999,maxSessions:999},mcpServers:{example:{command:'example'}},openaiTunnels:[{tunnelId:'example'}]};
  let document={workDir:'/projects'},revision=1;
  const writes=[],errors=[];page.on('pageerror',error=>errors.push(error.message));
  const snapshot=()=>({path:'/fixture/config.json',document,defaults,fieldSchema,revision:String(revision),raw:JSON.stringify(document)});
  await routeOwner(page,'http://127.0.0.1:43230/**',async route=>{
   const request=route.request(),path=new URL(request.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return route.fulfill({json:{version:'1.7.0',settingsAvailable:true,latest:{updateAvailable:false}}});
   if(path==='/api/chats')return route.fulfill({json:{chats:[]}});
   if(path==='/api/settings'&&request.method()==='GET')return route.fulfill({json:snapshot()});
   if(path==='/api/settings'){
    const body=request.postDataJSON();writes.push(body);assert.equal(body.revision,String(revision));
    if(body.raw!=null)document=JSON.parse(body.raw);
    else for(const [key,value] of Object.entries(body.changes)){const parts=key.split('.');let target=document;for(const part of parts.slice(0,-1))target=target[part]??={};target[parts.at(-1)]=value;}
    revision++;return route.fulfill({json:{...snapshot(),restartRequired:true}});
   }
   if(path==='/api/analytics/turn-cutoffs')return route.fulfill({json:{models:[],samples:0}});
   if(path==='/api/analytics/extras')return route.fulfill({status:503,body:'Optional extras unavailable'});
   throw Error(`Unexpected request: ${path}`);
  });
  await page.goto('http://127.0.0.1:43230/#test-token');await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Codexify settings'});
  assert.equal(await dialog.getByRole('group',{name:'multiProject',exact:true}).count(),0);
  const inheritedBoolean=dialog.getByRole('group',{name:'uiWidgets',exact:true}).getByRole('button',{name:'On (default)',exact:true});
  await inheritedBoolean.waitFor();assert.equal(await inheritedBoolean.getAttribute('aria-pressed'),'true');await inheritedBoolean.click();
  await dialog.getByRole('button',{name:'Tools & files',exact:true}).click();
  for(const [path,text] of Object.entries({'exec.defaultShell':'/bin/zsh (default)','exec.idleTimeoutMs':'300000 (default)','exec.maxSessions':'8 (default)','filesystem.maxConcurrentWorkers':'4 (default)','filesystem.maxLineBytes':'1048576 (default)','filesystem.maxPendingRequests':'16 (default)'})){
   const input=dialog.getByLabel(path,{exact:true});assert.equal(await input.inputValue(),'');assert.equal(await input.getAttribute('placeholder'),text);
   await input.focus();await input.blur();
  }
  await dialog.getByRole('button',{name:'Workspaces',exact:true}).click();
  const mode=dialog.getByLabel('worktrees.mode',{exact:true});assert.equal(await mode.locator('option:checked').textContent(),'Automatic (default)');
  assert.equal(await mode.evaluate(input=>getComputedStyle(input).color),await dialog.locator('#settings-content-description').evaluate(input=>getComputedStyle(input).color));
  await mode.selectOption('');
  await dialog.getByRole('button',{name:'Connections',exact:true}).click();
  const mcp=dialog.locator('[data-setting-row="mcpServers"]');assert.equal(await mcp.locator('.setting-object-summary span').textContent(),'{} (default)');
  await mcp.getByRole('button',{name:'Edit configuration'}).click();assert.equal(await mcp.getByLabel('mcpServers',{exact:true}).getAttribute('placeholder'),'{} (default)');
  await page.clock.fastForward(700);assert.deepEqual(writes,[]);assert.deepEqual(document,{workDir:'/projects'});
  await dialog.getByRole('button',{name:'Tools & files',exact:true}).click();
  // Entering the displayed default is an explicit edit; unrelated hints stay absent.
  const sessions=dialog.getByLabel('exec.maxSessions',{exact:true});await sessions.fill('8');await page.clock.fastForward(700);
  await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
  assert.deepEqual(writes[0].changes,{'exec.maxSessions':8});assert.deepEqual(document,{workDir:'/projects',exec:{maxSessions:8}});
  assert.equal(await sessions.getAttribute('data-default'),'false');assert.equal(await sessions.getAttribute('placeholder'),'');
  await sessions.fill('12');await page.clock.fastForward(700);await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();assert.equal(document.exec.maxSessions,12);
  await sessions.fill('');await page.clock.fastForward(700);await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
  assert.deepEqual(document.exec,{});assert.equal(await sessions.getAttribute('placeholder'),'8 (default)');
  await dialog.getByLabel('tree.ignore',{exact:true}).fill('custom');await page.clock.fastForward(700);await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
  await dialog.getByLabel('tree.ignore',{exact:true}).fill('');await page.clock.fastForward(700);await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
  assert.deepEqual(document.tree.ignore,[]);assert.equal(await dialog.getByLabel('tree.ignore',{exact:true}).getAttribute('placeholder'),'');
  assert.deepEqual(errors,[]);
 }finally{await browser.close();}
});

for(const [engineName,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${engineName}: connector refresh queues once and survives closing settings`,{timeout:30000},async()=>{
 const browser=await engine.launch();
 try {
  const page=await browser.newPage();page.setDefaultTimeout(5000);await page.clock.install();
  let refresh=null,submissions=0,holdGlobal=false,oldReadStarted,releaseOldRead;
  const oldRead=new Promise(resolve=>{oldReadStarted=resolve;});
  const oldReadGate=new Promise(resolve=>{releaseOldRead=resolve;});
  const errors=[];page.on('pageerror',error=>errors.push(error.message));
  await routeOwner(page,'http://127.0.0.1:43231/**',async route=>{
   const request=route.request(),path=new URL(request.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/chats')return route.fulfill({json:{chats:[]}});
   if(path==='/api/global'){
    const response={version:'1.7.0',settingsAvailable:true,restartAvailable:true,latest:{updateAvailable:false},connectorRefresh:refresh&&{...refresh}};
    if(holdGlobal){holdGlobal=false;oldReadStarted();await oldReadGate;}
    return route.fulfill({json:response});
   }
   if(path==='/api/settings')return route.fulfill({json:{document:{workDir:'/projects'},defaults:{workDir:null},revision:'1',path:'/fixture/config.json',raw:'{"workDir":"/projects"}'}});
   if(path==='/api/settings/refresh-connector'){
    submissions++;refresh={refreshId:`refresh-${submissions}`,source:'manual',phase:'queued'};
    return route.fulfill({json:refresh});
   }
   if(path==='/api/analytics/turn-cutoffs')return route.fulfill({json:{models:[],samples:0}});
   if(path==='/api/analytics/extras')return route.fulfill({status:503,body:'Optional extras unavailable'});
   throw Error(`Unexpected request: ${path}`);
  });
  await page.goto('http://127.0.0.1:43231/#test-token');await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Codexify settings'});await dialog.getByLabel('workDir',{exact:true}).waitFor();
  // An older status response must not erase the newly queued job.
  holdGlobal=true;await page.clock.fastForward(15000);await oldRead;
  const button=dialog.locator('#settings-refresh-connector');await button.click();
  await dialog.getByText('Connector refresh queued. Waiting for browser access.',{exact:true}).waitFor();
  assert.equal(await button.textContent(),'Queued…');assert.equal(await button.isDisabled(),true);
  releaseOldRead();await page.clock.fastForward(100);
  assert.equal(await button.textContent(),'Queued…');await button.dispatchEvent('click');assert.equal(submissions,1);
  refresh={...refresh,phase:'running'};await page.clock.fastForward(3100);
  await dialog.getByText('Refreshing connector tools…',{exact:true}).waitFor();assert.equal(await button.textContent(),'Refreshing…');assert.equal(await button.isDisabled(),true);
  await dialog.getByRole('button',{name:'Close settings',exact:true}).click();
  refresh={...refresh,phase:'completed',result:{refreshObserved:true}};await page.clock.fastForward(3100);
  await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  await dialog.getByText('Connector refresh completed; Allow all tools is selected.',{exact:true}).waitFor();assert.equal(await button.textContent(),'Refresh connector');assert.equal(await button.isDisabled(),false);assert.equal(submissions,1);
  await button.click();await dialog.getByText('Connector refresh queued. Waiting for browser access.',{exact:true}).waitFor();assert.equal(submissions,2);
  refresh={...refresh,phase:'failed',result:{error:'Connector settings changed.'}};await page.clock.fastForward(3100);
  await dialog.getByText('Connector settings changed.',{exact:true}).waitFor();assert.equal(await dialog.locator('#settings-save-state').getAttribute('data-error'),'true');assert.equal(await button.isDisabled(),false);
  assert.deepEqual(errors,[]);
 }finally{await browser.close();}
});

for(const [engineName,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${engineName}: annotation overrides support inherited hints, true and false without saving untouched defaults`,{timeout:30000},async()=>{
 const browser=await engine.launch();
 try {
  const page=await browser.newPage({viewport:{width:1280,height:900}});page.setDefaultTimeout(5000);await page.clock.install();
  const keys=['forceDestructiveToolAnnotations','forceIdempotentToolAnnotations','forceOpenWorldToolAnnotations'];
  const hints=Object.fromEntries(keys.map(key=>[key,null]));
  let document={workDir:'/fixture',experimental:{forceReadOnlyToolAnnotations:true}},revision=1;
  const writes=[],errors=[];page.on('pageerror',error=>errors.push(error.message));
  const snapshot=()=>({document,raw:JSON.stringify(document,null,2),revision:String(revision),defaults:{experimental:{forceReadOnlyToolAnnotations:false,...hints}},fieldSchema:{experimental:{forceReadOnlyToolAnnotations:false,...hints}},defaultLabels:Object.fromEntries(keys.map(key=>['experimental.'+key,'Keep tool hints']))});
  await routeOwner(page,'http://127.0.0.1:43240/**',route=>{
   const request=route.request(),path=new URL(request.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   assert.equal(request.headers().authorization,'Bearer test-token');
   if(path==='/api/chats')return route.fulfill({json:{chats:[]}});
   if(path==='/api/global')return route.fulfill({json:{version:'1.7.0',settingsAvailable:true,restartAvailable:true,latest:{updateAvailable:false}}});
   if(path==='/api/settings'&&request.method()==='GET')return route.fulfill({json:snapshot()});
   if(path==='/api/settings'){
    const body=request.postDataJSON();writes.push(body);assert.equal(body.revision,String(revision));
    if(body.raw!=null)document=JSON.parse(body.raw);
    else for(const [path,value]of Object.entries(body.changes)){const parts=path.split('.');let target=document;for(const part of parts.slice(0,-1))target=target[part]??={};target[parts.at(-1)]=value;}
    revision++;return route.fulfill({json:{...snapshot(),restartRequired:true}});
   }
   if(path==='/api/analytics/turn-cutoffs')return route.fulfill({json:{models:[],samples:0}});
   if(path==='/api/analytics/extras')return route.fulfill({status:503,body:'Optional extras unavailable'});
   throw Error('Unexpected request: '+path);
  });
  await page.goto('http://127.0.0.1:43240/#test-token');await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Codexify settings'});await dialog.getByRole('button',{name:'Advanced',exact:true}).click();
  for(const key of keys){const group=dialog.getByRole('group',{name:'experimental.'+key,exact:true});await group.waitFor();assert.equal(await group.getByRole('button',{name:'Keep tool hints (default)',exact:true}).getAttribute('aria-pressed'),'true');assert.equal(await group.getByRole('button',{name:'True',exact:true}).count(),1);assert.equal(await group.getByRole('button',{name:'False',exact:true}).count(),1);}
  await page.clock.fastForward(1000);assert.equal(writes.length,0,'opening settings must not materialize three null defaults');
  for(const key of keys){
   const group=()=>dialog.getByRole('group',{name:'experimental.'+key,exact:true});
   for(const [label,value]of [['True',true],['False',false]]){
    await group().getByRole('button',{name:label,exact:true}).click();await page.clock.fastForward(700);await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
    assert.equal(document.experimental[key],value);assert.equal(document.experimental.forceReadOnlyToolAnnotations,true);
    assert.equal(await group().getByRole('button',{name:label,exact:true}).getAttribute('aria-pressed'),'true');
   }
   await group().getByRole('button',{name:'Keep tool hints (default)',exact:true}).click();await page.clock.fastForward(700);await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
   assert.equal(Object.hasOwn(document.experimental,key),false,'reset removes just the override');
   assert.equal(await group().getByRole('button',{name:'Keep tool hints (default)',exact:true}).getAttribute('aria-pressed'),'true');
  }
  assert.deepEqual(document.experimental,{forceReadOnlyToolAnnotations:true});assert.equal(writes.length,9);
  for(const width of [1280,390])for(const theme of ['light','dark']){
   await page.setViewportSize({width,height:900});await page.emulateMedia({colorScheme:theme});
   assert.equal(await dialog.evaluate(element=>element.scrollWidth>element.clientWidth),false);
   for(const key of keys)assert.equal(await dialog.getByRole('group',{name:'experimental.'+key,exact:true}).evaluate(element=>element.scrollWidth>element.clientWidth),false);
   if(process.env.CODEXIFY_SETTINGS_SCREENSHOTS){mkdirSync(process.env.CODEXIFY_SETTINGS_SCREENSHOTS,{recursive:true});await page.screenshot({path:`${process.env.CODEXIFY_SETTINGS_SCREENSHOTS}/${engineName.toLowerCase()}-${width}-${theme}-annotations.png`,fullPage:true});}
  }
  assert.deepEqual(errors,[]);
 }finally{await browser.close();}
});

for(const [engineName,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${engineName}: missing plans stay hidden and footer controls remain aligned`,{timeout:30000},async()=>{
 const browser=await engine.launch();
 try {
  const page=await browser.newPage();page.setDefaultTimeout(5000);await page.clock.install();
  let plan=null,fail=false;
  await routeOwner(page,'http://127.0.0.1:43241/**',async route=>{
   const path=new URL(route.request().url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return fail?route.fulfill({status:503,body:'Unavailable'}):route.fulfill({json:{version:'1.7.0',plan,latest:{updateAvailable:false}}});
   if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
   if(path==='/api/analytics/turn-cutoffs')return route.fulfill({json:{models:[],samples:0}});
   if(path==='/api/analytics/extras')return route.fulfill({status:503,body:'Optional extras unavailable'});
   throw Error('Unexpected request: '+path);
  });
  await page.goto('http://127.0.0.1:43241/#test-token');await page.getByText('v1.7.0',{exact:true}).waitFor();
  const label=page.locator('#account-plan');
  assert.equal(await label.isVisible(),false);assert.equal(await label.textContent(),'');
  for(const width of [1280,390]){
   await page.setViewportSize({width,height:900});
   const bounds=await page.locator('#global-footer').evaluate(footer=>{
    const actions=footer.querySelector('#global-footer-actions').getBoundingClientRect(),version=footer.querySelector('#global-version').getBoundingClientRect(),gear=footer.querySelector('#global-settings').getBoundingClientRect(),box=footer.getBoundingClientRect();
    return {overflow:footer.scrollWidth>footer.clientWidth,rightGap:box.right-actions.right,aligned:Math.abs((gear.top+gear.bottom)/2-(version.top+version.bottom)/2)<1};
   });
   assert.equal(bounds.overflow,false);assert(bounds.rightGap>=9&&bounds.rightGap<=17);assert.equal(bounds.aligned,true);
   if(process.env.CODEXIFY_SETTINGS_SCREENSHOTS){mkdirSync(process.env.CODEXIFY_SETTINGS_SCREENSHOTS,{recursive:true});await page.locator('aside').screenshot({path:`${process.env.CODEXIFY_SETTINGS_SCREENSHOTS}/${engineName.toLowerCase()}-${width}-no-plan.png`});}
  }
  plan={label:'Pro',sourceLabel:'Pro',kind:'pro'};await page.clock.fastForward(15000);await label.waitFor({state:'visible'});assert.equal(await label.textContent(),'Pro');
  plan={label:'',sourceLabel:'',kind:'unknown'};await page.clock.fastForward(15000);await label.waitFor({state:'hidden'});assert.equal(await label.textContent(),'');assert.equal(await label.getAttribute('title'),'');
  fail=true;await page.reload();
  assert.equal(await label.isVisible(),false);assert.equal(await label.textContent(),'');assert.equal(await page.getByText('Plan unavailable',{exact:true}).count(),0);
 }finally{await browser.close();}
});


for(const [engineName,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${engineName}: progress report interval defaults to twenty-five and saves without restart`,{timeout:20000},async()=>{
 const browser=await engine.launch();try{
  const page=await browser.newPage();page.setDefaultTimeout(5000);
  const defaults=JSON.parse(readFileSync(new URL('../codexify.config.example.json',import.meta.url)));
  let document={workDir:'/fixture',agentChat:{enabled:true},codexMcp:{enabled:false}},revision=1;
  const writes=[],errors=[];page.on('pageerror',error=>errors.push(error.message));
  const snapshot=()=>({path:'/fixture/config.json',defaults,document,raw:JSON.stringify(document),revision:String(revision),restartRequired:false});
  await routeOwner(page,'http://127.0.0.1:43316/**',async route=>{
   const request=route.request(),path=new URL(request.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return route.fulfill({json:{version:'2.0.0',plan:null,settingsAvailable:true,restartAvailable:true,latest:{updateAvailable:false}}});
   if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
   if(path==='/api/settings'&&request.method()==='GET')return route.fulfill({json:snapshot()});
   if(path==='/api/settings'){
    const body=request.postDataJSON();writes.push([path,body]);
    if(body.raw)document=JSON.parse(body.raw);else{assert.deepEqual(Object.keys(body.changes),['agentChat.rolloutReminderInterval']);document.agentChat.rolloutReminderInterval=body.changes['agentChat.rolloutReminderInterval'];}
    revision++;return route.fulfill({json:snapshot()});
   }
   if(request.method()!=='GET')writes.push([path,request.postData()]);return route.fulfill({status:404});
  });
  await page.goto('http://127.0.0.1:43316/#test-token');await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Codexify settings'});await dialog.getByRole('button',{name:'Chats & goals',exact:true}).click();
  const interval=dialog.getByLabel('agentChat.rolloutReminderInterval',{exact:true});await interval.waitFor();assert.equal(await interval.inputValue(),'');assert.equal(await interval.getAttribute('placeholder'),'25 (default)');
  await interval.fill('50');await dialog.getByText('All changes saved.',{exact:true}).waitFor();assert.equal(document.agentChat.rolloutReminderInterval,50);
  assert.deepEqual(writes[0][1].changes,{'agentChat.rolloutReminderInterval':50});
  await interval.fill('');await dialog.getByText('All changes saved.',{exact:true}).waitFor();await page.waitForFunction(()=>document.querySelector('[data-setting="agentChat.rolloutReminderInterval"]').dataset.default==='true');
  assert.equal(document.agentChat.rolloutReminderInterval,undefined);assert.equal(await interval.getAttribute('placeholder'),'25 (default)');assert.equal(writes.length,2);assert.deepEqual(errors,[]);
 }finally{await browser.close();}
});

for(const [engineName,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${engineName}: renewal limits remain editable while off and independent from progress reporting`,{timeout:25000},async()=>{
 const browser=await engine.launch();try{
  const page=await browser.newPage();page.setDefaultTimeout(5000);
  const defaults=JSON.parse(readFileSync(new URL('../codexify.config.example.json',import.meta.url)));
  let document={workDir:'/fixture',agentChat:{enabled:true}},revision=1;const writes=[],errors=[];page.on('pageerror',e=>errors.push(e.message));
  const snapshot=()=>({path:'/fixture/config.json',defaults,document,raw:JSON.stringify(document),revision:String(revision),restartRequired:false});
  await routeOwner(page,'http://127.0.0.1:43317/**',async route=>{
   const req=route.request(),path=new URL(req.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return route.fulfill({json:{version:'2.0.0',plan:null,settingsAvailable:true,restartAvailable:true,latest:{updateAvailable:false}}});
   if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
   if(path==='/api/settings'&&req.method()==='GET')return route.fulfill({json:snapshot()});
   if(path==='/api/settings'){
    const body=req.postDataJSON();writes.push(body);for(const [key,val] of Object.entries(body.changes)){const parts=key.split('.');let target=document;for(const part of parts.slice(0,-1))target=target[part]??={};target[parts.at(-1)]=val;}
    revision++;return route.fulfill({json:snapshot()});
   }
   return route.fulfill({status:404});
  });
  await page.goto('http://127.0.0.1:43317/#fixture');await page.getByRole('button',{name:'Codexify settings',exact:true}).click();const dialog=page.getByRole('dialog',{name:'Codexify settings'});await dialog.getByRole('button',{name:'Chats & goals',exact:true}).click();
  const pro=dialog.getByLabel('gpt-5.6-sol max turn limit in minutes',{exact:true});assert.equal(await pro.inputValue(),'115');assert.equal(await pro.isEnabled(),true);assert.equal(await dialog.getByLabel('gpt-6-astra max turn limit in minutes',{exact:true}).inputValue(),'55');assert.equal(await dialog.locator('.setting-turn-limit').count(),15);
  await pro.fill('110');await dialog.getByText('All changes saved.',{exact:true}).waitFor();assert.equal(document.agentChat.turnRenewal.limits.find(l=>l.model==='gpt-5.6-sol'&&l.effort==='max').minutes,110);assert.equal(document.agentChat.turnRenewal.enabled,undefined);
  await dialog.getByRole('group',{name:'agentChat.rolloutEnabled',exact:true}).getByRole('button',{name:'Off',exact:true}).click();await dialog.getByText('All changes saved.',{exact:true}).waitFor();
  await dialog.getByRole('group',{name:'agentChat.turnRenewal.enabled',exact:true}).getByRole('button',{name:'On',exact:true}).click();await page.waitForFunction(()=>document.querySelector('[data-setting-row="agentChat.turnRenewal.enabled"] button[aria-pressed="true"]').textContent==='On');await dialog.getByText('All changes saved.',{exact:true}).waitFor();assert.equal(document.agentChat.rolloutEnabled,false);assert.equal(document.agentChat.turnRenewal.enabled,true);
  assert.equal(await dialog.getByRole('checkbox').count(),15);assert.equal(await dialog.locator('.setting-turn-limit input[type=checkbox]:checked').count(),15);
  const limits=dialog.locator('[data-setting-row="agentChat.turnRenewal.limits"]');
  await limits.getByRole('button',{name:'Disable all',exact:true}).click();await dialog.getByText('All changes saved.',{exact:true}).waitFor();assert(document.agentChat.turnRenewal.limits.every(limit=>limit.enabled===false));
  await dialog.getByRole('checkbox',{name:'Renew GPT-5.6 Sol Pro',exact:true}).check();await dialog.getByText('All changes saved.',{exact:true}).waitFor();assert.deepEqual(document.agentChat.turnRenewal.limits.filter(limit=>limit.enabled).map(limit=>[limit.model,limit.effort,limit.minutes]),[['gpt-5.6-sol','max',110]]);
  await dialog.getByRole('button',{name:'Close settings',exact:true}).click();document.agentChat.turnRenewal.limits=[{model:'gpt-5.6-sol',effort:'max',minutes:115}];revision++;
  await page.getByRole('button',{name:'Codexify settings',exact:true}).click();await dialog.getByRole('button',{name:'Chats & goals',exact:true}).click();
  assert.equal(await dialog.locator('.setting-turn-limit').count(),15);assert.equal(await dialog.getByRole('checkbox',{name:'Renew GPT-5.6 Sol Pro',exact:true}).isChecked(),true);assert.equal(await dialog.getByRole('checkbox',{name:'Renew GPT-6 Astra Pro',exact:true}).isChecked(),false);assert.equal(await dialog.locator('.setting-turn-limit input[type=checkbox]:checked').count(),1);
  await page.setViewportSize({width:390,height:844});await pro.scrollIntoViewIfNeeded();await page.screenshot({path:`/tmp/turn-renewal-settings-${engineName}.png`});assert(await dialog.evaluate(e=>e.scrollWidth<=e.clientWidth));assert.deepEqual(errors,[]);assert.equal(writes.length,5);
 }finally{await browser.close();}
});


for(const [engineName,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${engineName}: cutoff estimates are read-only and optional failures stay silent`,{timeout:30000},async()=>{
 const browser=await engine.launch();
 try {
  const page=await browser.newPage({viewport:{width:1280,height:900}});page.setDefaultTimeout(5000);
  const errors=[],writes=[];page.on('pageerror',error=>errors.push(error.message));let fail=false;
  const config={agentChat:{turnRenewal:{enabled:false,limits:[]}}};
  await routeOwner(page,'http://127.0.0.1:43242/**',async route=>{
   const path=new URL(route.request().url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(route.request().method()!=='GET')writes.push(path);
   if(path==='/api/global')return route.fulfill({json:{version:'2.0.0',settingsAvailable:true}});
   if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
   if(path==='/api/settings')return route.fulfill({json:{document:config,defaults:config,revision:'1',raw:JSON.stringify(config),path:'/fixture/config.json'}});
   if(path==='/api/analytics/turn-cutoffs')return fail==='malformed'?route.fulfill({json:{models:[null,{},42,{model:'invalid',effort:'max',samples:-1}]}}):fail?route.fulfill({status:500,body:'private disk error'}):route.fulfill({json:{samples:6,models:[{model:'gpt-5.6-sol',effort:'max',samples:6,medianMs:6900000,p90Ms:7200000,minMs:6600000,maxMs:7200000}]}});
   return route.fulfill({status:404});
  });
  await page.goto('http://127.0.0.1:43242/#test-token');await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Codexify settings'});await dialog.getByRole('button',{name:'Chats & goals',exact:true}).click();
  const table=dialog.getByRole('table',{name:'Model cutoff estimates'});await table.getByText('gpt-5.6-sol / max',{exact:true}).waitFor();
  assert.deepEqual(await table.locator('tbody td').allTextContents(),['gpt-5.6-sol / max','6','1h 55m','2h','1h 50m–2h']);
  await page.setViewportSize({width:375,height:850});const card=dialog.locator('#cutoff-estimates');const box=await card.boundingBox();assert(box&&box.width<=375);assert.equal(await dialog.evaluate(el=>el.scrollWidth>el.clientWidth),false);
  assert.deepEqual(writes,[]);assert.deepEqual(errors,[]);
  await dialog.getByRole('button',{name:'Close settings',exact:true}).click();fail=true;
  await page.getByRole('button',{name:'Codexify settings',exact:true}).click();await dialog.getByRole('button',{name:'Chats & goals',exact:true}).click();
  await dialog.locator('#cutoff-estimates').waitFor();assert.equal(await dialog.locator('#settings-save-state').getAttribute('data-error'),'false');assert.equal(await page.getByText('private disk error').count(),0);assert.deepEqual(writes,[]);assert.deepEqual(errors,[]);
  await dialog.getByRole('button',{name:'Close settings',exact:true}).click();fail='malformed';
  await page.getByRole('button',{name:'Codexify settings',exact:true}).click();await dialog.getByRole('button',{name:'Chats & goals',exact:true}).click();
  await dialog.getByText('No cutoff estimates yet.',{exact:true}).waitFor();
  await dialog.getByRole('button',{name:'General',exact:true}).click();await dialog.getByRole('button',{name:'Chats & goals',exact:true}).click();
  assert.equal(await dialog.locator('#settings-save-state').getAttribute('data-error'),'false');assert.deepEqual(errors,[]);assert.deepEqual(writes,[]);
 }finally{await browser.close();}
});

for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: automatic retries is an immediate Chats and goals setting`,{timeout:20000},async()=>{
 const browser=await engine.launch();
 try{
  const page=await browser.newPage();page.setDefaultTimeout(5000);await page.clock.install();
  let enabled,revision=1;const writes=[];
  const snapshot=()=>({document:enabled===undefined?{}:{agentChat:{automation:{automaticRetries:enabled}}},defaults:{agentChat:{automation:{automaticRetries:true}}},fieldSchema:{agentChat:{automation:{automaticRetries:true}}},raw:'{}',revision:String(revision),restartRequired:false,schemaRefreshRequired:false});
  await routeOwner(page,'http://127.0.0.1:43277/**',route=>{
   const req=route.request(),path=new URL(req.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return route.fulfill({json:{version:'2.3.3',settingsAvailable:true}});
   if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
   if(path==='/api/settings'&&req.method()==='GET')return route.fulfill({json:snapshot()});
   if(path==='/api/settings'){
    const body=req.postDataJSON();writes.push(body);enabled=body.changes['agentChat.automation.automaticRetries'];revision++;
    return route.fulfill({json:snapshot()});
   }
   return route.fulfill({status:404});
  });
  await page.goto('http://127.0.0.1:43277/#test-token');await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Codexify settings'});await dialog.getByRole('button',{name:'Chats & goals',exact:true}).click();
  const toggle=dialog.locator('[data-setting="agentChat.automation.automaticRetries"]');
  await toggle.waitFor();assert.equal(await toggle.getByRole('button',{name:'On (default)',exact:true}).getAttribute('aria-pressed'),'true');
  const disabled=page.waitForResponse(response=>response.url().endsWith('/api/settings')&&response.request().method()==='POST');
  await toggle.getByRole('button',{name:'Off',exact:true}).click();await page.clock.fastForward(700);await disabled;
  assert.equal(enabled,false);await dialog.getByText('All changes saved.',{exact:true}).waitFor();
  const saved=page.waitForResponse(response=>response.url().endsWith('/api/settings')&&response.request().method()==='POST');
  await toggle.getByRole('button',{name:'On',exact:true}).click();await page.clock.fastForward(700);await saved;
  assert.equal(enabled,true);assert.equal(writes.length,2);
 }finally{await browser.close();}
});

for(const [name,engine]of [['Chromium',chromium],['WebKit',webkit]])test(`${name}: retry timings use duration editors, save in seconds, validate, and reset`,{timeout:25000},async()=>{
 const browser=await engine.launch();
 try{
  const page=await browser.newPage();page.setDefaultTimeout(5000);await page.clock.install();
  const errors=[];page.on('pageerror',error=>errors.push(String(error)));
  const retryTimings=JSON.parse(readFileSync(new URL('../codexify.config.example.json',import.meta.url),'utf8')).agentChat.automation.retryTimings;
  const defaults={agentChat:{automation:{automaticRetries:true,retryTimings}}};
  let document={},revision=1;const writes=[];
  const snapshot=()=>({document,defaults,fieldSchema:defaults,raw:JSON.stringify(document),revision:String(revision),restartRequired:revision>1,schemaRefreshRequired:false});
  await routeOwner(page,'http://127.0.0.1:43278/**',route=>{
   const req=route.request(),path=new URL(req.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return route.fulfill({json:{version:'2.3.9',settingsAvailable:true}});
   if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
   if(path==='/api/settings'&&req.method()==='GET')return route.fulfill({json:snapshot()});
   if(path==='/api/settings'){
    const body=req.postDataJSON();writes.push(body);
    if(body.raw!==undefined)document=JSON.parse(body.raw);
    else for(const [path,value]of Object.entries(body.changes)){const parts=path.split('.');let node=document;for(const key of parts.slice(0,-1))node=node[key]??={};node[parts.at(-1)]=value;}
    revision++;return route.fulfill({json:snapshot()});
   }
   return route.fulfill({status:404});
  });
  await page.goto('http://127.0.0.1:43278/#test-token');await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Codexify settings'});await dialog.getByRole('button',{name:'Chats & goals',exact:true}).click();
  const field=key=>dialog.getByLabel(`agentChat.automation.retryTimings.${key}`,{exact:true});
  assert.equal(await dialog.locator('[data-setting^="agentChat.automation.retryTimings."]').count(),8);
  assert.equal(await field('chatgptAccessDelaysSeconds').getAttribute('placeholder'),'3m, 5m, 10m, 20m, 30m, 1h (default)');
  assert.equal(await field('chatgptAccessDelaysSeconds').inputValue(),'');assert.deepEqual(writes,[]);
  await field('actionDelaysSeconds').fill('10s, 2m');
  await field('chatgptAccessDelaysSeconds').fill('1m, 2h, 86400');
  await field('goalRecoveryDelaysSeconds').fill('11s, 31s, 61s, 91s');
  for(const [key,value]of Object.entries({jitterPercent:0,approvalResumeGraceSeconds:17,conversationLoadIntervalSeconds:41,conversationUnavailableCooldownSeconds:83,rateLimitCooldownSeconds:167}))await field(key).fill(String(value));
  await page.clock.fastForward(700);await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
  assert.deepEqual(document.agentChat.automation.retryTimings,{actionDelaysSeconds:[10,120],chatgptAccessDelaysSeconds:[60,7200,86400],goalRecoveryDelaysSeconds:[11,31,61,91],jitterPercent:0,approvalResumeGraceSeconds:17,conversationLoadIntervalSeconds:41,conversationUnavailableCooldownSeconds:83,rateLimitCooldownSeconds:167});
  const savedCount=writes.length;
  for(const invalid of ['0s','1.5m','25h','1m,','wat',Array(33).fill('1s').join(',')]){
   await field('actionDelaysSeconds').fill(invalid);await page.clock.fastForward(700);
   assert.equal(await field('actionDelaysSeconds').getAttribute('aria-invalid'),'true');assert.equal(writes.length,savedCount);
  }
  await field('jitterPercent').fill('101');await page.clock.fastForward(700);
  assert.equal(await field('jitterPercent').getAttribute('aria-invalid'),'true');assert.equal(writes.length,savedCount);
  await field('jitterPercent').fill('0');await field('actionDelaysSeconds').fill('10s, 2m');
  await page.clock.fastForward(700);assert.equal(await field('jitterPercent').getAttribute('aria-invalid'),null);assert.equal(await field('actionDelaysSeconds').getAttribute('aria-invalid'),null);
  await dialog.getByRole('button',{name:'Use default for ChatGPT access retry delays',exact:true}).click();await page.clock.fastForward(700);
  await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
  assert.equal(document.agentChat.automation.retryTimings.chatgptAccessDelaysSeconds,undefined);
  assert.equal(document.agentChat.automation.retryTimings.rateLimitCooldownSeconds,167);
  await dialog.getByRole('button',{name:'Close settings',exact:true}).click();await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  await dialog.getByRole('button',{name:'Chats & goals',exact:true}).click();
  assert.equal(await field('actionDelaysSeconds').inputValue(),'10s, 2m');assert.equal(await field('chatgptAccessDelaysSeconds').inputValue(),'');
  await page.setViewportSize({width:390,height:850});
  assert.equal(await dialog.evaluate(element=>element.scrollWidth>element.clientWidth),false);
  assert.deepEqual(errors,[]);
 }finally{await browser.close();}
});

for(const [engineName,engine] of [['Chromium',chromium],['WebKit',webkit]])test(`${engineName}: offline timeout saves whole minutes, validates bounds and resets to the default`,{timeout:20000},async()=>{
 const browser=await engine.launch();
 try {
  const page=await browser.newPage();page.setDefaultTimeout(5000);await page.clock.install();
  let document={},revision=1;const writes=[];
  const snapshot=()=>({document,defaults:{agentChat:{offlineTimeoutMinutes:5}},fieldSchema:{agentChat:{offlineTimeoutMinutes:5}},raw:JSON.stringify(document),revision:String(revision),restartRequired:revision>1});
  await routeOwner(page,'http://127.0.0.1:43279/**',route=>{
   const r=route.request(),path=new URL(r.url()).pathname;
   if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
   if(path==='/api/global')return route.fulfill({json:{version:'fixture',settingsAvailable:true}});
   if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
   if(path==='/api/settings'&&r.method()==='GET')return route.fulfill({json:snapshot()});
   if(path==='/api/settings'){
    const body=r.postDataJSON();writes.push(body);
    if(body.raw!=null)document=JSON.parse(body.raw);
    else for(const [key,value]of Object.entries(body.changes||{})){
     assert.equal(key,'agentChat.offlineTimeoutMinutes');document.agentChat??={};
     if(value==null)delete document.agentChat.offlineTimeoutMinutes;else document.agentChat.offlineTimeoutMinutes=value;
    }
    revision++;return route.fulfill({json:snapshot()});
   }
   return route.fulfill({status:404});
  });
  await page.goto('http://127.0.0.1:43279/#test-token');
  await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Codexify settings'});
  await dialog.getByRole('button',{name:'Chats & goals',exact:true}).click();
  const input=dialog.getByLabel('agentChat.offlineTimeoutMinutes',{exact:true});
  assert.equal(await input.getAttribute('placeholder'),'5 (default)');
  assert.match(await dialog.locator('[data-setting-row="agentChat.offlineTimeoutMinutes"]').textContent(),/Restart to apply/);
  for(const value of ['0','1.5','1441']){
   await input.fill(value);await page.clock.fastForward(700);
   assert.equal(await input.getAttribute('aria-invalid'),'true');assert.equal(writes.length,0);
  }
  await input.fill('10');await page.clock.fastForward(700);
  await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
  assert.equal(document.agentChat.offlineTimeoutMinutes,10);
  await input.fill('');await page.clock.fastForward(700);
  await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
  assert.equal(document.agentChat?.offlineTimeoutMinutes,undefined);
 }finally{await browser.close();}
});
