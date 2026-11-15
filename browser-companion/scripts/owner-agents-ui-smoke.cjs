// Read-only live metadata; every mutation is intercepted by this fixture.

const fs=require('node:fs'),path=require('node:path'),os=require('node:os'),assert=require('node:assert/strict');

const {chromium}=require('patchright');

const repo=path.resolve(__dirname,'../..'),read=p=>fs.readFileSync(path.join(repo,'src',p),'utf8');

const section=(s,a,b)=>s.split(a)[1].split(b)[0];

(async()=>{

 const root=path.join(os.homedir(),'.codexify'),auth=JSON.parse(fs.readFileSync(path.join(root,'owner-chat.json'),'utf8'));

 const base='http://127.0.0.1:'+auth.port,id='11b3e5771421419496ea3a3c31231336c042d37d151dbf64330f1b099152faa6';

 let chat=read('markdown_chat_ui.html').replace('/* CODEXIFY_MARKDOWN_LIBRARY */',read('vendor/markdown-it.min.js')).replace('/* CODEXIFY_MARKDOWN_RENDERER */',read('markdown_chat_render.js')).replace('/* CODEXIFY_ATTACHMENT_COMPOSER */',read('chat_attachments_ui.js'));

 let style=section(chat,'<style>','</style>').replaceAll(':root:not([data-theme="light"])',':host(:not([data-theme="light"]))').replaceAll(':root[data-theme="dark"]',':host([data-theme="dark"])').replaceAll(':root',':host').replace('body {',':host { display:block;');

 style+='\n#chat {max-width:none;height:100%;border:0;border-radius:0;display:flex;flex-direction:column} #panel {display:flex;flex:1;min-height:0;flex-direction:column} #panel[hidden]{display:none} #messages{flex:1;max-height:none;min-height:0}';

 let html=read('owner_chat.html').replace('<!-- CHAT_TEMPLATE -->','<template id="chat-template"><style>'+style+'</style>'+section(chat,'<body>','<script>')+'</template>').replace('/* CHAT_SCRIPT */',section(chat,'<script>','</script>'));

 for(const [marker,file] of [['GLOBAL_SETTINGS_SCRIPT','global_settings_ui.js'],['OWNER_PWA_SCRIPT','owner_pwa_ui.js'],['USAGE_ANALYTICS_SCRIPT','analytics_ui.js'],['STATISTICS_EXTRAS_SCRIPT','statistics_extras_ui.js']])html=html.replace('/* '+marker+' */',read(file));

 const state=JSON.parse(fs.readFileSync(path.join(root,'projects/newvegas-vita-89dfda6fee31/chats/4dfc6036f11899fdb9ede0f4c8db09ba83108297153661f9548eaf9629af2947/automation.json'),'utf8'));

 const fixture={enabled:true,features:{agentTeams:true,autoMode:true,agentFolders:true,conversationRecovery:true},ownerGeneration:state.pairing.verifiedOwnerGeneration,pairing:{...state.pairing,status:'verified'},goal:state.goal,selection:state.selection,action:{...state.action,status:'idle',lastError:null,automaticRetriesInfinite:true},presence:{lastAgentCallAtMs:Date.now()},companion:{authenticated:true,status:'idle'},conversationRecovery:{enabled:true,failedChatCount:2},agents:{allowDelegation:false},serverTimeMs:Date.now()};

 const browserRoot=path.resolve(__dirname,'../.browsers',process.platform+'-'+process.arch),metadata=JSON.parse(fs.readFileSync(path.join(browserRoot,'chrome-executable.json'),'utf8'));

 const profile=fs.mkdtempSync(path.join(os.tmpdir(),'codexify-owner-ui-'));

 const context=await chromium.launchPersistentContext(profile,{executablePath:path.resolve(browserRoot,metadata.executable),headless:true,viewport:{width:1280,height:900}});

 let submitted=null,enabled=false,failCreation=true,failControl=true,team=[];const errors=[],attempts=[],controls=[];

 try{

  const page=context.pages()[0];page.on('pageerror',e=>errors.push(e.message));

  await context.route(base+'/**',async route=>{

   const request=route.request(),url=new URL(request.url()),p=url.pathname;

   if(p==='/preview')return route.fulfill({contentType:'text/html',body:html});

   if(p.endsWith('/automation/agents/control')){

    const body=JSON.parse(request.postData());controls.push(body);

    if(failControl){failControl=false;return route.fulfill({status:503,body:'Worker control temporarily unavailable'});}

    const worker=team.find(w=>w.taskKey===body.workerTaskKey);assert(worker);

    if(body.action==='pause'||body.action==='stop'){worker.goal.status='configured';worker.stopped=true;}

    if(body.action==='resume'){worker.goal.status='active';worker.stopped=false;}

    if(body.action==='set_goal'){assert.equal(body.expectedRevision,worker.goal.revision);worker.goalText=body.content;worker.goal.revision++;}

    return route.fulfill({json:{applied:true}});

   }

   if(p.endsWith('/automation/agents/settings')){enabled=JSON.parse(request.postData()).enabled;return route.fulfill({json:{enabled}});}

   if(p.endsWith('/automation/agents')){if(request.method()==='POST'){submitted=JSON.parse(request.postData());attempts.push(submitted.requestId);if(failCreation){failCreation=false;return route.fulfill({status:503,body:'Browser is busy. Retry when it is ready.'});}team=submitted.workers.map((w,i)=>({...w,taskKey:String(i),goal:{status:'active',revision:1},goalText:w.goal,canControl:true,toolCallCount:12,action:'queued'}));}return route.fulfill({json:{allowDelegation:enabled,workers:team}});}

   if(p.endsWith('/automation'))return route.fulfill({json:{...fixture,agents:{allowDelegation:enabled,children:team}}});

   if(request.method()!=='GET')return route.fulfill({json:{through:0,enabled:true}});

   let r;try { r=await fetch(base+p+url.search,{headers:{Authorization:'Bearer '+auth.token},signal:AbortSignal.timeout(60000)}); } catch(error) { console.log('Fixture metadata unavailable: '+p);return route.fulfill({status:503,body:'Fixture metadata unavailable'}); }

   let body=await r.text();if(p==='/api/chats'){const data=JSON.parse(body);data.chats=data.chats.filter(c=>c.id===id);body=JSON.stringify(data);}

   return route.fulfill({status:r.status,contentType:r.headers.get('content-type')||'application/json',body});

  });

  await page.goto(base+'/preview?chat='+id+'#fixture-token',{waitUntil:'domcontentloaded'});

  await page.getByRole('button',{name:'Agents',exact:true}).click();

  await page.getByText('No workers yet.',{exact:true}).waitFor();

  await page.getByLabel('Allow requested delegation').check();

  await page.getByText('Requested delegation enabled for this chat.',{exact:true}).waitFor();

  await page.getByText('Delegation on',{exact:true}).waitFor();

  await page.getByRole('button',{name:'Close agents',exact:true}).click();

  await page.getByRole('button',{name:'Agents',exact:true}).click();

  assert(await page.getByLabel('Allow requested delegation').isChecked());

  for(let i=1;i<5;i++)await page.getByRole('button',{name:'Add worker',exact:true}).click();

  assert.equal(await page.locator('.agents-worker').count(),5);

  for(let i=0;i<5;i++){const row=page.locator('.agents-worker').nth(i);await row.locator('[data-worker="role"]').fill('Audit '+i);await row.locator('[data-worker="goal"]').fill('Verify subsystem '+i);await row.locator('[data-worker="scope"]').fill('subsystem-'+i);}

  const inputStyle=await page.locator('[data-worker="role"]').first().evaluate(e=>({radius:getComputedStyle(e).borderRadius,font:getComputedStyle(e).fontSize}));assert.equal(inputStyle.radius,'8px');assert.equal(inputStyle.font,'13px');

  await page.locator('#agents-body').evaluate(e=>e.scrollTop=0);

  const output=process.env.CODEXIFY_AGENTS_SCREENSHOTS||'D:/CodexifyBuild/upstream-2.3.15-20261025';await page.emulateMedia({colorScheme:'light'});await page.screenshot({path:output+'/agents-ui-desktop.png'});

  await page.emulateMedia({colorScheme:'dark'});await page.screenshot({path:output+'/agents-ui-dark.png'});

  await page.setViewportSize({width:390,height:850});await page.locator('#agents-body').evaluate(e=>e.scrollTop=0);await page.screenshot({path:output+'/agents-ui-mobile.png'});

  const panel=await page.locator('#agents-editor').boundingBox();assert(panel&&panel.y>=0,'Agents heading must remain inside the viewport');

  const fits=await page.locator('#agents-editor').evaluate(e=>e.scrollWidth<=e.clientWidth+2);assert(fits,'Agents panel must fit a narrow viewport');

  const footer=await page.locator('#agents-footer').boundingBox();assert(footer&&footer.y>=0&&footer.y+footer.height<=850,'Actions must stay visible while worker fields scroll');

  await page.getByRole('button',{name:'Start workers',exact:true}).click();await page.locator('#agents-status').getByText('Browser is busy. Retry when it is ready.',{exact:true}).waitFor();

  assert.equal(await page.locator('.agents-worker').count(),5,'Failed creation must preserve the draft workers');

  await page.getByRole('button',{name:'Start workers',exact:true}).click();await page.getByText('5 worker chats queued in this project.',{exact:true}).waitFor();

  assert.equal(attempts.length,2);assert.equal(attempts[0],attempts[1],'Retry must reuse the creation idempotency key');

  assert.equal(submitted.workers.length,5);assert(submitted.workers.every(w=>w.readOnly));

  const card=page.locator('.agent-card').first();await card.locator('summary').click();
  await page.getByRole('textbox',{name:'Instruction · Audit 0',exact:true}).fill('Check save path before the next build');
  await page.waitForTimeout(5500);assert.equal(await page.getByRole('textbox',{name:'Instruction · Audit 0',exact:true}).inputValue(),'Check save path before the next build','Polling preserves the focused instruction');
  await page.getByRole('button',{name:'Send instruction · Audit 0',exact:true}).click();await card.getByText('Worker control temporarily unavailable',{exact:true}).waitFor();
  assert.equal(await page.getByRole('textbox',{name:'Instruction · Audit 0',exact:true}).inputValue(),'Check save path before the next build');
  await page.getByRole('button',{name:'Send instruction · Audit 0',exact:true}).click();await card.getByText('Instruction saved for the worker.',{exact:true}).waitFor();
  assert.equal(controls[0].requestId,controls[1].requestId,'Retry must preserve the control request ID');
  await page.getByRole('textbox',{name:'Worker goal · Audit 0',exact:true}).fill('Verify save-path restoration');
  await page.getByRole('button',{name:'Update goal · Audit 0',exact:true}).click();await card.getByText('Worker goal updated.',{exact:true}).waitFor();assert.equal(team[0].goal.revision,2);
  await page.getByRole('button',{name:'Pause · Audit 0',exact:true}).click();await card.getByText('Worker paused.',{exact:true}).waitFor();
  assert(await page.getByRole('button',{name:'Send instruction · Audit 0',exact:true}).isDisabled());
  await page.getByRole('button',{name:'Resume · Audit 0',exact:true}).click();await card.getByText('Worker resumed.',{exact:true}).waitFor();
  await page.getByRole('button',{name:'Stop · Audit 0',exact:true}).click();await card.getByText('Worker stopped.',{exact:true}).waitFor();
  await page.getByRole('button',{name:'Resume · Audit 0',exact:true}).click();await card.getByText('Worker resumed.',{exact:true}).waitFor();
  assert(team.slice(1).every(w=>w.goal.revision===1&&!w.stopped),'Other workers must remain unchanged');
  await page.locator('#agents-body').evaluate(e=>e.scrollTop=0);await page.screenshot({path:output+'/agents-controls-mobile.png'});
  await page.setViewportSize({width:1280,height:900});await page.screenshot({path:output+'/agents-controls-desktop.png'});
  team[0].action='failed';await page.locator('#agents-editor-close').focus();await card.locator('.agent-phase').getByText('failed',{exact:true}).waitFor({timeout:10000});

  assert(await page.getByRole('button',{name:'Start workers',exact:true}).isDisabled());assert.deepEqual(errors,[]);

  console.log(JSON.stringify({ok:true,workers:5,mobileFits:true,modePersists:true,inlineErrors:true,retryIdempotent:true,liveStatus:true,parentControls:true,controlRetryIdempotent:true,draftsPreserved:true,mutationsIntercepted:true}));

 }finally{await context.close();fs.rmSync(profile,{recursive:true,force:true,maxRetries:12,retryDelay:250});}

})().catch(e=>{console.error(e.name+': '+e.message.replace(/#[^\s]+/g,'#redacted'));process.exitCode=1;});

