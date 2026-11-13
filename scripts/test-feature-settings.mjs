import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {createRequire} from 'node:module';
import {fileURLToPath} from 'node:url';
import test from 'node:test';
import {ownerHtml,routeOwner} from './owner-chat-source.mjs';

const {chromium}=createRequire(new URL('../browser-companion/package.json',import.meta.url))('patchright');
const chromeRoot=new URL(`../browser-companion/.browsers/${process.platform}-${process.arch}/`,import.meta.url);
const chrome=JSON.parse(readFileSync(new URL('chrome-executable.json',chromeRoot)));
const launch=()=>chromium.launch({executablePath:fileURLToPath(new URL(chrome.executable,chromeRoot))});
const example=JSON.parse(readFileSync(new URL('../codexify.config.example.json',import.meta.url)));
const switches=[
  'agentChat.features.agentTeams','agentChat.features.externalAgentControl','agentChat.features.autoMode',
  'agentChat.features.agentFolders','agentChat.features.conversationRecovery',
  'agentChat.automation.recoveryEnhancements',
  'agentChat.automation.retryTimings.infiniteRetries',
];
const at=(value,path)=>path.split('.').reduce((value,key)=>value?.[key],value);

const fastSetupChanges={
  'agentChat.enabled':true,
  'agentChat.automation.browserSimplicity':'bones',
  'agentChat.defaultMessageMode':'steer',
  'agentChat.automation.enabled':true,
  'agentChat.automation.recoveryEnhancements':true,
  'agentChat.automation.automaticRetries':true,
  'agentChat.automation.retryTimings.infiniteRetries':true,
  'agentChat.automation.retryTimings.conversationLoadIntervalSeconds':1,
  'agentChat.rolloutEnabled':true,
  'agentChat.features.agentTeams':true,
  'agentChat.features.autoMode':true,
  'agentChat.features.conversationRecovery':true,
  'agentChat.turnRenewal.enabled':true,
  'agentChat.extras.animations':false,
  'agentChat.extras.sounds':false,
  'agentChat.extras.weeklyRecap':false,
};

async function fastSetupFixture({failures=0,hold=false}={}) {
  const browser=await launch();
  try {
    const page=await browser.newPage();page.setDefaultTimeout(5000);await page.clock.install();
    let document={instanceName:'Keep this name',agentChat:{extras:{animations:true,sounds:true,weeklyRecap:true},features:{externalAgentControl:false,agentFolders:false},automation:{autoApproveToolCalls:false,requestBlockingEnabled:false,retryTimings:{infiniteRetries:false}},turnRenewal:{enabled:false,limits:structuredClone(example.agentChat.turnRenewal.limits)}}},revision=1,update=null,releaseSave,resolveStarted;
    const started=new Promise(resolve=>{resolveStarted=resolve;});
    const posts=[],writes=[],errors=[];page.on('pageerror',error=>errors.push(error.message));
    const snapshot=()=>({document,defaults:example,fieldSchema:example,raw:JSON.stringify(document),revision:String(revision),restartRequired:revision>1,
      fastSetup:{changes:fastSetupChanges,applied:Object.entries(fastSetupChanges).every(([key,value])=>at(document,key)===value)}});
    await routeOwner(page,'http://127.0.0.1:43274/**',async route=>{
      const request=route.request(),path=new URL(request.url()).pathname;
      if(request.method()==='POST')posts.push(path);
      if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
      if(path==='/api/global')return route.fulfill({json:{version:'2.3.15',settingsAvailable:true,restartAvailable:true,update}});
      if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
      if(path==='/api/browser')return route.fulfill({json:{companion:{authenticated:true,status:'idle'}}});
      if(path==='/api/settings'&&request.method()==='GET')return route.fulfill({json:snapshot()});
      if(path==='/api/settings'){
        const body=request.postDataJSON();writes.push(body);assert.equal(body.revision,String(revision));
        if(hold)await new Promise(resolve=>{releaseSave=()=>{hold=false;resolve();};resolveStarted();});
        else resolveStarted();
        if(failures-->0)return route.fulfill({status:503,body:'Fast setup could not be saved. Try again.'});
        if(body.raw!=null)document=JSON.parse(body.raw);
        else for(const [key,value] of Object.entries(body.changes)){
          const parts=key.split('.');let target=document;
          for(const part of parts.slice(0,-1))target=target[part]??={};
          target[parts.at(-1)]=value;
        }
        revision++;return route.fulfill({json:snapshot()});
      }
      return route.fulfill({status:404});
    });
    await page.goto('http://127.0.0.1:43274/#test-token');
    await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
    const dialog=page.getByRole('dialog',{name:'Codexify settings'}),button=dialog.locator('#settings-fast-setup-apply');
    await button.waitFor();
    return {browser,page,dialog,button,posts,writes,errors,started,document:()=>document,update:value=>{update=value;},release:()=>releaseSave?.()};
  }catch(error){await browser.close();throw error;}
}

