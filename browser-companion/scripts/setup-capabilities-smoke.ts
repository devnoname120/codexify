import assert from "node:assert/strict";
import {resolve} from "node:path";
import {chromium,type Page} from "patchright";
import {privateChromeExecutable} from "../src/chrome-executable";
import {detectChatGptAccountCapabilities,discoverChatGptEfforts,activateChatGptEffortMenu,CHATGPT_EFFORT_CONTROL_SELECTOR} from "../src/session";

// All requests use a fresh routed fixture; this never opens a managed profile.
const executablePath=process.env.CODEXIFY_TEST_CHROME_PATH??await privateChromeExecutable(resolve(".browsers",`${process.platform}-${process.arch}`));
const browser=await chromium.launch({executablePath,headless:true,timeout:20_000});
const fixture=`<!doctype html><style>#prompt-textarea{min-height:24px}[data-model-reasoning-effort-slider]{height:24px;min-width:240px}</style>
<form data-chatgpt-composer><div id="prompt-textarea" contenteditable="true" role="textbox"></div>
<button type="button" id="effort" data-testid="model-switcher-dropdown-button" aria-haspopup="menu" aria-controls="picker" aria-expanded="false">Medium</button></form>
<div id="picker" role="menu" hidden><div data-model-picker-view="advanced"><button role="menuitemradio" data-family="6" aria-checked="true">GPT-6</button></div>
<div id="range" role="menuitem" tabindex="0" aria-describedby="description"></div><span id="description"></span></div>
<script>
let value=1;const labels=['Fast','Medium','High'];window.ops={opens:0,arrows:0,escapes:0};function save(){document.body.dataset.ops=JSON.stringify(window.ops);}save();
const control=document.querySelector('#effort'),picker=document.querySelector('#picker'),range=document.querySelector('#range');
function render(){range.innerHTML='<div data-model-reasoning-effort-slider><span role="slider" aria-valuemin="0" aria-valuemax="2" aria-valuenow="'+value+'"></span>'+labels.map((label,i)=>'<span data-selected="'+(i===value)+'" data-locked="false" aria-label="'+label+'"></span>').join('')+'</div>';document.querySelector('#description').textContent='6 '+labels[value]+', '+(value+1)+' of 3.';}
control.onclick=()=>{window.ops.opens++;save();picker.hidden=false;control.setAttribute('aria-expanded','true');};
range.onkeydown=event=>{if(!['ArrowLeft','ArrowRight'].includes(event.key))return;event.preventDefault();window.ops.arrows++;save();value=Math.max(0,Math.min(2,value+(event.key==='ArrowRight'?1:-1)));render();};
document.addEventListener('keydown',event=>{if(event.key==='Escape'){window.ops.escapes++;save();picker.hidden=true;control.setAttribute('aria-expanded','false');}});render();
</script>`;
try {
  const context=await browser.newContext();await context.route("**/*",route=>route.fulfill({contentType:"text/html",body:fixture}));
  const page=await context.newPage();await page.goto("https://chatgpt.com/");
  const ops=()=>page.evaluate(()=>JSON.parse(document.body.dataset.ops??"{}") as Record<string,number>);
  const started=performance.now();const capabilities=await detectChatGptAccountCapabilities(page,{onProgress:step=>console.error(`[setup-fixture] ${step}`)});const discoveryMs=performance.now()-started;
  assert.equal(capabilities.models.length,1);assert.equal(capabilities.models[0]!.efforts.length,3);
  assert.equal(await page.locator('[role="slider"]').getAttribute('aria-valuenow'),'1');
  assert.deepEqual(await ops(),{opens:1,arrows:4,escapes:1});

  console.error('[setup-fixture] cancellation rows');await page.goto("https://chatgpt.com/");
  const rowsAbort=new AbortController();let rowReads=0;let releaseRows!:()=>void;
  // A transport read may settle after the deadline. It must not keep ownership
  // indefinitely or cause restoration input when its late result arrives.
  const delayedRows=new Promise<void>(resolve=>{releaseRows=resolve;});
  const wrappedPage=new Proxy(page,{get(target,key){
    if(key==='locator')return (...args:Parameters<Page['locator']>)=>{
      const locator=target.locator(...args);
      if(args[0]!=='[id="picker"]')return locator;
      return new Proxy(locator,{get(menu,menuKey){
        if(menuKey==='getByRole')return (...roleArgs:Parameters<typeof menu.getByRole>)=>{
          const rows=menu.getByRole(...roleArgs);
          return new Proxy(rows,{get(rowTarget,rowKey){
            if(rowKey==='evaluateAll')return async (...readArgs:unknown[])=>{rowReads++;setTimeout(()=>rowsAbort.abort(),20);await delayedRows;return (rowTarget.evaluateAll as Function).apply(rowTarget,readArgs);};
            const value=Reflect.get(rowTarget,rowKey);return typeof value==='function'?value.bind(rowTarget):value;
          }});
        };
        const value=Reflect.get(menu,menuKey);return typeof value==='function'?value.bind(menu):value;
      }});
    };
    const value=Reflect.get(target,key);return typeof value==='function'?value.bind(target):value;
  }});
  const cancelledAt=performance.now();await assert.rejects(detectChatGptAccountCapabilities(wrappedPage,{signal:rowsAbort.signal}),{name:'AbortError'});
  const cancelledRowsMs=performance.now()-cancelledAt;assert.equal(rowReads,1);const afterRowsAbort=await ops();
  releaseRows();await new Promise(resolve=>setTimeout(resolve,80));assert.deepEqual(await ops(),afterRowsAbort);
  assert.deepEqual(afterRowsAbort,{opens:1,arrows:0,escapes:0});assert(cancelledRowsMs<2_000);

  console.error('[setup-fixture] cancellation efforts');await page.goto("https://chatgpt.com/");const effortAbort=new AbortController();
  const menu=await activateChatGptEffortMenu(page,page.locator(CHATGPT_EFFORT_CONTROL_SELECTOR));
  const realSlider=menu.slider;let announcements=0;
  menu.slider=new Proxy(realSlider,{get(target,key){
    if(key==='getAttribute')return async (...args:Parameters<typeof realSlider.getAttribute>)=>{
      if(args[0]==='aria-valuetext'){announcements++;effortAbort.abort();}
      return target.getAttribute(...args);
    };
    const value=Reflect.get(target,key);return typeof value==='function'?value.bind(target):value;
  }});
  await assert.rejects(discoverChatGptEfforts(menu,effortAbort.signal),{name:'AbortError'});
  assert.equal(announcements,1);const afterEffortAbort=await ops();
  assert.deepEqual(afterEffortAbort,{opens:1,arrows:1,escapes:0});
  await new Promise(resolve=>setTimeout(resolve,80));assert.deepEqual(await ops(),afterEffortAbort);
  console.log(JSON.stringify({ok:true,discoveryMs,cancelledRowsMs,normalOperations:{opens:1,arrows:4,escapes:1},cancelledRowsOperations:afterRowsAbort,cancelledEffortOperations:afterEffortAbort,checks:['native effort discovery and restoration','cancelled model rows settle promptly','late row result causes no input','cancelled effort read causes no restore input']}));
} finally {await browser.close();}
