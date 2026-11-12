import assert from 'node:assert/strict';
import {chromium} from 'patchright';
import {selectAvailableModelAndEffort,assertSelectedModelAndEffort} from '../src/model-selection';
import {ChatGptBrowserDriver,type SingleBrowserSurface} from '../src/chatgpt-driver';
import {protocolBuild} from '../src/protocol';

const browser=await chromium.launch({executablePath:process.env.CODEXIFY_TEST_CHROME_PATH,headless:true});
try {
 const context=await browser.newContext(),page=await context.newPage();
 let accountId='fixture-account',planType='pro',hydrateAfter=0;
 await context.route('**/*',route=>new URL(route.request().url()).pathname==='/api/auth/session'
 ?route.fulfill({json:{user:{id:'fixture-user'},account:{id:accountId,planType,structure:'personal'}}})
 :route.fulfill({contentType:'text/html',body:`<!doctype html><style>[data-model-reasoning-effort-slider]{height:24px;min-width:240px}</style><body>
 <form data-chatgpt-composer><div id="prompt-textarea" role="textbox" contenteditable="true" style="min-height:24px"></div><button type="button" id="effort" data-testid="model-switcher-dropdown-button" aria-haspopup="menu" aria-controls="picker" aria-expanded="false">Medium</button></form>
 <div id="picker" role="menu" hidden><div data-model-picker-view="advanced"><button role="menuitemradio" data-family="6" aria-checked="true">GPT-6</button></div><div id="range" role="menuitem" tabindex="0" aria-describedby="description"></div><span id="description"></span></div>
 <script>
 let value=1;const labels=['Fast','Medium','High','Extra High','Pro'];
 const control=document.querySelector('#effort'),picker=document.querySelector('#picker'),range=document.querySelector('#range');
 window.renderModel=()=>{range.innerHTML='<div data-model-reasoning-effort-slider><span role="slider" aria-valuemin="0" aria-valuemax="4" aria-valuenow="'+value+'"></span>'+labels.map((label,i)=>'<span data-selected="'+(i===value)+'" data-locked="false" aria-label="'+label+'"></span>').join('')+'</div>';document.querySelector('#description').textContent='6 '+labels[value]+', '+(value+1)+' of 5.';control.textContent=labels[value];};
 window.changeValue=next=>{value=next;window.renderModel();};window.renderModel();
 control.onclick=()=>{document.body.dataset.opens=String(Number(document.body.dataset.opens||0)+1);picker.hidden=false;control.setAttribute('aria-expanded','true');};
 document.querySelector('[data-family]').onclick=()=>window.renderModel();
 range.onkeydown=event=>{if(!['ArrowLeft','ArrowRight'].includes(event.key))return;event.preventDefault();value=Math.max(0,Math.min(4,value+(event.key==='ArrowRight'?1:-1)));window.renderModel();};
 document.addEventListener('keydown',event=>{if(event.key==='Escape'){picker.hidden=true;control.setAttribute('aria-expanded','false');}});
 if(${hydrateAfter}){document.querySelector('form').hidden=true;setTimeout(()=>{document.querySelector('form').hidden=false;},${hydrateAfter});}
 </script>`}));
 const runs=[];
 for(const deferred of [false,true]){
  await page.goto('https://chatgpt.com/c/model-speed-fixture');
  const started=performance.now();
  const selected=await selectAvailableModelAndEffort(page,'gpt-6-sol','xhigh',undefined,undefined,undefined,deferred);
  await assertSelectedModelAndEffort(page,selected);
  const opens=Number(await page.locator('body').getAttribute('data-opens'));
  assert.equal(opens,deferred?2:3,'Deferred confirmation must remove exactly one redundant picker visit');
  runs.push({deferred,opens,elapsedMs:performance.now()-started});
  await page.locator('#effort').click();await page.locator('#range').press('ArrowLeft');await page.keyboard.press('Escape');
  await page.locator('#effort').evaluate(el=>{el.textContent='Extra High';});
  await assert.rejects(()=>assertSelectedModelAndEffort(page,selected),/requested effort/,'A stale label must not conceal a changed slider value');
 }
 const surface:SingleBrowserSurface={page:async()=>page,navigate:async url=>{await page.goto(url);return page;},beginAction:async()=>{},endAction:async()=>{},reveal:async()=>{}};
 const driver=new ChatGptBrowserDriver(surface,protocolBuild('test-build'));
 await page.goto('https://chatgpt.com/');
 const freshStarted=performance.now();const original=await driver.capabilities();const freshCatalogMs=performance.now()-freshStarted;
 const opensBefore=Number(await page.locator('body').getAttribute('data-opens'));
 const cachedStarted=performance.now();const cached=await driver.prepareNewChat(null);const cachedPreparationMs=performance.now()-cachedStarted;
 assert.deepEqual(cached,original);
 assert.equal(Number(await page.locator('body').getAttribute('data-opens')),opensBefore,'Batch preparation must not rediscover an unchanged account catalogue');
 await driver.capabilities();
 const afterExplicit=Number(await page.locator('body').getAttribute('data-opens'));
 assert(afterExplicit>opensBefore,'Explicit catalogue refresh must still inspect the live picker');
 accountId='different-account';const changed=await driver.prepareNewChat(null);
 assert.notEqual(changed.accountFingerprint,original.accountFingerprint);
 const afterAccount=Number(await page.locator('body').getAttribute('data-opens'));assert(afterAccount>afterExplicit,'Changed accounts must rescan');
 planType='team';await driver.prepareNewChat(null);
 assert(Number(await page.locator('body').getAttribute('data-opens'))>afterAccount,'Changed plans must rescan');
 hydrateAfter=350;await page.goto('https://chatgpt.com/');
 await driver.capabilities();
 console.log(JSON.stringify({ok:true,runs,freshCatalogMs,cachedPreparationMs,checks:['one fewer picker visit','fresh final slider and family proof','wrong value with unchanged label rejected','batch catalogue reuse','explicit refresh remains fresh','account and plan changes rescan','catalogue hydration succeeds on first attempt']}));
} finally {await browser.close();}