test('fast setup is one metadata-driven atomic save and does not restart or change protected choices',{timeout:30000},async()=>{
  const fixture=await fastSetupFixture({hold:true});
  const {browser,page,dialog,button,writes,posts,errors}=fixture;
  try {
    for(const name of ['Browser','General','Browser']){
      await dialog.getByRole('button',{name,exact:true}).click();
      assert.equal(await dialog.locator('#settings-fast-setup').count(),1);
    }
    await dialog.getByRole('searchbox',{name:'Search settings'}).fill('Steer delivery');
    assert.equal(await dialog.locator('#settings-fast-setup').count(),1);
    await dialog.getByRole('searchbox',{name:'Search settings'}).fill('');
    assert.match(await dialog.locator('#settings-fast-setup-description').textContent(),/Steer delivery/);
    if(process.env.CODEXIFY_FAST_SETUP_SCREENSHOT)await page.screenshot({path:process.env.CODEXIFY_FAST_SETUP_SCREENSHOT,fullPage:true});
    await button.click();
    await fixture.started;
    await page.waitForFunction(()=>document.querySelector('#settings-fast-setup-apply').getAttribute('aria-busy')==='true');
    assert.equal(await button.isDisabled(),true);
    assert.deepEqual(writes,[{revision:'1',changes:fastSetupChanges}]);
    fixture.release();
    await dialog.getByRole('button',{name:'Fast setup saved',exact:true}).waitFor();
    assert.equal(await button.isDisabled(),true);
    await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
    assert.deepEqual(posts,['/api/settings']);
    for(const [key,value] of Object.entries(fastSetupChanges))assert.equal(at(fixture.document(),key),value,key);
    assert.equal(fixture.document().instanceName,'Keep this name');
    assert.equal(fixture.document().agentChat.automation.autoApproveToolCalls,false);
    assert.equal(fixture.document().agentChat.automation.requestBlockingEnabled,false);
    assert.equal(fixture.document().agentChat.features.externalAgentControl,false);
    assert.equal(fixture.document().agentChat.features.agentFolders,false);
    assert.deepEqual(fixture.document().agentChat.turnRenewal.limits,example.agentChat.turnRenewal.limits);
    assert.equal(Object.keys(writes[0].changes).some(key=>/model|effort/i.test(key)),false);
    assert.deepEqual(errors,[]);
  }finally{fixture.release();await browser.close();}
});

test('fast setup reports save failure and retries the retained batch with the same revision',{timeout:30000},async()=>{
  const {browser,dialog,button,writes,posts,errors}=await fastSetupFixture({failures:1});
  try {
    await button.click();
    await dialog.getByText('Fast setup could not be saved. Try again.',{exact:true}).waitFor();
    const retry=dialog.getByRole('button',{name:'Retry fast setup',exact:true});await retry.waitFor();
    assert.equal(await retry.isEnabled(),true);
    assert.equal(await dialog.locator('#settings-save-state').getAttribute('data-error'),'true');
    assert.equal(await dialog.getByRole('button',{name:'Fast setup saved',exact:true}).count(),0);
    await retry.click();
    await dialog.getByRole('button',{name:'Fast setup saved',exact:true}).waitFor();
    assert.deepEqual(writes,[{revision:'1',changes:fastSetupChanges},{revision:'1',changes:fastSetupChanges}]);
    assert.deepEqual(posts,['/api/settings','/api/settings']);
    assert.equal(await dialog.locator('#settings-save-state').getAttribute('data-error'),'false');
    assert.deepEqual(errors,[]);
  }finally{await browser.close();}
});

