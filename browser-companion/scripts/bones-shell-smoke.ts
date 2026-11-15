import assert from "node:assert/strict";
import {resolve} from "node:path";
import {chromium} from "patchright";
import {privateChromeExecutable} from "../src/chrome-executable";
import {BONES_SHELL_CSS,installMinimalChatGptPage,styleRestoredChatGptPage,type BrowserSimplicity} from "../src/minimal-page";
import {activeComposer,composerText,insertExactPrompt} from "../src/composer";
import {readChatGptAccount} from "../src/account";
import {inspectConnectorSettings,beginConnectorToolRefresh} from "../src/account-settings";

// A fresh, entirely routed fixture: never uses the live managed profile/account.
const executablePath=process.env.CODEXIFY_TEST_CHROME_PATH??await privateChromeExecutable(resolve(".browsers",`${process.platform}-${process.arch}`));
const browser=await chromium.launch({executablePath,headless:true,timeout:20_000});
const fixture=`<!doctype html><html><head><style>
body{margin:0;font:16px sans-serif}aside.app-shell-left-panel{position:fixed;inset:0 auto 0 0;width:260px;background:#ddd;overflow:hidden}
#app-shell-sidebar{width:260px;height:100%;overflow:hidden}#sidebar-inner{overflow:hidden}button{min-height:28px}
main{margin-left:270px;padding:16px}.rounded{border-radius:24px;box-shadow:0 2px 8px #777}
#portal-composer{position:fixed;bottom:24px;left:80px;right:24px;background:white;padding:8px}
#prompt-textarea{min-height:30px;white-space:pre-wrap}[role=menu]{position:fixed;left:60px;top:120px;background:white;border:1px solid}
</style></head><body>
<aside class="app-shell-left-panel" id="rail"><div id="sidebar-inner"><div id="app-shell-sidebar"><nav aria-label="Main navigation" id="modern-nav">History and projects</nav><button aria-label="Open profile menu" id="profile">Profile</button><button data-testid="create-new-chat-button" id="new-chat">New</button></div></div></aside>
<main><header id="shell-header"><div role="tablist"><button>Chat</button><button>Work</button></div></header><header id="legacy-header"><button data-testid="model-switcher-dropdown-button" id="legacy-model">Model</button></header><h1 id="welcome">How can I help?</h1><div id="turns"></div></main>
<div id="portal-composer" class="rounded"><form data-chatgpt-composer class="rounded"><div id="prompt-textarea" contenteditable="plaintext-only" role="textbox" class="rounded"></div><button type="button" id="apps" aria-haspopup="menu">Apps</button><button type="button" id="effort" data-codexify-effort-control="true" aria-haspopup="menu">Effort</button><button id="send" data-testid="send-button" type="submit">Send</button><button id="stop" data-testid="stop-button" type="button">Stop</button><button id="voice" data-testid="voice-mode-button" type="button">Voice</button></form></div>
<div id="profile-menu" role="menu" hidden><button id="profile-action">Account settings</button></div><div id="apps-menu" role="menu" hidden><button id="app-choice">Codexify</button></div><div id="effort-menu" role="menu" hidden><button id="effort-choice">High</button></div>
<aside id="approval" role="dialog" style="position:fixed;right:12px;top:90px">Tool permission <button id="approve">Approve</button></aside><div role="alert" id="error" style="position:fixed;right:12px;top:150px">A recoverable tool error</div>
<script>window.clicks={};window.sent=[];document.addEventListener('click',event=>{const id=event.target.id;if(id)window.clicks[id]=(window.clicks[id]||0)+1;document.body.dataset.clicks=JSON.stringify(window.clicks);});
for(const [trigger,menu] of [['profile','profile-menu'],['apps','apps-menu'],['effort','effort-menu']])document.getElementById(trigger).onclick=()=>{document.getElementById(menu).hidden=false;};
document.querySelector('form').onsubmit=event=>{event.preventDefault();window.sent.push(document.getElementById('prompt-textarea').innerText);document.body.dataset.sent=JSON.stringify(window.sent);document.getElementById('welcome').remove();document.getElementById('turns').innerHTML='<article data-turn-key="answer"><div data-conversation-role="assistant"><header id="answer-header"><h2 id="answer-heading">Verified answer heading</h2></header><p>Fresh answer</p><button id="copy" data-testid="copy-turn-action-button">Copy</button></div></article>';document.getElementById('prompt-textarea').textContent='';};</script></body></html>`;
const settingsFixture=`<!doctype html><style>body{font:16px sans-serif}button{min-height:28px}</style><main>
<header id="settings-header"><h1 id="settings-title">Settings</h1><button id="refresh" data-action="refresh-tools">Refresh tools</button></header>
<div role="tablist" id="settings-tabs"><button>Details</button><button>Permissions</button></div>
<h2 id="connector-title">Codexify</h2><section><h2 id="manage-heading">Manage app</h2><p>Connector details</p></section>
<section role="dialog" id="settings-approval"><header id="approval-header"><h2 id="approval-heading">Tool permission</h2><button id="approve-setting">Approve</button></header></section>
</main><script>document.querySelector('#refresh').onclick=function(){document.body.dataset.refreshes=String(Number(document.body.dataset.refreshes||0)+1);this.disabled=true;setTimeout(()=>this.disabled=false,100);};</script>`;
try {
  const context=await browser.newContext({viewport:{width:1280,height:800}});
  await context.route("**/*",route=>new URL(route.request().url()).pathname==='/api/auth/session'
    ?route.fulfill({json:{user:{id:'fixture-user'},account:{id:'fixture-account',planType:'pro',structure:'personal'}}})
    :route.fulfill({contentType:"text/html",body:new URL(route.request().url()).pathname.startsWith('/settings')?settingsFixture:fixture}));
  await installMinimalChatGptPage(context,BONES_SHELL_CSS,1,"bones");
  const page=await context.newPage();await page.goto("https://chatgpt.com/c/bones-shell-fixture");
  const css=async(selector:string,key:string)=>page.locator(selector).evaluate((el,property)=>getComputedStyle(el).getPropertyValue(property),key);
  const visible=async(selector:string)=>assert.equal(await page.locator(selector).isVisible(),true,`${selector} must remain usable`);
  const bones=async()=>{
    for(const selector of ["#modern-nav","#shell-header","#voice"])assert.equal(await page.locator(selector).isVisible(),false,`${selector} must be hidden in Bones`);
    assert.equal(await css("#rail","width"),"0px");
    for(const selector of ["#profile","#new-chat"]){await visible(selector);assert.equal(await css(selector,"position"),"fixed");}
    assert.equal(await css("#profile","bottom"),"6px");assert.equal(await css("#new-chat","top"),"6px");
    for(const selector of ["#portal-composer","#prompt-textarea"]){assert.equal(await css(selector,"border-radius"),"0px");assert.equal(await css(selector,"box-shadow"),"none");}
    await visible("#legacy-header");await visible("#legacy-model");
  };
  await bones();assert.equal(await page.locator("#welcome").isVisible(),false);
  await page.locator("#profile").click();await page.locator("#profile-action").click();await page.locator("#profile-menu").evaluate(el=>{(el as HTMLElement).hidden=true;});
  await page.locator("#new-chat").click();await page.locator("#legacy-model").click();
  assert.equal(await page.locator("#prompt-textarea").getAttribute("contenteditable"),"plaintext-only");
  assert.equal(await page.locator("main #prompt-textarea").count(),0,"Composer must really be in an external portal");
  for(const [trigger,choice,menu] of [["#apps","#app-choice","#apps-menu"],["#effort","#effort-choice","#effort-menu"]]){
    await page.locator(trigger!).click();await page.locator(choice!).click();await page.locator(menu!).evaluate(el=>{(el as HTMLElement).hidden=true;});
  }
  const prompt="Exact  Bones prompt\nwith approval and tool continuation";
  const composer=await activeComposer(page);await insertExactPrompt(composer,prompt);assert.equal(await composerText(composer),prompt);
  await page.locator('[data-testid="send-button"]').click();await page.locator('[data-testid="stop-button"]').click();
  for(const selector of ["#answer-header","#answer-heading","#copy","#approval","#error"])await visible(selector);
  await page.locator("#copy").click();await page.locator("#approve").click();
  const actions=await page.evaluate(()=>({clicks:JSON.parse(document.body.dataset.clicks??"{}"),sent:JSON.parse(document.body.dataset.sent??"[]")}));
  assert.deepEqual(actions.sent,[prompt]);
  for(const id of ["profile","profile-action","new-chat","legacy-model","apps","app-choice","effort","effort-choice","send","stop","copy","approve"])assert.equal(actions.clicks[id],1,`${id} must handle a real click`);
  // Hydration and restored-tab styling must repair the same active mode.
  await page.evaluate(()=>{const replacement=document.createElement("head");document.head.replaceWith(replacement);});
  await styleRestoredChatGptPage(page,BONES_SHELL_CSS,1,"bones");await bones();await visible("#answer-heading");
  let revision=2;
  for(const level of ["simplified","off"] as BrowserSimplicity[]){
    await installMinimalChatGptPage(context,BONES_SHELL_CSS,revision,level);await styleRestoredChatGptPage(page,BONES_SHELL_CSS,revision++,level);
    await page.goto(`https://chatgpt.com/c/${level}-shell-fixture`);
    for(const selector of ["#rail","#modern-nav","#shell-header","#voice","#legacy-header"])await visible(selector);
    assert.notEqual(await css("#rail","width"),"0px");assert.notEqual(await css("#profile","position"),"fixed");
    assert.notEqual(await css("#portal-composer","border-radius"),"0px","Bones must not leak into another mode");
    assert.equal(await page.locator("html").getAttribute("data-codexify-simplicity"),level);
  }
  await installMinimalChatGptPage(context,BONES_SHELL_CSS,revision,"bones");await styleRestoredChatGptPage(page,BONES_SHELL_CSS,revision,"bones");
  await page.goto("https://chatgpt.com/c/bones-navigation-fixture");await bones();assert.equal(await page.locator("#welcome").isVisible(),false);
  // A portal composer can remain mounted while SPA settings replace MAIN.
  // Route ownership, rather than generic headings or composer presence, must
  // decide which shell chrome may be hidden.
  await page.evaluate(()=>{document.body.dataset.chatMain=document.querySelector('main')!.innerHTML;history.pushState({},'', '/settings/plugins-settings/plugin_asdk_app_fixture');document.querySelector('main')!.innerHTML='<header id="spa-settings-header"><h1 id="spa-settings-title">Codexify</h1><button id="spa-refresh">Refresh tools</button></header>';});
  for(const selector of ['#spa-settings-header','#spa-settings-title','#spa-refresh'])await visible(selector);
  assert.equal(await page.locator('#prompt-textarea').count(),1,'The external composer intentionally remains during SPA settings');
  assert.equal(await page.locator('html').getAttribute('data-codexify-chat-page'),'false');
  await page.evaluate(()=>{history.replaceState({},'', '/c/spa-chat-return');document.querySelector('main')!.innerHTML=document.body.dataset.chatMain!;});
  await bones();assert.equal(await page.locator('#welcome').isVisible(),false);
  await page.goto('https://chatgpt.com/g/g-p-fixture/project');await bones();assert.equal(await page.locator('#welcome').isVisible(),false);
  const account=(await readChatGptAccount(page)).fingerprint;
  const inspectedAt=performance.now();await inspectConnectorSettings(page,'asdk_app_fixture','Codexify',account);const settingsInspectionMs=performance.now()-inspectedAt;
  const settingsVisible=async()=>{for(const selector of ['#settings-header','#settings-title','#settings-tabs','#connector-title','#manage-heading','#refresh','#settings-approval','#approval-header','#approval-heading'])await visible(selector);};
  await settingsVisible();assert.equal(await page.locator('html').getAttribute('data-codexify-chat-page'),'false');
  const refresh=await beginConnectorToolRefresh(page,'asdk_app_fixture','Codexify',account);
  try {assert.equal(await refresh.wait(AbortSignal.timeout(2_000)),true);}finally{await refresh.dispose();}
  assert.equal(await page.locator('body').getAttribute('data-refreshes'),'1','The real refresh helper must click the fixture control exactly once');
  await page.evaluate(()=>document.head.replaceWith(document.createElement('head')));
  await styleRestoredChatGptPage(page,BONES_SHELL_CSS,revision,'bones');await settingsVisible();
  for(const path of ['/settings/billing','/settings/plugins-settings/plugin_asdk_app_fixture/permissions','/unknown-maintenance-page']){
    await page.goto(`https://chatgpt.com${path}`);
    if(path.startsWith('/settings'))await settingsVisible();
    else {await visible('#shell-header');await visible('#welcome');}
    assert.equal(await page.locator('html').getAttribute('data-codexify-chat-page'),'false');
  }
  console.log(JSON.stringify({ok:true,settingsInspectionMs,settingsRefreshClicks:1,checks:["modern shell hidden","profile/newchat fixed and clickable","legacy model control","portal plaintext composer","Apps/effort menus","exact Send","Stop/Copy/approval/error controls","answer headings","head replacement/restored style","Simplified/Off isolation with extra Bones CSS","navigation persistence","SPA push/replace route changes with retained composer","project chat welcome hidden","full-page plugin and billing headings preserved","actual settings inspection and Refresh helper","settings approvals preserved","unknown routes fail open"],scope:"Fresh routed Chrome fixture; no live profile/account"}));
} finally {await browser.close();}
