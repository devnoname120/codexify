// Isolated owner-UI fixture. Native access is GET-only; mutations fail closed.
const fs=require('node:fs'),path=require('node:path'),os=require('node:os'),assert=require('node:assert/strict');
const {chromium}=require('patchright');
const repo=path.resolve(__dirname,'../..'),read=file=>fs.readFileSync(path.join(repo,'src',file),'utf8');
const section=(source,start,end)=>source.split(start)[1].split(end)[0];
(async()=>{
 const root=path.join(os.homedir(),'.codexify'),auth=JSON.parse(fs.readFileSync(path.join(root,'owner-chat.json'),'utf8'));
 const base='http://127.0.0.1:'+auth.port,real='901363c803db5b95fc3d60af3327e560de6ebc3e284c1997daacf8e124742136';
 const get=async suffix=>{const response=await fetch(base+suffix,{headers:{Authorization:'Bearer '+auth.token}});assert(response.ok);return response.json();};
 const widget=await get('/api/chats/'+real),automation=await get('/api/chats/'+real+'/automation');
 automation.features={agentTeams:true,autoMode:true,agentFolders:true,conversationRecovery:true};
 let component=read('markdown_chat_ui.html').replace('/* CODEXIFY_MARKDOWN_LIBRARY */',read('vendor/markdown-it.min.js')).replace('/* CODEXIFY_MARKDOWN_RENDERER */',read('markdown_chat_render.js')).replace('/* CODEXIFY_ATTACHMENT_COMPOSER */',read('chat_attachments_ui.js'));
 let style=section(component,'<style>','</style>').replaceAll(':root:not([data-theme="light"])',':host(:not([data-theme="light"]))').replaceAll(':root[data-theme="dark"]',':host([data-theme="dark"])').replaceAll(':root',':host').replace('body {',':host { display:block;');
 style+='\n#chat{max-width:none;height:100%;border:0;border-radius:0;display:flex;flex-direction:column}#panel{display:flex;flex:1;min-height:0;flex-direction:column}#panel[hidden]{display:none}#messages{flex:1;max-height:none;min-height:0}';
 let html=read('owner_chat.html').replace('<!-- CHAT_TEMPLATE -->','<template id="chat-template"><style>'+style+'</style>'+section(component,'<body>','<script>')+'</template>').replace('/* CHAT_SCRIPT */',section(component,'<script>','</script>'));
 for(const [marker,file] of [['GLOBAL_SETTINGS_SCRIPT','global_settings_ui.js'],['OWNER_PWA_SCRIPT','owner_pwa_ui.js'],['USAGE_ANALYTICS_SCRIPT','analytics_ui.js'],['STATISTICS_EXTRAS_SCRIPT','statistics_extras_ui.js']])html=html.replace('/* '+marker+' */',read(file));
 const id=letter=>letter.repeat(64),now=Date.now();
 const common={customTitle:null,pinnedAtMs:null,archivedAtMs:null,ownerReadThrough:0,workspace:'New Vegas Vita',projectId:'project-vita',projectPath:'C:/projects/newvegas-vita',projectGroup:{name:'New Vegas Vita',path:'C:/projects/newvegas-vita'},isScratch:false,lastEntryEnd:100,lastIncomingEntryEnd:100,lastEntryAtMs:now,lastAgentCallAtMs:now,goalStatus:'active',goal:null,automationError:null,automationRetrying:false,totalToolCalls:10};
 const members=[{taskKey:id('b'),role:'Material parity',scope:'Renderer materials'},{taskKey:id('c'),role:'World gameplay',scope:'Player and collision'},{taskKey:id('d'),role:'Build verification',scope:'Package evidence'},{taskKey:id('9'),role:'Reserved auditor',scope:'Pending setup'}];
 const parent={...common,id:id('a'),title:'New Vegas coordinator',agentTeam:{taskKey:id('a'),parentTaskKey:null,children:members}};
 const workers=members.slice(0,3).map((member,index)=>({...common,id:member.taskKey,title:'Worker '+(index+1),agentTeam:{...member,parentTaskKey:id('a'),children:[],actionStatus:index===1?'queued':'idle',creating:index===1}}));
 workers[2].automationError='Temporary setup observation failure';workers[2].automationRetrying=true;
 const foreign={...common,id:id('f'),title:'Another project',projectId:'other',projectPath:'C:/projects/other',workspace:'Other project',agentTeam:{taskKey:id('f'),parentTaskKey:id('a'),role:'Unrelated worker',children:[]}};
 let chats=[parent,...workers,foreign],mutations=0;const errors=[],requests=[]; automation.agents.parentTaskKey=null;
 const browserRoot=path.resolve(__dirname,'../.browsers',process.platform+'-'+process.arch),metadata=JSON.parse(fs.readFileSync(path.join(browserRoot,'chrome-executable.json'),'utf8'));
 const browser=await chromium.launch({executablePath:path.resolve(browserRoot,metadata.executable),headless:true});
 try {
  const context=await browser.newContext({viewport:{width:1280,height:900}}),page=await context.newPage();page.on('pageerror',error=>errors.push(error.message));
  await context.route(base+'/**',async route=>{
   const request=route.request(),url=new URL(request.url()),p=url.pathname;
   if(request.method()==='POST'&&p.endsWith('/read'))return route.fulfill({json:{}});
   if(request.method()==='POST'&&p.endsWith('/automation/auto-mode')){
    const body=request.postDataJSON();requests.push(body);
    if(requests.length===1){automation.goal={...automation.goal,revision:body.expectedRevision+1,status:'active',markdown:'Auto mode\n\nObjective:\n'+body.prompt+'\n\nWorking agreement:\nFixture instructions'};return route.fulfill({status:503,body:'Response interrupted after saving. Retry safely.'});}
    return route.fulfill({json:{goal:automation.goal,replayed:true,autoMode:true}});
   }

   if(request.method()!=='GET'){mutations++;return route.fulfill({status:403,body:'Fixture mutation rejected'});}
   if(p==='/preview')return route.fulfill({contentType:'text/html',body:html});
   if(p==='/api/chats')return route.fulfill({json:{chats,matchedChatIds:url.searchParams.has('q')?[]:null,automationEnabled:true,serverTimeMs:Date.now()}});
   if(/^\/api\/chats\/[a-f0-9]{64}\/automation$/.test(p))return route.fulfill({json:automation});
   if(/^\/api\/chats\/[a-f0-9]{64}$/.test(p))return route.fulfill({json:widget});
   if(p.endsWith('/automation/agents'))return route.fulfill({json:{allowDelegation:true,workers:[]}});
   const response=await fetch(base+p+url.search,{headers:{Authorization:'Bearer '+auth.token}});
   return route.fulfill({status:response.status,contentType:response.headers.get('content-type')||'application/json',body:await response.text()});
  });
  await page.goto(base+'/preview?chat='+id('a')+'#fixture-token');

  const button=page.getByRole('button',{name:'Auto mode',exact:true});await button.waitFor();await button.click();
  const prompt=page.getByRole('textbox',{name:'What should be done?'}),submit=page.getByRole('button',{name:'Start Auto mode',exact:true});
  await prompt.fill('Implement a tested save-path fix and verify the result.');
  await submit.click();await page.getByText('Response interrupted after saving. Retry safely.').waitFor();
  assert.equal(await prompt.inputValue(),'Implement a tested save-path fix and verify the result.');
  assert.equal(await prompt.isEnabled(),true);await submit.click();
  assert.equal(requests.length,2);assert.deepEqual(requests[0],requests[1]);
  await page.getByRole('button',{name:'Auto mode',exact:true}).click();
  await prompt.waitFor();assert.equal(await prompt.inputValue(),requests[0].prompt);
  const out='D:/CodexifyBuild/smartness-setup-20261025';await page.screenshot({path:out+'/auto-mode-desktop.png'});
  await page.emulateMedia({colorScheme:'dark'});await page.screenshot({path:out+'/auto-mode-dark.png'});
  await page.setViewportSize({width:390,height:850});await page.screenshot({path:out+'/auto-mode-mobile.png'});
  assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
  assert.equal(mutations,0);assert.deepEqual(errors,[]);
  console.log('AUTO_MODE_UI_SMOKE_OK: one prompt, themed desktop/dark/mobile, unchanged idempotent retry after lost response, retained objective, no live mutations');
 } finally {await browser.close();}
})().catch(error=>{console.error(error);process.exitCode=1;});