test('fast setup waits for invalid fields, unsaved raw input, and active maintenance',{timeout:30000},async()=>{
  const fixture=await fastSetupFixture();
  const {browser,page,dialog,button,writes}=fixture;
  try {
    await dialog.getByRole('searchbox',{name:'Search settings'}).fill('agentChat.automation.retryTimings.jitterPercent');
    const jitter=dialog.getByLabel('agentChat.automation.retryTimings.jitterPercent',{exact:true});
    await jitter.fill('101');await dialog.getByRole('button',{name:'General',exact:true}).click();
    assert.equal(await button.isDisabled(),true);
    await dialog.getByRole('searchbox',{name:'Search settings'}).fill('agentChat.automation.retryTimings.jitterPercent');
    await jitter.fill('');await page.clock.fastForward(700);
    await dialog.getByRole('button',{name:'General',exact:true}).click();
    assert.equal(await button.isEnabled(),true);
    await dialog.getByRole('button',{name:'View raw config',exact:true}).click();
    const rawDraft=JSON.stringify({...fixture.document(),instanceName:'My raw draft'});
    await dialog.getByRole('textbox',{name:'Raw configuration JSON'}).fill(rawDraft);
    await dialog.getByRole('button',{name:'Back to settings',exact:true}).click();
    assert.equal(await button.isDisabled(),true);
    assert.deepEqual(writes,[]);
    await dialog.getByRole('button',{name:'View raw config',exact:true}).click();
    assert.equal(await dialog.getByRole('textbox',{name:'Raw configuration JSON'}).inputValue(),rawDraft);
    await dialog.getByRole('button',{name:'Save raw config',exact:true}).click();
    await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
    await dialog.getByRole('button',{name:'Back to settings',exact:true}).click();
    assert.equal(await button.isEnabled(),true);
    assert.equal(fixture.document().instanceName,'My raw draft');
    fixture.update({state:'installing'});await page.clock.fastForward(16000);
    await page.getByText('Installing update…',{exact:true}).waitFor();
    assert.equal(await button.isDisabled(),true);
    assert.equal(writes.length,1,'Maintenance must not trigger a preset save');
  }finally{await browser.close();}
});

test('browser simplicity offers Off, Simplified, and Bones while retaining the legacy saved choice',{timeout:30000},async()=>{
  const browser=await launch();
  try {
    const page=await browser.newPage();page.setDefaultTimeout(5000);await page.clock.install();
    let document={agentChat:{automation:{minimalUi:true}}},revision=1;
    const defaults=structuredClone(example);defaults.agentChat.automation.browserSimplicity='simplified';
    const snapshot=()=>({document,defaults,fieldSchema:example,raw:JSON.stringify(document),revision:String(revision),restartRequired:revision>1});
    await routeOwner(page,'http://127.0.0.1:43273/**',route=>{
      const request=route.request(),path=new URL(request.url()).pathname;
      if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
      if(path==='/api/global')return route.fulfill({json:{version:'2.3.15',settingsAvailable:true}});
      if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
      if(path==='/api/settings'&&request.method()==='GET')return route.fulfill({json:snapshot()});
      if(path==='/api/settings'){
        const body=request.postDataJSON();assert.equal(body.revision,String(revision));
        if(body.raw!=null)document=JSON.parse(body.raw);
        else for(const [key,value] of Object.entries(body.changes)){
          assert.equal(key,'agentChat.automation.browserSimplicity');
          if(value===null)delete document.agentChat.automation.browserSimplicity;
          else document.agentChat.automation.browserSimplicity=value;
        }
        revision++;return route.fulfill({json:snapshot()});
      }
      return route.fulfill({status:404});
    });
    await page.goto('http://127.0.0.1:43273/#test-token');
    await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
    const dialog=page.getByRole('dialog',{name:'Codexify settings'});
    await dialog.getByRole('searchbox',{name:'Search settings'}).fill('Browser simplicity');
    const level=dialog.getByLabel('agentChat.automation.browserSimplicity',{exact:true});
    assert.deepEqual(await level.locator('option').allTextContents(),['Simplified (default)','Off','Simplified','Bones — maximum simplicity']);
    assert.equal(await level.inputValue(),'');
    if(process.env.CODEXIFY_UI_SCREENSHOT)await page.screenshot({path:process.env.CODEXIFY_UI_SCREENSHOT,fullPage:true});
    for(const choice of ['bones','simplified','off','']){
      await level.selectOption(choice);await page.clock.fastForward(700);
      await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
      assert.equal(document.agentChat.automation.browserSimplicity,choice||undefined);
      assert.equal(document.agentChat.automation.minimalUi,true,'Legacy value survives edits and default reset');
    }
    assert.equal(await dialog.locator('[data-setting-row="agentChat.automation.minimalUi"]').count(),0);
  }finally{await browser.close();}
});

