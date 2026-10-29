import {beforeAll,afterAll,expect,test} from "bun:test";
import {chromium,type BrowserContext,type Page} from "patchright";
import {privateChromeExecutable} from "../src/chrome-executable";
import {detectChatGptAccountCapabilities,requireChatGptEffortControl} from "../src/session";
import {selectPairingMedium} from "../src/pairing-model";
import {selectModelAndEffort,assertSelectedModelAndEffort} from "../src/model-selection";
const native=process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME==="1";
let context:BrowserContext,page:Page;
beforeAll(async()=>{
 if(!native)return;
 const executablePath=await privateChromeExecutable(`${import.meta.dirname}/../.browsers/${process.platform}-${process.arch}`);
 const browser=await chromium.launch({channel:"chrome",executablePath,headless:true});
 context=await browser.newContext();page=await context.newPage();
},30_000);
afterAll(async()=>{const browser=context?.browser();await context?.close();await browser?.close();});
type Layout="inline"|"portal"|"late-portal"|"replaced-trigger-portal"|"power"|"mounted-power";
async function fixture(layout:Layout="inline",value=2,control:"known"|"localized"|"reasoning"="known",family="6") {
 await page.setContent(`<!doctype html><meta charset="utf-8"><style>#prompt-textarea{min-height:30px}[data-model-reasoning-effort-slider]{height:24px;min-width:240px}#model-slot[aria-hidden="true"]{opacity:0;pointer-events:none;position:absolute}</style>
 <form id="composer"><div id="prompt-textarea" role="textbox" contenteditable="true"></div>
 <button type="button" id="effort" ${control==="known"?'data-testid="model-switcher-dropdown-button"':control==="reasoning"?'data-composer-navigation-target="reasoning"':''} aria-label="思考量" aria-haspopup="menu" aria-controls="picker" aria-expanded="false">高</button>
 <button type="button" aria-haspopup="menu" aria-controls="attachment-menu">添付</button><button type="submit">送信</button></form>
 <div id="picker" role="menu" hidden><div ${layout.endsWith("power")?'data-model-picker-view="simple"':''} id="view">
 <button type="button" role="menuitem" id="model-toggle" ${layout.endsWith("power")?'data-model-picker-view-toggle="true"':''} aria-expanded="false" ${(layout==="portal"||layout==="replaced-trigger-portal")?'aria-controls="model-submenu"':''}>Instant ›</button><div id="model-slot"></div></div>
 <div id="range" role="menuitem" tabindex="0" aria-describedby="announcement"></div><span id="announcement"></span></div>
 <div role="menu" id="foreign-menu"><button role="menuitemradio" data-model-id="gpt-99-sol" aria-checked="true">GPT-99</button></div>
 <script>(()=>{
 const layout=${JSON.stringify(layout)},control=document.querySelector('#effort'),picker=document.querySelector('#picker'),range=document.querySelector('#range'),toggle=document.querySelector('#model-toggle');
 const labels=['Instant','中','高'];let family=${JSON.stringify(family)},values={'6':${value},'5.6':1,'7':${value}};
 document.body.dataset.sends='0';document.body.dataset.clicks='0';
 function render(){const value=values[family];range.innerHTML='<div data-model-reasoning-effort-slider><span role="slider" aria-valuemin="0" aria-valuemax="2" aria-valuenow="'+value+'"></span>'+labels.map((label,i)=>'<span data-selected="'+(value===i)+'" data-locked="false" aria-label="'+label+'"></span>').join('')+'</div>';document.querySelector('#announcement').textContent=labels[value]+'、3件中'+(value+1)+'件目。';control.textContent=labels[value];document.body.dataset.family=family;document.body.dataset.effort=String(value);}
 function closeModels(){document.querySelector('#model-submenu')?.remove();if(!toggle.isConnected)document.querySelector('#view').prepend(toggle);const slot=document.querySelector('#model-slot');if(layout==='mounted-power'){slot.inert=true;slot.setAttribute('aria-hidden','true');slot.querySelectorAll('[data-model-id]').forEach(row=>row.setAttribute('aria-checked',String(row.getAttribute('data-model-id')==='gpt-'+family+'-sol')));}else slot.innerHTML='';toggle.setAttribute('aria-expanded','false');if(layout==='late-portal')toggle.removeAttribute('aria-controls');document.querySelector('#view').setAttribute('data-model-picker-view','simple');range.hidden=false;}
 control.onclick=()=>{document.body.dataset.clicks=String(+document.body.dataset.clicks+1);picker.hidden=false;control.setAttribute('aria-expanded','true');};
 toggle.onclick=()=>{document.body.dataset.clicks=String(+document.body.dataset.clicks+1);toggle.setAttribute('aria-expanded','true');document.querySelector('#view').setAttribute('data-model-picker-view',layout==='replaced-trigger-portal'?'simple':'advanced');let scope=document.querySelector('#model-slot');scope.inert=false;scope.setAttribute('aria-hidden','false');if(layout==='portal'||layout==='late-portal'||layout==='replaced-trigger-portal'){scope=document.createElement('div');scope.id='model-submenu';scope.setAttribute('role','menu');document.body.append(scope);toggle.setAttribute('aria-controls','model-submenu');range.hidden=true;if(layout==='replaced-trigger-portal')toggle.remove();}
 const families=family==='7'?['7','6']:['6','5.6'];scope.innerHTML=families.map(f=>'<button role="menuitemradio" data-model-id="gpt-'+f+'-sol" aria-checked="'+(family===f)+'">GPT-'+f+'</button>').join('');scope.querySelectorAll('button').forEach(row=>row.onclick=()=>{family=row.getAttribute('data-model-id').split('-')[1];render();closeModels();});};
 range.onkeydown=event=>{if(event.key==='ArrowLeft'||event.key==='ArrowRight'){values[family]=Math.min(2,Math.max(0,values[family]+(event.key==='ArrowRight'?1:-1)));render();event.preventDefault();}};
 document.addEventListener('keydown',event=>{if(event.key==='Escape'){if(toggle.getAttribute('aria-expanded')==='true')closeModels();else{picker.hidden=true;control.setAttribute('aria-expanded','false');}}});
 document.querySelector('form').onsubmit=event=>{event.preventDefault();document.body.dataset.sends=String(+document.body.dataset.sends+1);};render();if(layout==='mounted-power'){toggle.onclick();closeModels();document.body.dataset.clicks='0';}
 })();</script>`);
}
for(const layout of ["inline","portal","late-portal","replaced-trigger-portal","power","mounted-power"] as const)test.skipIf(!native)(`Japanese ${layout} picker discovers models, restores High and verifies a model switch`,async()=>{
 await fixture(layout);
 const catalog=await detectChatGptAccountCapabilities(page);
 expect(catalog.models.map(model=>model.id)).toEqual(["gpt-6-sol","gpt-5.6-sol"]);
 expect(catalog.models[0]!.efforts.map(e=>[e.id,e.label])).toEqual([["low","Instant"],["medium","中"],["high","高"]]);
 expect(await page.evaluate(()=>[document.body.dataset.family,document.body.dataset.effort,document.body.dataset.sends])).toEqual(["6","2","0"]);
 expect(await page.locator('#effort').getAttribute('aria-expanded')).toBe('false');
 const selected=await selectModelAndEffort(page,"gpt-5.6-sol","high",catalog);await assertSelectedModelAndEffort(page,selected);
 expect(await page.evaluate(()=>[document.body.dataset.family,document.body.dataset.effort,document.body.dataset.sends])).toEqual(["5.6","2","0"]);
},30_000);
for(const control of ["localized","reasoning"] as const)test.skipIf(!native)(`Japanese ${control} effort control pairs from High and restores its original selection`,async()=>{
 await fixture("portal",2,control);const selection=await selectPairingMedium(page,new AbortController().signal);
 expect(await page.evaluate(()=>[document.body.dataset.family,document.body.dataset.effort])).toEqual(["6","1"]);
 await selection.verify(new AbortController().signal);await selection.restore(new AbortController().signal);
 expect(await page.evaluate(()=>[document.body.dataset.family,document.body.dataset.effort,document.body.dataset.sends])).toEqual(["6","2","0"]);
},20_000);
test.skipIf(!native)("pairing already at Japanese Medium remains on the same model and effort",async()=>{
 await fixture("inline",1,"localized");const selection=await selectPairingMedium(page,new AbortController().signal);
 await selection.verify(new AbortController().signal);await selection.restore(new AbortController().signal);
 expect(await page.evaluate(()=>[document.body.dataset.family,document.body.dataset.effort,document.body.dataset.sends])).toEqual(["6","1","0"]);
},15_000);
test.skipIf(!native)("ambiguous or missing owned effort controls fail before any click with a specific stage",async()=>{
 await fixture("inline",2,"localized");await page.locator('#effort').evaluate(node=>{const copy=node.cloneNode(true) as HTMLElement;copy.id='duplicate-effort';node.after(copy);});
 const failure=await selectPairingMedium(page,AbortSignal.timeout(2_000)).catch(error=>error);
 expect(failure).toMatchObject({code:"selector_drift",phase:"effort_control_lookup",sendAccepted:false});
 expect(await page.evaluate(()=>document.body.dataset.clicks)).toBe("0");await page.locator('button[aria-label="思考量"]').evaluateAll(nodes=>nodes.forEach(node=>node.remove()));
 const missing=await requireChatGptEffortControl(page.locator('form'),undefined,50).catch(error=>error);
 expect(missing).toMatchObject({phase:"effort_control_lookup",sendAccepted:false});
 expect(await page.evaluate(()=>document.body.dataset.clicks)).toBe("0");
},15_000);
test.skipIf(!native)("a selected unsupported family cannot be inferred from the effort or a different model row",async()=>{
 await fixture("inline",2,"known","7");
 const failure=await detectChatGptAccountCapabilities(page).catch(error=>error);
 expect(failure).toMatchObject({code:"model_unavailable",phase:"model_identity",sendAccepted:false});
 expect(await page.evaluate(()=>[document.body.dataset.family,document.body.dataset.effort,document.body.dataset.sends])).toEqual(["7","2","0"]);
},15_000);
