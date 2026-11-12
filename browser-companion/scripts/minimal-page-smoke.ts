import assert from "node:assert/strict";
import {writeFile} from "node:fs/promises";
import {resolve} from "node:path";
import {chromium} from "patchright";
import {privateChromeExecutable} from "../src/chrome-executable";
import {installMinimalChatGptPage, styleRestoredChatGptPage, MINIMAL_CHATGPT_CSS} from "../src/minimal-page";
import {activeComposer, composerText, insertExactPrompt} from "../src/composer";
import {captureSubmissionBaseline, readTurnDomState} from "../src/turn-binding";
import {responseDomSnapshot} from "../src/response-observer";
import {inspectPresentation} from "../src/presentation";

// Run the bundled script under Node on Windows, matching the packaged driver.
// All requests are fixtures; this never opens the managed account/profile.
const executablePath=process.env.CODEXIFY_TEST_CHROME_PATH
  ?? await privateChromeExecutable(resolve(".browsers",`${process.platform}-${process.arch}`));
const browser=await chromium.launch({executablePath, headless:true, timeout:20_000});
try {
  const context=await browser.newContext({viewport:{width:1280,height:800},reducedMotion:"reduce"});
  await installMinimalChatGptPage(context);
  let sends=0;
  const page=await context.newPage();
  await page.exposeFunction("fixtureSend",()=>{sends++;});
  const fixture=`<!doctype html><html><head><style nonce="fixture">
    body { margin:0; font:16px sans-serif; }
    main { width:900px; margin:auto; }
    article,[data-turn-key] { padding:12px; }
    .markdown p { margin:8px 0; }
    pre { box-shadow:0 2px 8px #777; }
    .moving { animation:loading 2s infinite; transition:opacity 1s; }
    @keyframes loading { from { opacity:.6; } to { opacity:1; } }
    form { position:fixed; bottom:0; background:white; width:100%; padding-bottom:22px; }
    #prompt-textarea { min-height:25px; white-space:pre-wrap; }
  </style></head><body><div style="min-width:340px;width:340px"><div id="app-shell-sidebar"><button id="account">Account</button><div class="_ConversationSidebar-fixture">History</div></div></div><div class="group/home-suggestions">Suggestions</div><nav aria-label="Chat history">History</nav>
    <header><button aria-haspopup="menu" id="model">Model</button></header>
    <main id="turns"></main>
    <aside role="dialog" id="approval">Tool permission <button>Approve</button></aside>
    <form data-chatgpt-composer><div id="prompt-textarea" contenteditable="true" role="textbox"></div>
      <button data-testid="send-button" type="submit">Send</button>
      <button data-testid="stop-button" type="button">Stop</button>
      <button id="connector" type="button" aria-haspopup="menu">Apps</button>
      <span class="moving">Working</span>
    </form><script>
      document.querySelector('form').onsubmit=event=>{
        event.preventDefault(); window.fixtureSend();
        const group=document.createElement('div');group.dataset.turnKey='accepted';
        const user=document.createElement('div');user.setAttribute('data-user-message-bubble','');
        user.textContent=document.querySelector('#prompt-textarea').textContent;
        const answer=document.createElement('div');answer.dataset.conversationRole='assistant';
        answer.innerHTML='<div class="markdown" data-markdown-text-style="assistant-message"><p>Fresh answer</p></div><button data-testid="copy-turn-action-button">Copy</button>';
        const unit=document.createElement('section');unit.dataset.contentSearchUnitKey='fresh-answer';unit.append(answer);
        group.append(user,unit);document.querySelector('main').append(group);
        document.querySelector('#prompt-textarea').textContent='';
      };
    </script></body></html>`;
  await context.route("**/*",route=>route.fulfill({contentType:"text/html",body:fixture,
    ...(new URL(route.request().url()).pathname==="/c/csp-fixture"?{headers:{"Content-Security-Policy":"style-src 'nonce-fixture'"}}:{})}));
  await page.goto("https://chatgpt.com/c/minimal-fixture",{waitUntil:"domcontentloaded"});
  const turnCount=320;
  await page.evaluate(count=>{
    const host=document.querySelector("main")!;
    for(let index=0;index<count;index++) {
      const turn=document.createElement("article");
      let outer:HTMLElement=turn;
      if(index%2===0)turn.setAttribute("data-turn-key",`old-${index}`);
      else {
        turn.dataset.testid=`conversation-turn-${index}`;turn.dataset.turnId=`old-${index}`;turn.dataset.turn="assistant";
        outer=document.createElement("section");outer.dataset.turnIdContainer=`old-${index}`;outer.append(turn);
      }
      // Include nested legacy shells inside the new grouped renderer.
      turn.innerHTML=`<section data-content-search-unit-key="answer-${index}" ${index%2===0?`data-testid="conversation-turn-nested-${index}"`:""}>
        <div data-message-author-role="assistant" data-conversation-role="assistant"><div class="markdown" data-markdown-text-style="assistant-message">`+
        Array.from({length:48},(_,row)=>`<p>Turn ${index}, line ${row}: ${"Source code and tool output with wrapping text. ".repeat(6)}</p>`).join("")+
        `</div><pre><code>tool_call_${index}()</code></pre><button data-testid="copy-turn-action-button">Copy</button></div></section>`;
      host.append(outer);
    }
  },turnCount);
  const css=async(selector:string,property:string)=>page.locator(selector).first().evaluate((el,key)=>getComputedStyle(el).getPropertyValue(key),property);
  assert.equal(await css('nav[aria-label="Chat history"]',"display"),"none");
  assert.equal(await css('#codexify-fast-indicator',"position"),"fixed");
  assert.equal(await css('#prompt-textarea',"font-size"),"14px");
  assert.equal(await css('#app-shell-sidebar',"width"),'52px');
  assert.equal(await page.locator('#app-shell-sidebar').evaluate(el=>getComputedStyle(el.parentElement!).width),'52px');
  assert.equal(await css('[class*="ConversationSidebar"]','display'),'none');
  assert.equal(await css('[class~="group/home-suggestions"]','display'),'none');
  assert.notEqual(await css('#account','display'),'none','Keep account/navigation rail accessible');
  await page.evaluate(()=>{
    document.getElementById('codexify-minimal-chatgpt')!.remove();
    document.getElementById('codexify-fast-indicator')!.remove();
  });
  await page.locator('#codexify-fast-indicator').waitFor({state:"visible"});
  assert.equal(await page.locator('#codexify-minimal-chatgpt').count(),1,"Hydration must not permanently remove fast mode");
  const diagnostic=await inspectPresentation(page);
  assert.equal(diagnostic.mode,"fast-v2");
  assert.equal(diagnostic.badgeVisible,true);
  assert.equal(diagnostic.turnCount,turnCount);
  assert.equal(JSON.stringify(diagnostic).includes("Source code and tool output"),false,"Diagnostics must exclude conversation text");
  for(const selector of ['#prompt-textarea','[data-testid="send-button"]','[data-testid="stop-button"]','#model','#connector','#approval']) {
    assert.notEqual(await css(selector,"display"),"none",`${selector} must remain usable`);
  }
  assert.equal(await page.evaluate(()=>matchMedia("(prefers-reduced-motion: reduce)").matches),true);
  assert(parseFloat(await css('.moving',"animation-duration"))<.001);
  assert.equal(await css('[data-turn-key="old-0"]',"content-visibility"),"auto");
  assert.equal(await css('[data-turn-key="old-0"] section',"content-visibility"),"visible","Nested turn shells must not be contained twice");
  assert.equal(await css('[data-turn-id-container="old-1"]',"content-visibility"),"auto");
  assert.equal(await css('[data-turn-id="old-1"]',"content-visibility"),"visible");
  for(let index=turnCount-4;index<turnCount;index++) {
    assert.equal(await css(`main > :nth-child(${index+1})`,"content-visibility"),"visible");
  }
  // Expanded controls and focused history must immediately leave containment.
  const oldCopy=page.locator('[data-turn-key="old-0"] button');
  await oldCopy.evaluate(el=>el.setAttribute("aria-expanded","true"));
  assert.equal(await css('[data-turn-key="old-0"]',"content-visibility"),"visible");
  await oldCopy.evaluate(el=>el.removeAttribute("aria-expanded"));
  await oldCopy.focus();
  assert.equal(await css('[data-turn-key="old-0"]',"content-visibility"),"visible");
  await page.locator("#prompt-textarea").focus();

  const state=await readTurnDomState(page);
  assert.equal(state.responseIdentities.length,turnCount);
  assert.equal(state.generationRunning,true);
  const oldSnapshot=await responseDomSnapshot(page.locator('[data-turn-key="old-0"]'));
  assert(oldSnapshot.visibleText.includes("Turn 0, line 47"),"Offscreen historical output must remain extractable");
  const text="Exact  prompt\nwith continuation evidence";
  const baseline=await captureSubmissionBaseline(page,text);
  const composer=await activeComposer(page);
  await insertExactPrompt(composer,text);
  assert.equal(await composerText(composer),text);
  await page.locator('[data-testid="send-button"]').click();
  assert.equal(sends,1);
  assert.deepEqual((await readTurnDomState(page,baseline.browserBaselineId!)).newResponseIdentities,["group:assistant:accepted"]);
  const fresh=await responseDomSnapshot(page.locator('[data-turn-key="accepted"]'));
  assert.equal(fresh.visibleText,"Fresh answer");
  assert.equal(fresh.completionActionVisible,true);

  // Full navigation, SPA replacement and restored styling retain one stylesheet.
  await styleRestoredChatGptPage(page);
  assert.equal(await page.locator('#codexify-minimal-chatgpt').count(),1);
  const other=await context.newPage();
  await other.goto("https://fixture.invalid/");
  assert.equal(await other.locator('#codexify-minimal-chatgpt').count(),0,"Account/other-origin pages must be untouched");
  await other.close();
  const strict=await context.newPage();
  await strict.goto("https://chatgpt.com/c/csp-fixture");
  const strictStyle=await strict.locator('#codexify-fast-indicator').evaluate(el=>({position:getComputedStyle(el).position,
    inlineSheetBlocked:(document.getElementById('codexify-minimal-chatgpt') as HTMLStyleElement).sheet===null}));
  assert.equal(strictStyle.inlineSheetBlocked,true,"Fixture must actually reject the old inline-style mechanism");
  assert.equal(strictStyle.position,"fixed","Constructed fast-mode styles must survive strict site CSP");
  await strict.close();

  // Alternate the old and new styles in the SAME document/process. Compare
  // forced width reflows of a long chat, not model/network/total action speed.
  const oldCss='nav[aria-label="Chat history"] { display:none !important; }';
  const session=await context.newCDPSession(page);
  await session.send("Performance.enable");
  const metrics=async()=>{
    const result=await session.send("Performance.getMetrics");
    return Object.fromEntries(result.metrics.map(({name,value})=>[name,value]));
  };
  const samples:{mode:string;layoutMs:number;styleMs:number;wallMs:number}[]=[];
  for(const mode of ["baseline","minimal","minimal","baseline","baseline","minimal"]) {
    await page.evaluate(css=>{
      // The real fast mode also survives HEAD replacement via an adopted
      // sheet. Remove that second copy in BOTH benchmark arms so the baseline
      // really exercises the old presentation, rather than measuring fast twice.
      document.adoptedStyleSheets=[];
      document.querySelector('#codexify-minimal-chatgpt')!.textContent=css;
      scrollTo(0,document.body.scrollHeight);
    },mode==="minimal"?MINIMAL_CHATGPT_CSS:oldCss);
    await page.evaluate(()=>new Promise<void>(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve()))));
    const before=await metrics();
    const start=performance.now();
    await page.evaluate(()=>{
      const main=document.querySelector('main')!;
      for(let index=0;index<8;index++) {
        main.style.width=`${index%2?900:850}px`;
        void document.body.offsetHeight;
      }
    });
    const wallMs=performance.now()-start;
    const after=await metrics();
    samples.push({mode,layoutMs:(after.LayoutDuration!-before.LayoutDuration!)*1000,styleMs:(after.RecalcStyleDuration!-before.RecalcStyleDuration!)*1000,wallMs});
  }
  const median=(values:number[])=>[...values].sort((a,b)=>a-b)[Math.floor(values.length/2)]!;
  const baselineLayoutMs=median(samples.filter(s=>s.mode==="baseline").map(s=>s.layoutMs));
  const minimalLayoutMs=median(samples.filter(s=>s.mode==="minimal").map(s=>s.layoutMs));
  assert(minimalLayoutMs<baselineLayoutMs,"History containment must reduce actual Chrome layout work");
  const result={ok:true,turns:turnCount,paragraphs:turnCount*48,checks:["two renderers","nested shells","newest four","focus/expanded controls","old response extraction","exact composer","single Send","new turn receipt","fresh response","menus/approval/Stop","other origins","idempotent restoration","strict CSP blocks old inline style but fast mode applies"],baselineLayoutMs,minimalLayoutMs,layoutReductionPercent:100*(1-minimalLayoutMs/baselineLayoutMs),samples,scope:"Synthetic long-chat layout, not network/model/end-to-end speed"};
  await page.reload({waitUntil:"domcontentloaded"});
  await page.locator('#codexify-fast-indicator').waitFor({state:"visible"});
  assert.equal(await page.locator('#codexify-minimal-chatgpt').count(),1);
  const liveCss='#prompt-textarea {font-size:13px !important;}';
  await installMinimalChatGptPage(context,liveCss,2);
  await styleRestoredChatGptPage(page,liveCss,2);
  await styleRestoredChatGptPage(page,'#prompt-textarea {font-size:60px !important;}',1);
  assert.equal(await css('#prompt-textarea','font-size'),'13px','An older init script must not replace a newer live style');
  await page.reload({waitUntil:'domcontentloaded'});
  assert.equal(await css('#prompt-textarea','font-size'),'13px','Live styles must survive later navigation regardless of init order');
  // Both levels preserve actual automation controls, and turning the feature
  // off removes every owned sheet instead of leaving invisible style policy.
  await installMinimalChatGptPage(context,"",3,"bones");
  await styleRestoredChatGptPage(page,"",3,"bones");
  assert.equal(await css('#app-shell-sidebar','width'),'38px');
  assert.equal(await page.locator('html').getAttribute('data-codexify-simplicity'),'bones');
  for(const selector of ['#prompt-textarea','[data-testid="send-button"]','[data-testid="stop-button"]','#model','#connector','#approval','#account']) {
    assert.equal(await page.locator(selector).isVisible(),true,`Bones must retain ${selector}`);
  }
  const bonesPrompt='Bones exact prompt\nwith required tool approvals';
  await insertExactPrompt(await activeComposer(page),bonesPrompt);
  assert.equal(await composerText(await activeComposer(page)),bonesPrompt);
  await page.locator('[data-testid="send-button"]').click();
  assert.equal(sends,2,'Bones must still send exactly once');
  assert.equal((await responseDomSnapshot(page.locator('[data-turn-key="accepted"]'))).visibleText,'Fresh answer');
  assert.equal((await inspectPresentation(page)).simplicity,'bones');
  if(process.env.CODEXIFY_BONES_SCREENSHOT)await page.screenshot({path:process.env.CODEXIFY_BONES_SCREENSHOT,fullPage:false});
  await styleRestoredChatGptPage(page,"",4,"off");
  assert.equal(await page.locator('#codexify-minimal-chatgpt').count(),0);
  assert.equal(await page.locator('#codexify-fast-indicator').count(),0);
  assert.notEqual(await css('nav[aria-label="Chat history"]','display'),'none');
  await styleRestoredChatGptPage(page,"",3,"bones");
  assert.equal(await page.locator('#codexify-fast-indicator').count(),0,'An older controller cannot turn simplification back on');
  assert.equal((await inspectPresentation(page)).ownedAdoptedStylesheets,0);
  await installMinimalChatGptPage(context,"",4,"off");
  await page.reload({waitUntil:'domcontentloaded'});
  assert.equal(await page.locator('#codexify-fast-indicator').count(),0,'Off survives navigation after previous level init scripts');
  await installMinimalChatGptPage(context,"",5,"bones");
  await styleRestoredChatGptPage(page,"",5,"bones");
  await page.reload({waitUntil:'domcontentloaded'});
  assert.equal(await css('#app-shell-sidebar','width'),'38px','Bones survives later navigation');
  result.checks.push('Bones controls/account/approval retained','Bones exact prompt and single Send','level survives navigation','Off removes all owned CSS','stale level cannot re-enable');
  if(process.env.CODEXIFY_MINIMAL_UI_RESULT)await writeFile(process.env.CODEXIFY_MINIMAL_UI_RESULT,JSON.stringify(result,null,2));
  console.log(JSON.stringify(result));
} finally {await browser.close();}