test('all added settings start off, persist explicit choices, and reset to off',{timeout:60000},async()=>{
  const browser=await launch();
  try {
    const page=await browser.newPage();page.setDefaultTimeout(5000);await page.clock.install();
    const errors=[];page.on('pageerror',error=>errors.push(error.message));
    let document={},revision=1;const writes=[];
    const snapshot=()=>({document,defaults:example,fieldSchema:example,raw:JSON.stringify(document),revision:String(revision),restartRequired:revision>1});
    await routeOwner(page,'http://127.0.0.1:43271/**',route=>{
      const request=route.request(),path=new URL(request.url()).pathname;
      if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
      if(path==='/api/global')return route.fulfill({json:{version:'2.3.15',settingsAvailable:true}});
      if(path==='/api/chats')return route.fulfill({json:{chats:[],serverTimeMs:Date.now()}});
      if(path==='/api/settings'&&request.method()==='GET')return route.fulfill({json:snapshot()});
      if(path==='/api/settings'){
        const body=request.postDataJSON();assert.equal(body.revision,String(revision));writes.push(body);
        if(body.raw!=null)document=JSON.parse(body.raw);
        else for(const [key,value] of Object.entries(body.changes)){
          const parts=key.split('.');let target=document;
          for(const part of parts.slice(0,-1))target=target[part]??={};
          target[parts.at(-1)]=value;
        }
        revision++;return route.fulfill({json:snapshot()});
      }
      return route.fulfill({status:404});
    });
    await page.goto('http://127.0.0.1:43271/#test-token');
    await page.getByRole('button',{name:'Codexify settings',exact:true}).click();
    const dialog=page.getByRole('dialog',{name:'Codexify settings'});
    for(const path of switches){
      assert.equal(at(example,path),false,path);
      await dialog.getByRole('searchbox',{name:'Search settings'}).fill(path);
      const row=dialog.locator(`[data-setting-row="${path}"]`),group=row.getByRole('group');
      await row.waitFor();assert.equal(await group.getByRole('button',{name:'Off (default)',exact:true}).getAttribute('aria-pressed'),'true',path);
      for(const [label,value] of [['On',true],['Off',false],['Off (default)',undefined]]){
        await group.getByRole('button',{name:label,exact:true}).click();await page.clock.fastForward(700);
        await dialog.getByText('Saved. Restart Codexify to apply changes.',{exact:true}).waitFor();
        assert.equal(at(document,path),value,`${path} ${label}`);
        assert.equal(await group.getByRole('button',{name:label,exact:true}).getAttribute('aria-pressed'),'true');
      }
    }
    assert.equal(writes.length,switches.length*3);assert.deepEqual(errors,[]);
    assert.doesNotMatch(ownerHtml,/agents-board|agents\.txt/);
  }finally{await browser.close();}
});

test('agent and Auto mode controls require feature settings and no board is exposed',{timeout:30000},async()=>{
  const browser=await launch();
  try{
    const page=await browser.newPage();page.setDefaultTimeout(5000);await page.clock.install();
    const id='e'.repeat(64);let features={};
    await routeOwner(page,'http://127.0.0.1:43272/**',route=>{
      const path=new URL(route.request().url()).pathname;
      if(path==='/')return route.fulfill({contentType:'text/html',body:ownerHtml});
      if(path==='/api/global')return route.fulfill({json:{version:'2.3.15',settingsAvailable:true}});
      if(path==='/api/chats')return route.fulfill({json:{chats:[{id,title:'Feature fixture',workspace:'fixture',lastEntryEnd:0,lastEntryAtMs:Date.now(),totalToolCalls:0}],serverTimeMs:Date.now()}});
      if(path.endsWith('/automation'))return route.fulfill({json:{enabled:true,features,pairing:{status:'verified'},agents:{allowDelegation:false,children:[]},goal:null,action:{status:'idle'},selection:{model:'test',effort:'high'},catalog:{models:[]},companion:{authenticated:true,status:'idle'},presence:{},serverTimeMs:Date.now()}});
      if(path.endsWith('/agents'))return route.fulfill({json:{allowDelegation:false,workers:[]}});
      return route.fulfill({json:{_meta:{'io.github.devnoname120/codexify/markdown-chat':{chat_file:'/fixture/CHAT.md',revision:'0',messages:[],delivered_through:0,read_through:0,total_tool_calls:0,server_time_ms:Date.now(),has_more:false}}}});
    });
    await page.goto('http://127.0.0.1:43272/#test-token');
    await page.getByText('Feature fixture',{exact:true}).click();
    await page.locator('#automation-agents').waitFor({state:'attached'});
    assert.equal(await page.locator('#automation-agents').isVisible(),false);
    assert.equal(await page.locator('#automation-auto-mode').isVisible(),false);
    features={agentTeams:true,autoMode:true};await page.clock.fastForward(2000);
    await page.locator('#automation-agents').waitFor({state:'visible'});
    await page.locator('#automation-auto-mode').waitFor({state:'visible'});
    await page.locator('#automation-agents').click();
    assert.equal(await page.locator('#agents-editor').isVisible(),true);
    assert.equal(await page.locator('[data-worker="readOnly"]').isChecked(),false);
    assert.equal(await page.getByRole('button',{name:'Shared board',exact:true}).count(),0);
    features={};await page.clock.fastForward(2000);
    await page.locator('#automation-agents').waitFor({state:'hidden'});
    assert.equal(await page.locator('#agents-editor').isVisible(),false);
  }finally{await browser.close();}
});
